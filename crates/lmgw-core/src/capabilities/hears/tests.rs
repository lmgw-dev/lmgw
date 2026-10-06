//! The predicate, question by question (module doc): the egress, the
//! published capabilities, the server's own word — and nothing about where
//! the model runs.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::config::{
    HoldFallbackMode, LlamaParams, LocalModel, ModelAlias, Snapshot, Upstream, UpstreamKind,
};
use crate::state::AppState;

const CLOUD: i64 = 10;
const CLAUDE: i64 = 11;
const GEMINI: i64 = 12;
const LAN: i64 = 13;

fn upstream(id: i64, protocol: Protocol, kind: UpstreamKind) -> Upstream {
    Upstream {
        id,
        name: format!("up{id}"),
        protocol,
        kind,
        // A closed port: a catalog probe is refused at once.
        base_url: "http://127.0.0.1:9/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 1_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// An alias on `upstream_id` whose override says `caps` (`None`: no
/// override, so the closed catalog leaves the modalities unknown).
fn alias(name: &str, upstream_id: i64, caps: Option<serde_json::Value>) -> (String, ModelAlias) {
    (
        name.to_string(),
        ModelAlias {
            id: 1,
            alias: name.into(),
            upstream_id,
            upstream_model_id: format!("{name}-model"),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: caps.map(|c| json!({ "capabilities": c })),
        },
    )
}

fn hears_audio() -> Option<serde_json::Value> {
    Some(json!({ "task": "chat", "input_modalities": ["text", "audio"] }))
}

/// Cloud aliases that hear, read text only, do not say, and transcribe; an
/// Anthropic and a Gemini alias whose overrides claim audio; a llama-server
/// on the network that does not say; and a local row that is not public.
async fn gateway() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut snap: Snapshot = (*state.snapshot()).clone();
    snap.upstreams = HashMap::from([
        (
            CLOUD,
            upstream(CLOUD, Protocol::Openai, UpstreamKind::Generic),
        ),
        (
            CLAUDE,
            upstream(CLAUDE, Protocol::Anthropic, UpstreamKind::Generic),
        ),
        (
            GEMINI,
            upstream(GEMINI, Protocol::Gemini, UpstreamKind::Generic),
        ),
        (
            LAN,
            upstream(LAN, Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        ),
    ]);
    snap.aliases = HashMap::from([
        alias("openai/gpt", CLOUD, hears_audio()),
        alias(
            "openai/texty",
            CLOUD,
            Some(json!({ "task": "chat", "input_modalities": ["text"] })),
        ),
        alias("openai/unsaid", CLOUD, None),
        alias(
            "openai/whisper",
            CLOUD,
            Some(json!({ "task": "asr", "input_modalities": ["audio"] })),
        ),
        alias("claude/x", CLAUDE, hears_audio()),
        alias("gemini/x", GEMINI, hears_audio()),
        alias("lan-model", LAN, None),
    ]);
    snap.local_models = vec![LocalModel {
        id: 1,
        model_id: "hidden".into(),
        gguf_path: "hidden.gguf".into(),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: false,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::default(),
        hold_fallback: None,
        capabilities_override: Some(json!({ "capabilities": hears_audio() })),
        ladder: vec![],
    }];
    state.set_snapshot_for_tests(snap);
    state
}

async fn ask(state: &SharedState, name: &str, facts: Option<&LlamaFacts>) -> Hears {
    let route = state.snapshot().resolve(name).unwrap();
    hears(state, Model::Named(name), &route, facts).await
}

fn facts(audio: Option<bool>) -> LlamaFacts {
    LlamaFacts {
        audio,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_chat_model_that_takes_audio_hears_wherever_it_runs() {
    let state = gateway().await;
    assert_eq!(ask(&state, "openai/gpt", None).await, Hears::Yes);
    assert_eq!(ask(&state, "gemini/x", None).await, Hears::Yes);
    assert_eq!(
        ask(&state, "openai/texty", None).await,
        Hears::No(NotHeard::NoAudioInput)
    );
    assert_eq!(
        ask(&state, "openai/whisper", None).await,
        Hears::No(NotHeard::NotChat),
        "a speech-to-text model takes audio and is no chat model"
    );
}

#[tokio::test]
async fn unknown_is_no_and_says_so() {
    let state = gateway().await;
    let h = ask(&state, "openai/unsaid", None).await;
    assert_eq!(h, Hears::Unknown);
    assert!(!h.yes());
    assert_eq!(
        h.why("openai/unsaid").as_deref(),
        Some("lmgw cannot tell whether openai/unsaid takes audio")
    );
    assert_eq!(
        h.goes_to("openai/unsaid").as_deref(),
        Some("this goes to openai/unsaid, and lmgw cannot tell whether it takes audio")
    );
    assert_eq!(Hears::Yes.why("x"), None);
}

/// Anthropic's API has no audio part: an override that claims audio does
/// not change what the egress can send.
#[tokio::test]
async fn the_anthropic_egress_has_no_audio_part() {
    let state = gateway().await;
    let h = ask(&state, "claude/x", None).await;
    assert_eq!(h, Hears::No(NotHeard::NoAudioPart));
    assert_eq!(
        h.why("claude/x").as_deref(),
        Some("claude/x is served over the Anthropic API, which has no audio input part")
    );
}

/// A llama-server's `/props`: no audio projector vetoes, one loaded lifts
/// an unknown, not known leaves the published answer — and a published
/// "no" stands.
#[tokio::test]
async fn the_servers_own_word_vetoes_or_lifts_an_unknown() {
    let state = gateway().await;
    assert_eq!(ask(&state, "lan-model", None).await, Hears::Unknown);
    assert_eq!(
        ask(&state, "lan-model", Some(&facts(Some(true)))).await,
        Hears::Yes
    );
    assert_eq!(
        ask(&state, "lan-model", Some(&facts(None))).await,
        Hears::Unknown
    );
    let vetoed = ask(&state, "openai/gpt", Some(&facts(Some(false)))).await;
    assert_eq!(vetoed, Hears::No(NotHeard::NoProjector));
    assert_eq!(
        vetoed.why("gemma").as_deref(),
        Some("gemma's server loaded no audio projector")
    );
    assert_eq!(
        ask(&state, "openai/gpt", Some(&facts(Some(true)))).await,
        Hears::Yes
    );
    assert_eq!(
        ask(&state, "openai/texty", Some(&facts(Some(true)))).await,
        Hears::No(NotHeard::NoAudioInput),
        "the owner's text-only override stands"
    );
}

/// A candidate a walk picked that is not public: read by its row, from the
/// route.
#[tokio::test]
async fn a_row_that_is_not_public_is_read_by_its_route() {
    let state = gateway().await;
    let route = state.snapshot().chat_local_route("hidden");
    assert_eq!(
        hears(&state, Model::Named("hidden"), &route, None).await,
        Hears::Yes
    );
    // Nothing up: a managed row's server says nothing yet.
    assert_eq!(server_facts(&state, None, &route), None);
}

#[test]
fn the_reasons_read_after_a_swaps_lead() {
    for (h, want) in [
        (
            Hears::No(NotHeard::NoAudioInput),
            "this goes to m, which does not take audio input",
        ),
        (
            Hears::No(NotHeard::NotChat),
            "this goes to m, which is not a chat model",
        ),
        (
            Hears::No(NotHeard::NoAudioPart),
            "this goes to m, served over the Anthropic API, which has no \
             audio input part",
        ),
        (
            Hears::No(NotHeard::NoProjector),
            "this goes to m, whose server loaded no audio projector",
        ),
    ] {
        assert_eq!(h.goes_to("m").as_deref(), Some(want));
    }
    assert_eq!(Hears::Yes.goes_to("m"), None);
}

/// Review V8: a candidate's pick is read from its row, through the route,
/// even when an alias of the same name says something else.
#[tokio::test]
async fn a_pick_is_read_from_its_row_not_a_same_named_alias() {
    let state = gateway().await;
    let mut snap: Snapshot = (*state.snapshot()).clone();
    snap.local_models[0].public = true;
    snap.local_models[0].capabilities_override = Some(json!({
        "capabilities": { "task": "chat", "input_modalities": ["text"] }
    }));
    snap.aliases
        .insert("hidden".into(), alias("hidden", CLOUD, hears_audio()).1);
    state.set_snapshot_for_tests(snap);
    let route = state.snapshot().chat_local_route("hidden");
    assert_eq!(
        hears(&state, Model::Named("hidden"), &route, None).await,
        Hears::Yes,
        "by name, the alias shadows the row"
    );
    assert_eq!(
        hears(&state, Model::Pick("hidden"), &route, None).await,
        Hears::No(NotHeard::NoAudioInput),
        "the pick is its row"
    );
}

/// The pure half the attachments ask: the egress, then what was read.
#[test]
fn hears_from_capabilities_already_read() {
    let caps: ModelCapabilities = serde_json::from_value(json!({
        "task": "chat", "endpoints": [], "input_modalities": ["text", "audio"],
        "source": "owner"
    }))
    .unwrap();
    let mut route = Route {
        upstream: upstream(CLOUD, Protocol::Openai, UpstreamKind::Generic),
        upstream_model: "m".into(),
        param_defaults: Default::default(),
        fallback: Default::default(),
    };
    assert_eq!(hears_from(Some(&caps), &route, None), Hears::Yes);
    assert_eq!(hears_from(None, &route, None), Hears::Unknown);
    assert_eq!(
        hears_from(Some(&caps), &route, Some(&facts(Some(false)))),
        Hears::No(NotHeard::NoProjector)
    );
    route.upstream.protocol = Protocol::Anthropic;
    assert_eq!(
        hears_from(Some(&caps), &route, None),
        Hears::No(NotHeard::NoAudioPart)
    );
}
