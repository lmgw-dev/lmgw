//! Anthropic's `request-id` on `/v1/messages` (gateway design §6.2): an
//! answer an Anthropic upstream gave carries its `request-id` header, whole
//! and streamed, and an error that is that upstream's own answer carries its
//! id in the body's `request_id` and in the header. Another upstream's
//! answer, and an error lmgw raises itself, carry none (lmgw makes no id of
//! its own yet).

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::anthropic_prompt_caching::anthropic_stream;
use crate::messages_context_management::world;

const ID: &str = "req_011CTestRequestId";

async fn post(gw: &crate::common::Gw, model: &str, stream: bool) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": model, "max_tokens": 16, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap()
}

fn request_id(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("request-id")
        .map(|v| v.to_str().unwrap().to_string())
}

fn message() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
        "stop_sequence": null, "usage": {"input_tokens": 3, "output_tokens": 1},
    })
}

#[tokio::test]
async fn an_anthropic_answer_carries_its_request_id_whole_and_streamed() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let t = ResponseTemplate::new(200).insert_header("request-id", ID);
            if body["stream"] == true {
                t.set_body_raw(
                    anthropic_stream(
                        &json!({"input_tokens": 3, "output_tokens": 1}),
                        &json!({"output_tokens": 2}),
                    ),
                    "text/event-stream",
                )
            } else {
                t.set_body_json(message())
            }
        })
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    for stream in [false, true] {
        let resp = post(&gw, "m", stream).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(request_id(&resp).as_deref(), Some(ID), "stream {stream}");
        let _ = resp.text().await.unwrap();
    }
}

/// The upstream's own error: its body's `request_id` in lmgw's envelope,
/// else the header's; the response header is the upstream's own header, else
/// the body's id when that is a valid header value, else none.
#[tokio::test]
async fn an_anthropic_error_carries_its_request_id() {
    // (body id, upstream header, header out, body id out)
    let cases = [
        (Some(ID), Some("req_header"), Some("req_header"), Some(ID)),
        (None, Some(ID), Some(ID), Some(ID)),
        (Some(ID), None, Some(ID), Some(ID)),
        (Some("req_bad\u{1}id"), None, None, Some("req_bad\u{1}id")),
        (
            Some("req_bad\u{1}id"),
            Some("req_header"),
            Some("req_header"),
            Some("req_bad\u{1}id"),
        ),
    ];
    for (body_id, header_id, header_out, body_out) in cases {
        let mock = MockServer::start().await;
        let mut body = json!({"type": "error", "error": {
            "type": "invalid_request_error", "message": "Your request was rejected."}});
        if let Some(id) = body_id {
            body["request_id"] = json!(id);
        }
        let mut t = ResponseTemplate::new(400).set_body_json(body);
        if let Some(id) = header_id {
            t = t.insert_header("request-id", id);
        }
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(t)
            .mount(&mock)
            .await;
        let gw = world(&mock.uri(), Protocol::Anthropic).await;
        for stream in [false, true] {
            let resp = post(&gw, "m", stream).await;
            assert_eq!(resp.status(), 400);
            assert_eq!(request_id(&resp).as_deref(), header_out, "stream {stream}");
            let err: Value = resp.json().await.unwrap();
            assert_eq!(err["type"], "error", "{err}");
            assert_eq!(err["error"]["type"], "invalid_request_error", "{err}");
            assert_eq!(err["request_id"], json!(body_out), "{err}");
        }
    }
}

/// Not yet: lmgw makes no id of its own. Another upstream's answer and
/// error, and an error lmgw raises itself, carry none.
#[tokio::test]
async fn other_answers_and_lmgw_s_own_errors_carry_none() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("request-id", "req_other")
                .set_body_json(
                    json!({"error": {"message": "bad", "type": "invalid_request_error"}}),
                ),
        )
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let resp = post(&gw, "m", false).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(request_id(&resp), None);
    let err: Value = resp.json().await.unwrap();
    assert!(err.get("request_id").is_none(), "{err}");

    let unknown = post(&gw, "no-such-model", false).await;
    assert_eq!(unknown.status(), 404);
    assert_eq!(request_id(&unknown), None);
    let err: Value = unknown.json().await.unwrap();
    assert!(err.get("request_id").is_none(), "{err}");
}
