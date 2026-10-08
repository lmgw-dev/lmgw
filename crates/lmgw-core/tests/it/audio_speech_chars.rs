//! A character the TTS engine cannot say never fails the synthesis
//! (`audio::charset`): Supertonic refuses a whole reply for a single
//! character it lacks, e.g. the German `„` ("Supertonic unicode indexer has
//! no entry for codepoint 8222"). On an lmgw row whose package carries the engine's vocabulary such
//! a character is replaced by one it says, or dropped, and `x-lmgw-speech`
//! says which; the characters counted are the ones sent. An input with
//! nothing left to say is `empty_input`: a 400 on the HTTP routes, a skipped
//! clause in a spoken answer. The container stand-in refuses what the real
//! engine refuses (`support::audiocpp_options`), with the real indexer,
//! trimmed, and an empty text.

use lmgw_core::config::{HoldFallbackMode, Protocol, Settings, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::realtime_expressive::{answer, create, each, gateway, session, spoken, Row};
use crate::stream_usage::row_after;
use crate::support::audio_world::{wav, world};
use crate::support::{audiocpp_gguf, audiocpp_options};

/// A German reply as a model writes it: „…“, a dash, an ellipsis, an emoji.
const REPLY: &str = "„Gern“ – bis bald… 😊";

/// [`REPLY`] as Supertonic is sent it: `„` becomes `"`, the emoji goes, and
/// the rest is the engine's to say (it rewrites `“` and `–` itself, and
/// NFKD makes `…` three dots).
const SENT: &str = "\"Gern“ – bis bald… ";

mod refusals;

fn header(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-lmgw-speech")
        .map(|v| v.to_str().unwrap().to_string())
}

async fn tweak(state: &SharedState, f: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    f(&mut s);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn speech_sends_supertonic_only_characters_it_says_and_counts_those() {
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    w.answer_wav().await;

    let before = store::query_logs(&w.state.db, &Default::default())
        .await
        .unwrap()
        .len();
    let resp = w
        .speak(json!({"model": "audio/st", "input": REPLY, "voice": "F1"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp).as_deref(),
        Some("chars=replaced:U+201E,dropped:U+1F60A")
    );
    assert_eq!(
        w.sent().await,
        [json!({"model": "st", "input": SENT, "voice": "F1"})]
    );
    // chars_in is what was sent (billable-units design §4.3), not the
    // client's text.
    let row = row_after(&w.state, before).await;
    assert_eq!(row.chars_in, Some(SENT.chars().count() as i64));
    assert_ne!(row.chars_in, Some(REPLY.chars().count() as i64));

    // A text it says whole goes as it came, and says nothing.
    let resp = w
        .speak(json!({"model": "audio/st", "input": "Schöne Grüße — bis gleich…"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp).is_none());
    assert_eq!(w.sent().await[1]["input"], "Schöne Grüße — bis gleich…");
}

/// The failure this guards against: a package lmgw knows no vocabulary of is
/// sent the text as it came, and the engine refuses it.
#[tokio::test]
async fn without_the_vocabulary_the_engine_refuses_the_quote_as_it_did() {
    let w = world().await;
    w.row("bare", "supertonic", |r| {
        audiocpp_gguf::with_options(r, "supertonic", &[], &["offline"])
    })
    .await;
    w.answer_wav().await;
    let resp = w
        .speak(json!({"model": "audio/bare", "input": REPLY, "voice": "F1"}))
        .await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Supertonic unicode indexer has no entry for codepoint 8222"),
        "{body}"
    );
    assert_eq!(w.sent().await[0]["input"], REPLY);
}

/// Under the GPU hold the row's fallback answers — a cloud TTS, which is
/// sent the client's text unchanged: only an engine with a known
/// vocabulary is shaped for one.
#[tokio::test]
async fn a_fallback_that_answers_is_sent_the_text_unchanged() {
    let w = world().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(wav())
        .mount(&cloud)
        .await;
    let up = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "cloud-tts".into(),
            upstream_id: up,
            upstream_model_id: "gpt-4o-mini-tts".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    w.row_with("st", "supertonic", audiocpp_gguf::supertonic, |row| {
        row.hold_fallback_mode = HoldFallbackMode::Alias;
        row.hold_fallback = Some("cloud-tts".into());
    })
    .await;
    w.answer_wav().await;
    tweak(&w.state, |s| s.hold.active = true).await;

    let resp = w
        .speak(json!({"model": "audio/st", "input": REPLY, "voice": "alloy"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp).is_none(), "{:?}", header(&resp));
    let sent: Vec<Value> = cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["input"], REPLY);
    assert!(w.sent().await.is_empty(), "the local row was not asked");
}

fn supertonic() -> Row {
    (
        "st",
        "supertonic",
        "tts",
        json!({}),
        audiocpp_gguf::supertonic,
    )
}

/// A realtime answer goes through the same shaping (`Synthesis::speak`):
/// every clause is sent in the characters the engine says, and the answer
/// completes instead of failing at its first `„`.
#[tokio::test]
async fn a_spoken_answer_is_sent_in_the_characters_the_engine_says() {
    let (g, addr, chat) = gateway(&[supertonic()], 1, |_| {}).await;
    let (mut ws, _) = session(&addr, json!({})).await;
    let n = spoken(&g);
    let events = answer(
        &mut ws,
        &chat,
        &["„Gern“, sagte sie. ", "Bis bald… 😊"],
        create(),
    )
    .await;
    let said: Vec<Value> = each(&g, n, "input");
    assert!(!said.is_empty());
    let joined: String = said.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(!joined.contains('„') && !joined.contains('😊'), "{said:?}");
    assert!(joined.contains("\"Gern“, sagte sie."), "{said:?}");
    // The transcript is what the model wrote, as the listener reads it.
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    let transcript = done["output"][0]["content"][0]["transcript"]
        .as_str()
        .unwrap();
    assert!(transcript.contains('„'), "{transcript}");
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// An input of nothing Supertonic can say (review TC-1) would be sent
/// empty, which its engine refuses ("Supertonic requires --text input"), or
/// blank: it is `empty_input` instead, before anything is started, naming
/// the characters — and `x-lmgw-speech` says what shaping did.
#[tokio::test]
async fn speech_of_nothing_the_engine_says_is_empty_input_before_anything_starts() {
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    w.answer_wav().await;

    let resp = w.speak(json!({"model": "audio/st", "input": "😊"})).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(header(&resp).as_deref(), Some("chars=dropped:U+1F60A"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "empty_input", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("only characters 'audio/st' cannot say (U+1F60A \u{1F60A})"),
        "{message}"
    );
    // Blank once fitted: the same.
    let resp = w
        .speak(json!({"model": "audio/st", "input": " 👍🏽 🎉\n"}))
        .await;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        header(&resp).as_deref(),
        Some("chars=dropped:U+1F44D/U+1F3FD/U+1F389")
    );
    assert!(w.sent().await.is_empty());
    assert_eq!(w.runs(), 0, "no container was started for it");

    // An empty text, which the engine itself refuses with a 502 (TC-19):
    // the same `empty_input`, naming the alias asked for.
    let resp = w.speak(json!({"model": "audio/st", "input": ""})).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "empty_input", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'audio/st'"),
        "{body}"
    );
    assert!(w.sent().await.is_empty());
    assert_eq!(w.runs(), 0, "no container was started for it");
}

/// A reply that ends in an emoji (review TC-1): the clause of only `😊` is
/// skipped like a clause of only tags — no audio, its text kept in the
/// transcript — and the answer completes with the rest spoken. Before, it
/// went up as `""` and the engine's refusal failed the response.
#[tokio::test]
async fn a_clause_of_only_an_emoji_is_skipped_and_the_answer_completes() {
    let (g, addr, chat) = gateway(&[supertonic()], 1, |_| {}).await;
    let (mut ws, _) = session(&addr, json!({})).await;
    let n = spoken(&g);
    let events = answer(&mut ws, &chat, &["Gern geschehen! ", "😊"], create()).await;
    let said: Vec<Value> = each(&g, n, "input");
    assert_eq!(said, [json!("Gern geschehen!")], "one request, the text's");
    assert!(g.world().refused_speech.is_empty(), "nothing went up empty");
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    let transcript = done["output"][0]["content"][0]["transcript"]
        .as_str()
        .unwrap();
    assert!(
        transcript.contains("Gern geschehen!") && transcript.contains('😊'),
        "{transcript}"
    );
}

/// `/v1/tasks/run` reaches the same engine (review TC-4): its `text` is
/// fitted as speech's `input` is, said in `x-lmgw-speech`, and a text of
/// nothing the engine says is `empty_input` before anything starts.
#[tokio::test]
async fn a_task_run_s_text_is_fitted_like_speech() {
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    let podman = w.podman.clone();
    Mock::given(method("POST"))
        .and(path("/v1/tasks/run"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let served = podman.served.lock().unwrap();
            let speech = json!({"input": body["request"]["text"]});
            body["model"]
                .as_str()
                .and_then(|m| served.get(m))
                .and_then(|s| audiocpp_options::refusal(s, &speech))
                .unwrap_or_else(|| {
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(json!({"audio_base64": "", "timing": {"wall_ms": 1.0}}))
                })
        })
        .mount(&w.container)
        .await;
    let run = |body: Value| {
        w.gw.client()
            .post(format!("{}/v1/tasks/run", w.gw))
            .json(&body)
            .send()
    };

    let resp = run(json!({"model": "audio/st", "request": {"text": "😊 🎉"}}))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        header(&resp).as_deref(),
        Some("chars=dropped:U+1F60A/U+1F389")
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "empty_input", "{body}");
    assert_eq!(w.runs(), 0);

    let resp = run(json!({"model": "audio/st", "request": {"text": REPLY, "voice_id": "F1"}}))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp).as_deref(),
        Some("chars=replaced:U+201E,dropped:U+1F60A")
    );
    let sent: Vec<Value> = w
        .container
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/tasks/run")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        sent,
        [json!({"model": "st", "request": {"text": SENT, "voice_id": "F1"}})]
    );
}
