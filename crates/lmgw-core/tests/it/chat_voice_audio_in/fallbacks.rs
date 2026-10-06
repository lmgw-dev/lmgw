//! Voice turns the model hears, through a fallback (voice-audio-input
//! design §3.4, §3.5; changed 2026-10-06: capability, not locality). A
//! configured fallback is always used. It hears the turn when it takes
//! audio, wherever it runs, and reads the transcript otherwise:
//!
//! - **under the GPU hold**, the thread JSON names the fallback and whether
//!   it hears, and a bound session's turn goes to a fallback that hears as
//!   audio: the row says it was heard, the reply follows the row; one that
//!   reads text only gets the transcript, and with no block live the page
//!   is told what a block would bring;
//! - **at admission** (§4.7's outside-VRAM swap of a plain row), which the
//!   verdict cannot foresee, a fallback that reads text only is refused the
//!   audio before anything is sent, and a bound session's turn goes again as
//!   its transcript, with the capability's why;
//! - **a fallback's own refusal** of the audio goes again as the transcript
//!   and is kept for the fallback alone.
//!
//! The fallback is a wiremock speaking OpenAI's SSE ([`Cloud`]): nothing
//! here calls a cloud.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use lmgw_core::config::{Protocol, Settings, UpstreamKind};
use lmgw_core::realtime::heard_response_for_tests;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use lmgw_core::web::spoken_turn_for_tests;

use super::session::{hearing, quiet, rows, session};
use super::turns::{audio, hears, row_written, thread, world};
use crate::realtime_chat_thread::{of_type, say, until_type, World};
use crate::support::gpu_world::{Gpu, ANSWER, GIB};
use crate::support::realtime_audio::Asr;

/// What the fallback answers with.
pub(super) const CLOUD_SAYS: &str = "Aus der Wolke.";

/// The fallback's answer to each chat call: a refusal of a request that
/// carries audio when `refuse_audio`, else [`CLOUD_SAYS`], streamed.
struct Answers {
    refuse_audio: bool,
}

impl Respond for Answers {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if self.refuse_audio && String::from_utf8_lossy(&req.body).contains("input_audio") {
            return ResponseTemplate::new(400).set_body_json(json!({"error": {
                "message": "audio input is not supported by this model",
                "type": "invalid_request_error",
            }}));
        }
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": CLOUD_SAYS}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        );
        ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
    }
}

/// The alias `cloud` on an OpenAI-compatible upstream `cloud-up` of its
/// own, whose capabilities are the owner's `caps`.
pub(super) struct Cloud {
    mock: MockServer,
}

impl Cloud {
    pub(super) async fn new(g: &Gpu, caps: Value, refuse_audio: bool) -> Self {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(Answers { refuse_audio })
            .mount(&mock)
            .await;
        let up = store::insert_upstream(
            &g.state.db,
            &NewUpstream {
                name: "cloud-up".into(),
                protocol: Protocol::Openai,
                kind: UpstreamKind::Generic,
                base_url: format!("{}/v1", mock.uri()),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
            },
        )
        .await
        .unwrap();
        store::insert_alias(
            &g.state.db,
            &NewAlias {
                alias: "cloud".into(),
                upstream_id: up,
                upstream_model_id: "gpt".into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: Some(caps),
            },
        )
        .await
        .unwrap();
        g.state.reload_snapshot().await.unwrap();
        Self { mock }
    }

    /// The chat bodies it got, in order.
    pub(super) async fn bodies(&self) -> Vec<Value> {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/v1/chat/completions")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }
}

/// Capabilities that read text only.
pub(super) fn text_only() -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text"],
        "source": "owner",
    }})
}

/// The `input_audio` parts of `body`.
pub(super) fn audio_parts(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|p| p["type"] == "input_audio")
        .count()
}

/// Settings changed by `f`, saved and live.
async fn settle(g: &Gpu, f: impl FnOnce(&mut Settings)) {
    let mut s = g.state.snapshot().settings.clone();
    f(&mut s);
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// `cloud` is every chat row's fallback; `hold` switches the GPU hold.
pub(super) async fn falls_back_to_cloud(g: &Gpu, hold: bool) {
    settle(g, |s| {
        s.hold.fallback_alias = Some("cloud".into());
        s.hold.active = hold;
    })
    .await;
}

/// A game holds the card: admission's outside-VRAM verdict hands a local
/// row's turn to its fallback, which the verdict does not foresee.
fn game_holds_the_card(g: &Gpu) {
    let mut w = g.world();
    w.attribution = true;
    w.outside = 20 * GIB;
}

/// The thread JSON's audio-input verdict for thread `tid`.
/// The newest Chat request row's marker: what its content lost to a model
/// that lacks a capability (`request_logs.degraded`).
async fn chat_row_marker(g: &Gpu) -> Option<String> {
    let rows = store::query_logs(
        &g.state.db,
        &store::LogFilter {
            limit: 20,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    rows.into_iter()
        .find(|r| r.ingress_proto == "chat")
        .expect("a chat row")
        .degraded
}

async fn verdict(w: &World, tid: i64) -> Value {
    let t = w.get(&format!("/chat/api/threads/{tid}")).await;
    t["thread"]["voice_resolved"]["audio_input"].clone()
}

/// F2: under the hold the page names the fallback that hears, and a bound
/// session's turn goes to it as audio — the row says it was heard, the
/// reply follows the row, and nothing runs locally.
#[tokio::test]
async fn under_the_hold_a_fallback_that_hears_gets_the_turn_as_audio() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let cloud = Cloud::new(&g, hears(), false).await;
    falls_back_to_cloud(&g, false).await;
    let (tid, mut ws) = session(&w).await;
    // No block live: the verdict is the model's own, and what a block
    // would bring is told beside it (decision D3).
    let v = verdict(&w, tid).await;
    assert_eq!(
        (v["path"].as_str(), v["model"].as_str()),
        (Some("audio"), Some("gemma"))
    );
    assert_eq!(
        v["blocked"],
        json!({ "path": "audio", "model": "cloud", "why": null }),
        "{v}"
    );
    falls_back_to_cloud(&g, true).await;
    let v = verdict(&w, tid).await;
    assert_eq!(
        v,
        json!({ "value": "on", "source": "chat", "path": "audio", "model": "cloud",
                "why": null, "lead": "under the GPU hold" })
    );

    // The bound session judged its verdict at the bind, before the hold:
    // the swap at resolve hands the turn to the fallback, which hears it.
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let mut events = until_type(&mut ws, "lmgw.response.timing").await;
    events.extend(quiet(&mut ws).await);
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 1, "{events:?}");
    assert_eq!(input[0]["input"], "audio");
    let sent = cloud.bodies().await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(audio_parts(&sent[0]), 1, "the fallback heard it");
    assert!(g.world().streamed_bodies.is_empty(), "nothing ran locally");
    let r = rows(&w, tid).await;
    let shape: Vec<(&str, &str)> = r.iter().map(|x| (x.1.as_str(), x.2.as_str())).collect();
    assert_eq!(
        shape,
        [("user", "Wie spät ist es?"), ("assistant", CLOUD_SAYS)]
    );
    assert_eq!(r[0].3["input"], "audio", "{r:?}");
}

/// F2, the other half: a fallback that reads text only gets the
/// transcript under the hold, and the page says so before the turn.
#[tokio::test]
async fn under_the_hold_a_fallback_that_reads_text_only_gets_the_transcript() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let cloud = Cloud::new(&g, text_only(), false).await;
    falls_back_to_cloud(&g, false).await;
    let (tid, mut ws) = session(&w).await;
    let v = verdict(&w, tid).await;
    assert_eq!(
        v["blocked"],
        json!({ "path": "transcript", "model": "cloud",
                "why": "this goes to cloud, which does not take audio input" }),
        "{v}"
    );
    falls_back_to_cloud(&g, true).await;
    let v = verdict(&w, tid).await;
    assert_eq!(
        v["why"],
        "under the GPU hold this goes to cloud, which does not take audio input"
    );
    // The session's verdict followed the hold by the next response's
    // re-judgement at the latest; either way the cloud never gets audio.
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let mut events = until_type(&mut ws, "lmgw.response.timing").await;
    events.extend(quiet(&mut ws).await);
    let sent = cloud.bodies().await;
    assert_eq!(sent.len(), 1, "{events:?}");
    assert_eq!(audio_parts(&sent[0]), 0, "the transcript only: {}", sent[0]);
    // The turn's request row says its audio went as the transcript.
    assert_eq!(
        chat_row_marker(&g).await.as_deref(),
        Some("'cloud' lacks audio: transcript sent")
    );
    let r = rows(&w, tid).await;
    assert_eq!(
        r.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["Wie spät ist es?", CLOUD_SAYS]
    );
}

/// F1: admission swaps a plain row's turn to a fallback that reads text
/// only, after the verdict said "hears you": that route is refused the
/// audio before a byte leaves — the plain stream and the tool loop alike —
/// and nothing is started.
#[tokio::test]
async fn admissions_swap_to_a_fallback_that_reads_text_only_is_refused_the_audio() {
    for kind in ["chat", "admin"] {
        let g = world().await;
        let cloud = Cloud::new(&g, text_only(), false).await;
        falls_back_to_cloud(&g, false).await;
        game_holds_the_card(&g);
        let tid = thread(&g, "gemma", kind).await;
        let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
        let error = &frames
            .iter()
            .find(|(e, _)| e == "error")
            .unwrap_or_else(|| panic!("{kind}: refused: {frames:?}"))
            .1;
        assert_eq!(
            error["code"], "audio_input_unsupported",
            "{kind}: {frames:?}"
        );
        assert_eq!(
            error["message"],
            "cloud does not take audio input (upstream 'cloud-up'), so nothing was sent",
            "{kind}"
        );
        assert!(
            cloud.bodies().await.is_empty(),
            "{kind}: the cloud got nothing"
        );
        assert!(g.world().streamed_bodies.is_empty() && g.runs().is_empty());
    }
}

/// F3: the same unforeseen swap in a bound session: the turn goes again
/// as its transcript, with the capability's why, and is answered.
#[tokio::test]
async fn an_unforeseen_swap_to_a_fallback_that_reads_text_only_goes_as_the_transcript() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let cloud = Cloud::new(&g, text_only(), false).await;
    falls_back_to_cloud(&g, false).await;
    game_holds_the_card(&g);
    let (tid, mut ws) = session(&w).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let mut events = until_type(&mut ws, "lmgw.response.timing").await;
    events.extend(quiet(&mut ws).await);
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 2, "{events:?}");
    assert_eq!(input[0]["input"], "audio", "the verdict's prediction");
    assert_eq!(input[1]["input"], "transcript");
    assert_eq!(
        input[1]["why"],
        "cloud does not take audio input (upstream 'cloud-up'), so nothing was sent"
    );
    let sent = cloud.bodies().await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(audio_parts(&sent[0]), 0, "{}", sent[0]);
    // The retry's request row says its audio went as the transcript.
    assert_eq!(
        chat_row_marker(&g).await.as_deref(),
        Some("'cloud' lacks audio: transcript sent")
    );
    let r = rows(&w, tid).await;
    assert_eq!(
        r.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["Wie spät ist es?", CLOUD_SAYS]
    );
    assert!(r[0].3.get("input").is_none(), "a transcript row: {r:?}");
}

/// F5: a fallback that takes audio by its capabilities, whose API refuses
/// it naming the audio: the turn goes again as its transcript, kept for
/// the fallback alone — once the hold ends, the thread's own model hears
/// again.
#[tokio::test]
async fn a_fallbacks_refusal_of_the_audio_is_kept_for_the_fallback_alone() {
    let g = world().await;
    let cloud = Cloud::new(&g, hears(), true).await;
    falls_back_to_cloud(&g, true).await;
    let tid = thread(&g, "gemma", "chat").await;
    let id = row_written(&g, tid, "Und morgen?").await;
    let out = heard_response_for_tests(&g.state, tid, vec![audio()], id).await;
    assert_eq!(out.result, Ok(CLOUD_SAYS.to_string()), "{out:?}");
    assert_eq!(out.notes.len(), 1, "{out:?}");
    assert!(
        out.notes[0].contains("audio input is not supported"),
        "{out:?}"
    );
    assert_eq!(out.remembered.len(), 1, "{out:?}");
    assert_eq!(out.remembered[0].0, "cloud", "kept for the fallback");
    let sent = cloud.bodies().await;
    assert_eq!(sent.len(), 2, "the audio, then the transcript");
    assert_eq!((audio_parts(&sent[0]), audio_parts(&sent[1])), (1, 0));

    // The hold ends: the thread's own model hears the next turn.
    falls_back_to_cloud(&g, false).await;
    let id = row_written(&g, tid, "Und übermorgen?").await;
    let out = heard_response_for_tests(&g.state, tid, vec![audio()], id).await;
    assert_eq!(out.result, Ok(ANSWER.to_string()), "{out:?}");
    assert!(out.notes.is_empty(), "{out:?}");
    let local = g.world().streamed_bodies.clone();
    assert_eq!(local.len(), 1);
    assert_eq!(audio_parts(&local[0]), 1, "gemma heard it");
}
