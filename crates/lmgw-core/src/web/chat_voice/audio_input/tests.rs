//! §2.2's table, row by row (voice-audio-input design §7): each row's
//! failure gives the transcript and names why; a model that takes audio
//! hears wherever it runs (changed 2026-10-06: capability, not locality);
//! the hold and a benchmark's lease judge a model (a candidate alias too)
//! by its fallback alone; a block's verdict is forecast beside; and a
//! guarded row never hears.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::config::{
    CandidateAlias, HoldFallbackMode, LlamaParams, LocalModel, ModelAlias, Protocol, Upstream,
    UpstreamKind,
};
use crate::state::AppState;

const CLOUD: i64 = 10;
const LAN: i64 = 11;
const CLAUDE: i64 = 12;

fn local(model_id: &str, modalities: Option<&[&str]>) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::default(),
        hold_fallback: None,
        capabilities_override: modalities
            .map(|m| json!({ "capabilities": { "task": "chat", "input_modalities": m } })),
        ladder: vec![],
    }
}

fn upstream(id: i64, name: &str, kind: UpstreamKind) -> Upstream {
    Upstream {
        id,
        name: name.into(),
        // A llama-server speaks llama_cpp (llama.cpp egress design §5).
        protocol: if kind == UpstreamKind::LlamaServer {
            Protocol::LlamaCpp
        } else {
            Protocol::Openai
        },
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

fn alias(name: &str, upstream_id: i64) -> (String, ModelAlias) {
    (
        name.to_string(),
        ModelAlias {
            id: 1,
            alias: name.into(),
            upstream_id,
            upstream_model_id: format!("{name}-model"),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({
                "capabilities": { "task": "chat", "input_modalities": ["text", "audio"] }
            })),
        },
    )
}

fn candidate(name: &str, candidates: &[&str], fallback: Option<&str>) -> (String, CandidateAlias) {
    (
        name.to_string(),
        CandidateAlias {
            id: 1,
            alias: name.into(),
            candidates: candidates.iter().map(|c| c.to_string()).collect(),
            background: false,
            fallback_mode: if fallback.is_some() {
                HoldFallbackMode::Alias
            } else {
                HoldFallbackMode::None
            },
            fallback: fallback.map(String::from),
            capabilities_disabled: vec![],
            capabilities_enabled: vec!["audio".into()],
            enabled: true,
            notes: String::new(),
        },
    )
}

/// A gateway whose rows are: `hears` and `hears-too` (local models that
/// take audio), `texty` (one that does not), `unsure` (a projector lmgw
/// cannot read), `laddered` and `pooled` (context guards), `pooled-off` (a
/// disabled guard), `gone-off` (a disabled row that would hear), a cloud
/// alias `openai/gpt`, a llama-server elsewhere `lan-model` (both take audio
/// by their overrides), an Anthropic alias `claude/x` whose override claims
/// audio, and the candidate aliases — all enabling audio, as the
/// save rules require of their candidates: `pick` (`hears`, `hears-too`;
/// fallback `openai/gpt`), `pick-guarded` (`hears`, `pooled`),
/// `pick-guard-off` (`hears`, `pooled-off`), `pick-lost` (`gone-off`, then
/// `hears`; fallback `openai/gpt`), `pick-bg` (background, `hears`; fallback
/// `openai/gpt`), `pick-bg-alone` (background, no fallback) and
/// `pick-guarded-fb` (`hears`, `pooled`; fallback `openai/gpt`). The hold's
/// chat fallback is `openai/gpt`; speech-to-text is `openai/whisper` (task
/// `asr`); the setting is `on` unless `f` says otherwise.
async fn gateway(f: impl FnOnce(&mut crate::config::Snapshot)) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut snap = (*state.snapshot()).clone();
    snap.settings.chat_voice_audio_input = "on".into();
    snap.settings.hold.fallback_alias = Some("openai/gpt".into());
    snap.settings.chat_stt_alias = "openai/whisper".into();
    let mut unsure = local("unsure", None);
    unsure.params.mmproj_path = Some("missing-mmproj.gguf".into());
    let mut laddered = local("laddered", Some(&["text", "audio"]));
    laddered.params.ctx_size = Some(8192);
    laddered.ladder = vec![crate::ladder::Rung {
        gguf_path: "laddered.gguf".into(),
        ctx_size: 32768,
    }];
    let mut pooled = local("pooled", Some(&["text", "audio"]));
    pooled.params.kv_unified = Some(true);
    pooled.params.parallel = Some(2);
    pooled.params.n_predict = Some(512);
    let mut pooled_off = pooled.clone();
    pooled_off.model_id = "pooled-off".into();
    pooled_off.enabled = false;
    let mut gone_off = local("gone-off", Some(&["text", "audio"]));
    gone_off.enabled = false;
    snap.local_models = vec![
        local("hears", Some(&["text", "audio"])),
        local("hears-too", Some(&["text", "audio"])),
        local("texty", Some(&["text"])),
        unsure,
        laddered,
        pooled,
        pooled_off,
        gone_off,
    ];
    let mut claude = upstream(CLAUDE, "claude", UpstreamKind::Generic);
    claude.protocol = Protocol::Anthropic;
    snap.upstreams = HashMap::from([
        (CLOUD, upstream(CLOUD, "cloud", UpstreamKind::Generic)),
        (LAN, upstream(LAN, "lan", UpstreamKind::LlamaServer)),
        (CLAUDE, claude),
    ]);
    let mut whisper = alias("openai/whisper", CLOUD);
    whisper.1.capabilities_override = Some(json!({
        "capabilities": { "task": "asr", "input_modalities": ["audio"] }
    }));
    snap.aliases = HashMap::from([
        alias("openai/gpt", CLOUD),
        whisper,
        alias("lan-model", LAN),
        alias("claude/x", CLAUDE),
    ]);
    let background = |(name, mut ca): (String, CandidateAlias)| {
        ca.background = true;
        (name, ca)
    };
    snap.candidate_aliases = HashMap::from([
        candidate("pick", &["hears", "hears-too"], Some("openai/gpt")),
        candidate("pick-guarded", &["hears", "pooled"], None),
        candidate("pick-guard-off", &["hears", "pooled-off"], None),
        candidate("pick-lost", &["gone-off", "hears"], Some("openai/gpt")),
        background(candidate("pick-bg", &["hears"], Some("openai/gpt"))),
        background(candidate("pick-bg-alone", &["hears"], None)),
        candidate("pick-guarded-fb", &["hears", "pooled"], Some("openai/gpt")),
        candidate("pick-claude", &["hears"], Some("claude/x")),
    ]);
    f(&mut snap);
    state.set_snapshot_for_tests(snap);
    state
}

fn thread(model: &str) -> ChatThread {
    ChatThread {
        model_alias: model.into(),
        ..Default::default()
    }
}

async fn why(state: &SharedState, t: &ChatThread) -> String {
    let v = verdict(state, t).await;
    assert_eq!(v.path, InputPath::Transcript, "{v:?}");
    v.why.unwrap()
}

/// `openai/gpt` reads text only.
fn text_only(s: &mut crate::config::Snapshot) {
    s.aliases
        .get_mut("openai/gpt")
        .unwrap()
        .capabilities_override = Some(json!({
        "capabilities": { "task": "chat", "input_modalities": ["text"] }
    }));
}

/// A verdict that hears `model`, reached as `via`, after `lead`.
fn heard(model: &str, via: &[&str], lead: Option<&'static str>) -> AudioInput {
    AudioInput {
        path: InputPath::Audio,
        model: model.into(),
        why: None,
        lead,
        blocked: None,
        via: via.iter().map(|m| m.to_string()).collect(),
        lacks: false,
    }
}

#[tokio::test]
async fn a_local_model_that_takes_audio_hears_the_turn() {
    let state = gateway(|_| {}).await;
    let v = verdict(&state, &thread("hears")).await;
    assert_eq!(
        v,
        AudioInput {
            blocked: Some(Box::new(heard("openai/gpt", &["openai/gpt"], None))),
            ..heard("hears", &["hears"], None)
        }
    );
    let shown = serde_json::to_value(shown(&state, &thread("hears")).await).unwrap();
    assert_eq!(
        shown,
        json!({ "value": "on", "source": "chat", "path": "audio", "model": "hears",
                "why": null,
                "blocked": { "path": "audio", "model": "openai/gpt", "why": null } })
    );
}

#[tokio::test]
async fn off_at_either_level_is_the_first_row_and_names_the_level() {
    let state = gateway(|s| s.settings.chat_voice_audio_input = "off".into()).await;
    let mut t = thread("hears");
    assert_eq!(
        why(&state, &t).await,
        "audio input is off (Settings → Chat → Voice)"
    );
    t.voice.audio_input = Some(AudioInputMode::On);
    assert_eq!(verdict(&state, &t).await.path, InputPath::Audio);
    assert_eq!(
        setting(&state.snapshot(), &t),
        (AudioInputMode::On, Source::Thread)
    );

    let state = gateway(|_| {}).await;
    t.voice.audio_input = Some(AudioInputMode::Off);
    assert_eq!(why(&state, &t).await, "audio input is off (this thread)");
    // A stored value this build cannot read is off, as the load makes it.
    let state = gateway(|s| s.settings.chat_voice_audio_input = "always".into()).await;
    assert_eq!(
        setting(&state.snapshot(), &thread("hears")),
        (AudioInputMode::Off, Source::Chat)
    );
}

#[tokio::test]
async fn auto_mode_knowledge_bases_take_the_transcript_tool_mode_does_not() {
    let state = gateway(|_| {}).await;
    let mut t = thread("hears");
    t.kb_ids = vec![3];
    t.kb_mode = KbMode::Auto;
    assert_eq!(
        why(&state, &t).await,
        "its knowledge bases search with your words (auto mode)"
    );
    t.kb_mode = KbMode::Tool;
    assert_eq!(verdict(&state, &t).await.path, InputPath::Audio);
    // Auto mode without a base searches nothing.
    t.kb_ids.clear();
    t.kb_mode = KbMode::Auto;
    assert_eq!(verdict(&state, &t).await.path, InputPath::Audio);
}

/// WP3 review #2: only a transcript can veto noise, so a thread with no
/// speech recognition that resolves reads the transcript, whatever its
/// model could hear.
#[tokio::test]
async fn no_speech_recognition_that_resolves_gives_the_transcript() {
    let state = gateway(|s| s.settings.chat_stt_alias = String::new()).await;
    assert_eq!(
        why(&state, &thread("hears")).await,
        "no speech recognition is set up, and only a transcript tells your words from noise"
    );
    let state = gateway(|_| {}).await;
    let mut t = thread("hears");
    t.voice.asr_alias = Some("gone-asr".into());
    let w = why(&state, &t).await;
    assert!(
        w.starts_with("its speech recognition gone-asr does not resolve (unknown model alias"),
        "{w}"
    );
    // The thread's own alias that resolves hears again.
    t.voice.asr_alias = Some("openai/whisper".into());
    assert_eq!(verdict(&state, &t).await.path, InputPath::Audio);
}

#[tokio::test]
async fn a_route_that_does_not_settle_gives_its_own_error() {
    let state = gateway(|_| {}).await;
    let v = verdict(&state, &thread("gone")).await;
    assert_eq!((v.path, v.model.as_str()), (InputPath::Transcript, "gone"));
    assert!(v.why.unwrap().contains("unknown model alias: gone"));
    // The hold with no fallback at all: the hold's own refusal.
    let state = gateway(|s| {
        s.settings.hold.active = true;
        s.settings.hold.fallback_alias = None;
    })
    .await;
    let w = why(&state, &thread("hears")).await;
    assert!(w.contains("hold"), "{w}");
}

/// Capability, not locality (the owner's ruling, 2026-10-06): a cloud
/// model and a llama-server elsewhere that take audio hear the turn.
#[tokio::test]
async fn a_model_that_takes_audio_hears_wherever_it_runs() {
    let state = gateway(|_| {}).await;
    assert_eq!(
        verdict(&state, &thread("openai/gpt")).await,
        heard("openai/gpt", &["openai/gpt"], None),
        "no block touches a cloud model: nothing forecast"
    );
    assert_eq!(
        verdict(&state, &thread("lan-model")).await,
        heard("lan-model", &["lan-model"], None)
    );
    assert_eq!(
        why(&state, &thread("openai/whisper")).await,
        "openai/whisper is not a chat model"
    );
    // N2: an override cannot give the Anthropic API an audio part.
    assert_eq!(
        why(&state, &thread("claude/x")).await,
        "claude/x is served over the Anthropic API, which has no audio input part"
    );
}

#[tokio::test]
async fn under_the_hold_the_fallback_alone_is_judged_a_candidate_aliases_too() {
    let state = gateway(|s| s.settings.hold.active = true).await;
    let hold = Some("under the GPU hold");
    assert_eq!(
        verdict(&state, &thread("hears")).await,
        heard("openai/gpt", &["openai/gpt"], hold),
        "the model that answers hears: no forecast under a live block"
    );
    // A model that does not hear itself hands the turn to one that does.
    assert_eq!(
        verdict(&state, &thread("texty")).await,
        heard("openai/gpt", &["openai/gpt"], hold)
    );
    // N3: the candidate alias hands the turn to its own fallback, judged
    // alone — no candidate's guard, no candidate in `via`.
    assert_eq!(
        verdict(&state, &thread("pick")).await,
        heard("openai/gpt", &["openai/gpt"], hold)
    );
    assert_eq!(
        verdict(&state, &thread("pick-guarded-fb")).await,
        heard("openai/gpt", &["openai/gpt"], hold)
    );
    // An Anthropic fallback has no audio part whatever its override says.
    // Changed 2026-10-06 (the owner's ruling: a configured fallback is
    // always used): for an alias that enables audio it no longer counts as
    // none — the hold hands the turn to it, as its transcript.
    assert_eq!(
        why(&state, &thread("pick-claude")).await,
        "under the GPU hold this goes to claude/x, served over the Anthropic API, which has no \
         audio input part"
    );
    // A fallback that reads text only: the transcript, named after the swap.
    let state = gateway(|s| {
        s.settings.hold.active = true;
        text_only(s);
    })
    .await;
    let v = verdict(&state, &thread("hears")).await;
    assert_eq!((v.model.as_str(), v.lead), ("openai/gpt", hold));
    assert_eq!(
        v.why.as_deref(),
        Some("under the GPU hold this goes to openai/gpt, which does not take audio input")
    );
    // Without the hold the candidate alias hears: every candidate takes
    // audio and none guards its context.
    let state = gateway(|_| {}).await;
    let v = verdict(&state, &thread("pick")).await;
    assert_eq!(
        (v.path, v.model.as_str()),
        (InputPath::Audio, "pick"),
        "{v:?}"
    );
}

#[tokio::test]
async fn a_benchmark_run_and_a_fallback_on_the_network_are_judged_alike() {
    // A benchmark run's lease swaps a local model as the hold does.
    let state = gateway(text_only).await;
    state.set_gpu_lease(Some(std::sync::Arc::new(crate::bench::lease::GpuLease {
        run_id: 7,
        model_id: "texty".into(),
    })));
    assert_eq!(
        why(&state, &thread("hears")).await,
        "while a benchmark run holds the GPU this goes to openai/gpt, which does not take audio \
         input"
    );
    // A llama-server elsewhere as the hold's fallback hears like any other.
    let state = gateway(|s| {
        s.settings.hold.active = true;
        s.settings.hold.fallback_alias = Some("lan-model".into());
    })
    .await;
    assert_eq!(
        verdict(&state, &thread("hears")).await,
        heard("lan-model", &["lan-model"], Some("under the GPU hold"))
    );
}

/// A candidate alias whose walk may end at its fallback by design (WP1
/// review M1, superseded 2026-10-06): the verdict judges the candidates the
/// walk picks from, which hear.
#[tokio::test]
async fn a_candidate_aliases_walk_to_its_fallback_hears_when_the_fallback_does() {
    let state = gateway(|_| {}).await;
    for name in ["pick-bg", "pick-lost", "pick-bg-alone"] {
        let v = verdict(&state, &thread(name)).await;
        assert_eq!(
            (v.path, v.model.as_str()),
            (InputPath::Audio, name),
            "{v:?}"
        );
    }
    // A fallback that does not take audio leaves that verdict alone; a turn
    // the walk does hand to it goes as its transcript (changed 2026-10-06:
    // a fallback lacking an enabled facet is used).
    let state = gateway(text_only).await;
    assert_eq!(
        verdict(&state, &thread("pick-bg")).await.path,
        InputPath::Audio
    );
}

/// N4 (decision D3): with no block live, the verdict a block would bring —
/// the fallback's own, or the refusal — and none for a model no block
/// touches.
#[tokio::test]
async fn the_verdict_forecasts_what_a_block_would_bring() {
    let state = gateway(|_| {}).await;
    let forecast = |v: AudioInput| v.blocked.map(|b| *b);
    // A model that reads the transcript itself, whose fallback hears.
    let v = verdict(&state, &thread("texty")).await;
    assert_eq!(v.why.as_deref(), Some("texty does not take audio input"));
    assert_eq!(
        forecast(v),
        Some(heard("openai/gpt", &["openai/gpt"], None))
    );
    assert_eq!(
        forecast(verdict(&state, &thread("pick")).await),
        Some(heard("openai/gpt", &["openai/gpt"], None))
    );
    assert_eq!(forecast(verdict(&state, &thread("openai/gpt")).await), None);
    let no_fallback = forecast(verdict(&state, &thread("pick-bg-alone")).await).unwrap();
    assert_eq!(
        no_fallback.why.as_deref(),
        Some("pick-bg-alone has no fallback, so the turn is refused")
    );
    // A fallback that reads text only: said as what follows the page's
    // words for the block.
    let state = gateway(text_only).await;
    let b = forecast(verdict(&state, &thread("hears")).await).unwrap();
    assert_eq!((b.path, b.lead), (InputPath::Transcript, None));
    assert_eq!(
        b.why.as_deref(),
        Some("this goes to openai/gpt, which does not take audio input")
    );
    // With no hold fallback at all, and one that does not resolve.
    let state = gateway(|s| s.settings.hold.fallback_alias = None).await;
    assert_eq!(
        forecast(verdict(&state, &thread("hears")).await)
            .unwrap()
            .why
            .as_deref(),
        Some("hears has no fallback, so the turn is refused")
    );
    let state = gateway(|s| s.settings.hold.fallback_alias = Some("gone".into())).await;
    assert_eq!(
        forecast(verdict(&state, &thread("hears")).await)
            .unwrap()
            .why
            .as_deref(),
        Some("hears's fallback gone does not resolve, so the turn is refused")
    );
    // The thread's own rows decide first: nothing to forecast.
    let state = gateway(|s| s.settings.chat_voice_audio_input = "off".into()).await;
    assert_eq!(forecast(verdict(&state, &thread("hears")).await), None);
}

#[tokio::test]
async fn a_context_guard_never_hears_for_a_candidate_alias_any_candidate() {
    let state = gateway(|_| {}).await;
    assert_eq!(
        why(&state, &thread("laddered")).await,
        "laddered guards its context, which cannot bound audio"
    );
    assert_eq!(
        why(&state, &thread("pooled")).await,
        "pooled guards its context, which cannot bound audio"
    );
    assert_eq!(
        why(&state, &thread("pick-guarded")).await,
        "pooled guards its context, which cannot bound audio"
    );
    // A disabled guarded row is no candidate the walk may pick.
    assert_eq!(
        verdict(&state, &thread("pick-guard-off")).await.path,
        InputPath::Audio
    );
    // Named as a turn names it: under the router's public prefix.
    let state = gateway(|s| s.settings.router.public_prefix = "local".into()).await;
    let v = verdict(&state, &thread("local/pooled")).await;
    assert_eq!(
        v.why.as_deref(),
        Some("local/pooled guards its context, which cannot bound audio"),
        "{v:?}"
    );
}

#[tokio::test]
async fn the_capability_is_audio_input_and_unknown_is_no() {
    let state = gateway(|_| {}).await;
    assert_eq!(
        why(&state, &thread("texty")).await,
        "texty does not take audio input"
    );
    assert_eq!(
        why(&state, &thread("unsure")).await,
        "lmgw cannot tell whether unsure takes audio: if it does, list audio in the input \
         modalities of its row's capabilities override"
    );
    // Worded by where the model lives (review V5): an alias, and a
    // passthrough model, which has no override of its own.
    let state = gateway(|s| {
        s.aliases
            .get_mut("openai/gpt")
            .unwrap()
            .capabilities_override = None;
    })
    .await;
    // The catalog read first: the view reads it from the cache alone.
    let cloud = state.snapshot().upstreams[&CLOUD].clone();
    let _ = crate::catalog::upstream_models(&state, &cloud).await;
    assert!(why(&state, &thread("openai/gpt")).await.ends_with(
        "if it does, give its alias a capabilities override with task chat and input \
                    modalities text and audio"
    ));
    let hint = with_hint(
        &state.snapshot(),
        "cloud/omni",
        Hears::Unknown,
        String::new(),
    );
    assert!(
        hint.contains("a passthrough model has no override of its own"),
        "{hint}"
    );
}

#[test]
fn a_server_that_refused_this_session_gets_the_transcript_from_then_on() {
    let hears = heard("hears", &["hears"], None);
    let mut refused = Refusals::new();
    assert_eq!(hears.clone().after_refusal(&refused), hears);
    refused.insert(
        "other".into(),
        "it refused the audio this session: x".into(),
    );
    assert_eq!(
        hears.clone().after_refusal(&refused),
        hears,
        "another model's refusal"
    );
    refused.insert(
        "hears".into(),
        "it refused the audio this session: audio input is not supported".into(),
    );
    let after = hears.clone().after_refusal(&refused);
    assert_eq!(after.path, InputPath::Transcript);
    assert_eq!(after.model, "hears");
    assert_eq!(
        after.why.as_deref(),
        Some("it refused the audio this session: audio input is not supported")
    );
    // A candidate alias: once every model it may reach refused.
    let pick = AudioInput {
        model: "pick".into(),
        via: vec!["hears".into(), "hears-too".into()],
        ..hears
    };
    assert_eq!(pick.clone().after_refusal(&refused).path, InputPath::Audio);
    refused.insert(
        "hears-too".into(),
        "its server failed on the audio this session: reset".into(),
    );
    assert_eq!(
        pick.clone().after_refusal(&refused).path,
        InputPath::Transcript
    );
    // A transcript verdict keeps its own why.
    let off = AudioInput::transcript("hears", "audio input is off (this thread)".into());
    assert_eq!(off.clone().after_refusal(&refused), off);
    // A block's forecast is held to the same memory, whatever the verdict.
    let texty = AudioInput {
        blocked: Some(Box::new(heard("hears-too", &["hears-too"], None))),
        ..AudioInput::transcript("texty", "texty does not take audio input".into())
    };
    let after = texty.after_refusal(&refused);
    assert_eq!(
        after.why.as_deref(),
        Some("texty does not take audio input")
    );
    let b = after.blocked.unwrap();
    assert_eq!(
        (b.path, b.model.as_str()),
        (InputPath::Transcript, "hears-too")
    );
    assert_eq!(
        b.why.as_deref(),
        Some("its server failed on the audio this session: reset")
    );
}

/// Who an audio turn may reach, as the session's memory names them: the
/// model, or a candidate alias's routable candidates by public name.
#[tokio::test]
async fn the_verdict_names_who_an_audio_turn_may_reach() {
    let state = gateway(|s| s.settings.router.public_prefix = "local".into()).await;
    let v = verdict(&state, &thread("local/hears")).await;
    assert_eq!(v.via, ["local/hears"]);
    let v = verdict(&state, &thread("pick")).await;
    assert_eq!(v.path, InputPath::Audio, "{v:?}");
    assert_eq!(v.via, ["local/hears", "local/hears-too"]);
}
