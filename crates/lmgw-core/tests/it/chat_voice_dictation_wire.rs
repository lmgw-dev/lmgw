//! `POST /chat/api/threads/{id}/transcribe` against its types
//! (`lmgw-api-types::chat_voice`): the answer is a `Dictation` and every
//! failure of the call itself a plain `ApiError`, each read back and written
//! again as the same JSON, and the answer is validated against the document;
//! a fallback that had the audio is named by `x-lmgw-fallback` alone. The keys go out sorted, as the handler always wrote them. The
//! error frame's `kind` (a revoked stream's last frame) is described as
//! never `null`.

use lmgw_api_types::chat_voice::Dictation;
use lmgw_api_types::ApiError;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_attach_kinds::new_thread;
use crate::chat_voice_dictation::{
    asr_row, cloud_asr, failing_cloud_asr, recording, store_voice, tweak, world,
};
use crate::chat_voice_settings::{gateway, mocks, set_voice};
use crate::common::{round_trips, Gw};
use crate::support::gpu_world::GIB;

/// A provider answering every transcription with `status` and `body`.
async fn upstream(status: u16, body: Value) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&mock)
        .await;
    mock
}

/// POST `body` to the thread's `transcribe` as `content_type`: the status,
/// the `x-lmgw-fallback` header and the raw text of the answer.
async fn transcribe(
    gw: &Gw,
    tid: i64,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Option<String>, String) {
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let fallback = r
        .headers()
        .get("x-lmgw-fallback")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (status, fallback, r.text().await.unwrap())
}

/// The answer's text as the value it is, which must also be the text a
/// `Value` writes: compact, with the keys sorted.
fn sorted(text: &str) -> Value {
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!(text, v.to_string(), "the keys go out sorted");
    v
}

/// A refusal read into `ApiError` and written back as itself: `{code,
/// message}` and nothing else.
fn refusal(v: &Value) -> ApiError {
    let typed: ApiError = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(&serde_json::to_value(&typed).unwrap(), v, "{v}");
    typed
}

#[tokio::test]
async fn a_dictation_is_the_documented_answer() {
    let (chat, _stt, stt2) = mocks().await;
    // The provider hears German in its own spelling, in text full of what
    // JSON escapes.
    let heard = upstream(
        200,
        json!({ "text": "Hallo \"Welt\"\n<b>\u{fc}\u{2603}\t\\ end", "language": "german" }),
    )
    .await;
    let (state, gw) = gateway(&chat, &heard, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    tweak(&state, |s| s.chat_stt_alias = "my-asr".into()).await;

    let (status, fallback, text) = transcribe(&gw, tid, "audio/wav", recording(1)).await;
    assert_eq!((status, fallback), (200, None), "{text}");
    let v = sorted(&text);
    let wav = round_trips::<Dictation>("a WAV dictation", &v);
    assert_eq!(wav.text, "Hallo \"Welt\"\n<b>\u{fc}\u{2603}\t\\ end");
    assert_eq!(wav.alias, "my-asr");
    assert_eq!(wav.asr_answered_by, None);
    assert_eq!(wav.audio_ms, Some(1_000), "from the WAV header");
    assert_eq!(
        wav.language.as_deref(),
        Some("german"),
        "as the provider said"
    );
    assert!(wav.asr_ms.is_some(), "{v}");

    // Any other container has no length, and says so with a `null`.
    let (status, _, text) = transcribe(&gw, tid, "audio/webm", vec![1, 2, 3, 4]).await;
    assert_eq!(status, 200, "{text}");
    let v = sorted(&text);
    assert_eq!(v["audio_ms"], Value::Null, "the key is there, null: {v}");
    let webm = round_trips::<Dictation>("a WebM dictation", &v);
    assert_eq!(webm.audio_ms, None);
}

#[tokio::test]
async fn a_failed_call_is_a_plain_error_and_the_header_names_the_fallback_that_had_the_audio() {
    let (chat, stt, _stt2) = mocks().await;
    let failing = upstream(
        500,
        json!({ "error": { "message": "boom \"x\"", "type": "server_error" } }),
    )
    .await;
    let (_state, gw) = gateway(&chat, &stt, &failing).await;
    let tid = new_thread(&gw, "plain", false).await;

    // No speech-to-text model anywhere: refused before any call.
    let (status, _, text) = transcribe(&gw, tid, "audio/wav", recording(1)).await;
    assert_eq!(status, 422, "{text}");
    let v = sorted(&text);
    assert_eq!(v["code"], "asr_not_configured", "{v}");
    refusal(&v);

    // A thread that is not there, an empty recording: the same two keys.
    let (status, _, text) = transcribe(&gw, tid + 1000, "audio/wav", recording(1)).await;
    assert_eq!(status, 404, "{text}");
    let v = sorted(&text);
    refusal(&v);
    set_voice(&gw, tid, json!({ "asr_alias": "other-asr" })).await;
    let (status, _, text) = transcribe(&gw, tid, "audio/wav", Vec::new()).await;
    assert_eq!(status, 400, "{text}");
    let v = sorted(&text);
    assert_eq!(v["code"], "empty_audio", "{v}");
    refusal(&v);

    // The call went out and failed with no fallback: the same two keys, and no
    // header.
    let (status, fallback, text) = transcribe(&gw, tid, "audio/wav", recording(1)).await;
    assert_eq!((status, fallback), (502, None), "{text}");
    let v = sorted(&text);
    assert_eq!(v["code"], "upstream", "{v}");
    refusal(&v);
}

#[tokio::test]
async fn under_the_hold_the_fallback_is_named_by_the_answer_and_by_the_refusals_header() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let _cloud = cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = new_thread(&gw, "talk", false).await;
    store_voice(&g.state, tid, json!({ "asr_alias": "audio/gpu-ears" })).await;

    let (status, fallback, text) = transcribe(&gw, tid, "audio/wav", recording(1)).await;
    assert_eq!(
        (status, fallback.as_deref()),
        (200, Some("cloud-asr")),
        "{text}"
    );
    let answer = round_trips::<Dictation>("a fallback's dictation", &sorted(&text));
    assert_eq!(answer.alias, "audio/gpu-ears");
    assert_eq!(answer.asr_answered_by.as_deref(), Some("cloud-asr"));

    // The fallback's call fails: the header still says it had the audio.
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let _failing = failing_cloud_asr(&g.state, "cloud-asr").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = new_thread(&gw, "talk", false).await;
    store_voice(&g.state, tid, json!({ "asr_alias": "audio/gpu-ears" })).await;
    let (status, fallback, text) = transcribe(&gw, tid, "audio/wav", recording(1)).await;
    assert_eq!(
        (status, fallback.as_deref()),
        (502, Some("cloud-asr")),
        "{text}"
    );
    refusal(&sorted(&text));
}

#[test]
fn the_document_describes_the_error_frame_s_kind_as_never_null() {
    let doc = lmgw_core::openapi::admin_doc();
    let schemas = crate::common::documented_schemas(doc, "ErrorFrame");
    assert!(!schemas.is_empty(), "the document lists the error frame");
    for schema in schemas {
        // Only the message is always sent; `code` and `kind` are left out.
        assert_eq!(schema["required"], json!(["message"]), "{schema}");
        let kind = schema["properties"]["kind"].to_string();
        assert!(kind.contains("RevokeKind"), "{kind}");
        assert!(!kind.contains("\"null\""), "never sent as null: {kind}");
    }
}
