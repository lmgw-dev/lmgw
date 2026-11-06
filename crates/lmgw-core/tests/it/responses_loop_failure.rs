//! A synthesized `/v1/responses` run that fails (api-compat design §9,
//! finding 10): a whole answer is the error's status with OpenAI's error
//! envelope (`message`, `type`, `param`, `code`), as the platform answers
//! and as every SDK reads a non-2xx, never the failed run's Response
//! object; nothing is stored, since the client got no id to fetch. A
//! stream has answered 200 by then and ends with `response.failed`. Level
//! 1 (`common::api_compat::answers`) checks the whole answer as the error
//! envelope.

use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::responses_api::{setup, sse_events};

/// A loop row whose upstream answers only after the row's timeout.
async fn timing_out() -> (lmgw_core::state::SharedState, String, MockServer) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(json!({"choices": []})),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    sqlx::query("UPDATE upstreams SET timeout_ms = 150")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    (state, base, mock)
}

#[tokio::test]
async fn a_whole_run_that_times_out_answers_the_error_envelope_and_stores_nothing() {
    let (state, base, _mock) = timing_out().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&json!({"model": "my-model", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    let v: Value = resp.json().await.unwrap();
    assert!(v.get("object").is_none(), "no Response object: {v}");
    let error = v["error"].as_object().expect("the error envelope");
    assert!(
        error["message"].as_str().is_some_and(|m| !m.is_empty()),
        "{v}"
    );
    assert_eq!(error["type"], "api_error", "{v}");
    assert_eq!(error["param"], Value::Null, "{v}");
    assert_eq!(error["code"], "timeout", "{v}");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM responses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(stored, 0, "a failed whole run is not stored");
}

#[tokio::test]
async fn a_streamed_run_that_times_out_ends_with_response_failed() {
    let (_state, base, _mock) = timing_out().await;
    let events = sse_events(
        &base,
        json!({"model": "my-model", "input": "hi", "stream": true}),
    )
    .await;
    let (name, data) = events.last().expect("events");
    assert_eq!(name, "response.failed", "{events:?}");
    assert_eq!(data["response"]["status"], "failed");
    assert_eq!(data["response"]["error"]["code"], "server_error");
}
