//! A conversion's transcription and the calling key's policy, the GPU hold
//! and the file's deadline (document-conversion WP7 review round, S2–S5,
//! S7), against mock speech-to-text upstreams and the fake GPU world: a key
//! whose scope lacks the model, a key at its budget, a key at its
//! requests-per-minute limit (the call neither asks nor counts it), the
//! hold with and without a fallback, an upstream that fails, a call cut by
//! `document_timeout` (its `499` row), and what the upload is named.

use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::hash_api_key;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_attach_kinds::{chat_mock, setup, stt_mock, transcriptions, wav};
use crate::chat_voice_dictation::{asr_row, cloud_asr, failing_cloud_asr, tweak, world};
use crate::common::Gw;
use crate::support::gpu_world::GIB;

const KEY: &str = "lmgw-example-key";

/// A request of one audio file named `name`, with `extra` options.
fn request(name: &str, extra: Value) -> Value {
    let mut body = json!({
        "files": [{
            "name": name,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(wav()),
        }],
        "output": "markdown",
    });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    body
}

/// POST `body`, as `key` when given (else as the owner).
async fn post(gw: &Gw, body: &Value, key: Option<&str>) -> (u16, Value) {
    let url = format!("{gw}/v1/documents/convert");
    let c = match key {
        Some(k) => reqwest::Client::new().post(url).bearer_auth(k),
        None => gw.client().post(url),
    };
    let r = c.json(body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

/// Auth on and the key [`KEY`] with these columns.
async fn with_key(state: &SharedState, scope: (&str, &str), budget: i64, rpm: i64) {
    tweak(state, |s| s.auth_enabled = true).await;
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit)
         VALUES ('example-agent', ?1, 1, ?2, ?3, ?4, 'month', ?5, 0, 0)",
    )
    .bind(hash_api_key(KEY))
    .bind(scope.0)
    .bind(scope.1)
    .bind(budget)
    .bind(rpm)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The conversion's model rows: label, alias, status, fallback reason.
async fn asr_rows(state: &SharedState) -> Vec<(String, String, i64, Option<String>)> {
    sqlx::query_as(
        "SELECT ingress_proto, requested_alias, status, fallback_reason FROM request_logs
         WHERE ingress_proto = 'documents-asr' ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_key_that_may_not_use_the_model_or_is_over_budget_sends_nothing() {
    let (chat, stt) = (chat_mock().await, stt_mock("never heard").await);
    let (state, gw) = setup(&chat, &stt).await;
    tweak(&state, |s| s.documents.asr_alias = "my-asr".into()).await;
    with_key(&state, ("allow", "plain"), 0, 0).await;
    let (status, a) = post(&gw, &request("example.wav", json!({})), Some(KEY)).await;
    assert_eq!(status, 403, "{a}");
    assert_eq!(a["documents"][0]["error"]["code"], "model_not_allowed");
    // The budget: spent this month.
    sqlx::query("UPDATE api_keys SET scope_mode = 'all', budget_micro = 1000")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'),
                 (SELECT id FROM api_keys WHERE name='example-agent'), 'a', 1, 'chat', 'ok',
                 1, 2000000)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, a) = post(&gw, &request("example.wav", json!({})), Some(KEY)).await;
    assert_eq!(status, 403, "{a}");
    let e = &a["documents"][0]["error"];
    assert_eq!(e["code"], "model_not_allowed", "{e}");
    assert!(e["message"].as_str().unwrap().contains("budget"), "{e}");
    assert_eq!(transcriptions(&stt).await, 0);
    assert!(asr_rows(&state).await.is_empty());
}

/// S3: a key at one request a minute converts: the conversion is its
/// request, the transcription that request's work, neither asked about nor
/// counted in the window. S5: the upload is `audio.wav`, never the
/// caller's name. R4/R5: the row is `documents-asr`, the stats name it.
#[tokio::test]
async fn a_transcription_is_not_a_request_of_its_own_and_carries_no_file_name() {
    let (chat, stt) = (chat_mock().await, stt_mock("hello from the example").await);
    let (state, gw) = setup(&chat, &stt).await;
    tweak(&state, |s| s.documents.asr_alias = "my-asr".into()).await;
    with_key(&state, ("all", ""), 0, 1).await;
    let (status, a) = post(
        &gw,
        &request("Secret Meeting Name.wav", json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["content"], "hello from the example", "{d}");
    assert_eq!(d["stats"]["model_calls"], 1);
    let m = &d["stats"]["models"][0];
    assert_eq!(m["alias"], "my-asr", "{m}");
    assert_eq!(m["answered_for"], Value::Null);
    assert_eq!(m["bound_from"], "one_call");
    let sent = stt.received_requests().await.unwrap();
    let up = sent
        .iter()
        .find(|r| r.url.path() == "/v1/audio/transcriptions")
        .unwrap();
    let body = String::from_utf8_lossy(&up.body);
    assert!(body.contains("filename=\"audio.wav\""), "{body:.300}");
    assert!(!body.contains("Secret Meeting"));
    assert_eq!(
        asr_rows(&state).await,
        [("documents-asr".to_string(), "my-asr".to_string(), 200, None)]
    );
}

/// The GPU hold: a local model without a fallback is `model_held` (503)
/// and nothing starts; with one, its fallback answers, named in the
/// warning and in `stats.models`, and the row is the fallback's.
#[tokio::test]
async fn under_the_hold_a_fallback_transcribes_and_without_one_the_file_is_held() {
    let (g, gw) = world(10 * GIB, 2).await;
    let cloud = cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    asr_row(&g, "bare-ears", false, None).await;
    tweak(&g.state, |s| {
        s.hold.active = true;
        s.documents.asr_alias = "audio/bare-ears".into();
    })
    .await;
    let (status, a) = post(&gw, &request("example.wav", json!({})), None).await;
    assert_eq!(status, 503, "{a}");
    assert_eq!(a["documents"][0]["error"]["code"], "model_held");
    assert!(g.runs().is_empty(), "{:?}", g.runs());

    tweak(&g.state, |s| {
        s.documents.asr_alias = "audio/gpu-ears".into()
    })
    .await;
    let (status, a) = post(&gw, &request("example.wav", json!({})), None).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["content"], "from the provider", "{d}");
    let held = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "model_held")
        .expect("the fallback is named");
    let said = held["message"].as_str().unwrap();
    assert!(
        said.contains("audio/gpu-ears") && said.contains("cloud-asr"),
        "{said}"
    );
    let m = &d["stats"]["models"][0];
    assert_eq!(
        (m["alias"].as_str(), m["answered_for"].as_str()),
        (Some("cloud-asr"), Some("audio/gpu-ears"))
    );
    assert_eq!(crate::chat_attach_kinds::transcriptions(&cloud).await, 1);
    assert!(g.runs().is_empty(), "nothing local started: {:?}", g.runs());
    let rows = asr_rows(&g.state).await;
    assert_eq!(rows.last().unwrap().2, 200);
    assert_eq!(rows.last().unwrap().3.as_deref(), Some("hold"));
}

/// An upstream that fails is `model_failed` (502), its call kept in the
/// file's stats.
#[tokio::test]
async fn a_failing_model_fails_the_file_by_name_and_keeps_its_call() {
    let (g, gw) = world(10 * GIB, 2).await;
    let _mock = failing_cloud_asr(&g.state, "cloud-asr").await;
    tweak(&g.state, |s| s.documents.asr_alias = "cloud-asr".into()).await;
    let (status, a) = post(&gw, &request("example.wav", json!({})), None).await;
    assert_eq!(status, 502, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["error"]["code"], "model_failed", "{d}");
    assert_eq!(d["stats"]["model_calls"], 1, "{d}");
    assert_eq!(d["stats"]["models"][0]["alias"], "cloud-asr");
}

/// S2, S4: `document_timeout` covers the transcription; at the deadline
/// the call is dropped, the file fails with `deadline_exceeded`, and the
/// dropped call leaves its `499` row under the caller.
#[tokio::test]
async fn the_deadline_cuts_a_transcription_and_its_call_leaves_a_row() {
    let chat = chat_mock().await;
    let stt = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(4))
                .set_body_json(json!({ "text": "too late" })),
        )
        .mount(&stt)
        .await;
    let (state, gw) = setup(&chat, &stt).await;
    tweak(&state, |s| s.documents.asr_alias = "my-asr".into()).await;
    let started = std::time::Instant::now();
    let (status, a) = post(
        &gw,
        &request("example.wav", json!({ "document_timeout": 1 })),
        None,
    )
    .await;
    assert_eq!(status, 504, "{a}");
    assert_eq!(a["documents"][0]["error"]["code"], "deadline_exceeded");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    // The row is written from a task of its own.
    let mut rows = Vec::new();
    for _ in 0..50 {
        rows = asr_rows(&state).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        rows,
        [("documents-asr".to_string(), "my-asr".to_string(), 499, None)]
    );
}
