//! A parameter the model refuses, on the native `/v1/responses` passthrough
//! (api-compat design §9, a live compatibility run: `openai/gpt-6-luna`
//! answered every Responses request that carried `temperature` with "Unsupported
//! parameter: 'temperature' is not supported with this model."). The chat
//! sends' mechanic on the forwarded body: one retry without exactly that key,
//! the refused attempt's own row, `x-lmgw-params-refused`, and the route's
//! memory leaving it out of the next request's first send. What the
//! matcher does not take stands.

use lmgw_core::config::Protocol;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::support::responses_wire as rw;

const REFUSAL: &str = "Unsupported parameter: 'temperature' is not supported with this model.";

/// OpenAI's refusal of a parameter, as the platform words it: `param` set,
/// `code` null.
fn refusal(message: &str, param: Option<&str>) -> Value {
    json!({"error": {"message": message, "type": "invalid_request_error",
        "param": param, "code": null}})
}

/// A platform mock whose `/responses` refuses a body carrying
/// `temperature: 0.5` with [`REFUSAL`], and answers every other one, a
/// stream as a stream.
async fn refusing_temperature() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"temperature": 0.5})))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(refusal(REFUSAL, Some("temperature"))),
        )
        .with_priority(1)
        .mount(&mock)
        .await;
    answering(&mock).await;
    mock
}

/// `mock`'s `/responses` answers: a stream as a stream, else whole.
async fn answering(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            rw::stream("resp_1", &[rw::message("msg_1", "OK")]),
            "text/event-stream",
        ))
        .with_priority(2)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rw::response(
            "resp_1",
            "completed",
            json!([rw::message("msg_1", "OK")]),
        )))
        .with_priority(3)
        .mount(mock)
        .await;
}

/// A platform mock whose `/responses` refuses every body with `body`.
async fn refusing_all(body: Value) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(body))
        .mount(&mock)
        .await;
    mock
}

/// The bodies `/responses` was posted, in order.
async fn posted(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// `(status, error_msg)` of every request row, oldest first.
async fn rows(state: &SharedState) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT status, error_msg FROM request_logs ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap()
}

fn refused_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-lmgw-params-refused")
        .map(|v| v.to_str().unwrap().to_string())
}

/// The refused attempt's row and the answer's: what the note says.
fn assert_refused_then_answered(r: &[(i64, Option<String>)]) {
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].0, 400);
    let note = r[0].1.as_deref().unwrap();
    assert!(note.contains(REFUSAL), "{note}");
    assert!(
        note.contains("the model refused temperature; lmgw retried once without it"),
        "{note}"
    );
    assert_eq!(r[1], (200, None));
}

#[tokio::test]
async fn a_whole_answer_is_retried_without_the_refused_parameter_and_remembered() {
    let mock = refusing_temperature().await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.", "temperature": 0.5,
        "top_p": 0.9});
    let (status, headers, text) = post(&gw, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "completed", "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0]["temperature"], 0.5);
    assert!(sent[1].get("temperature").is_none(), "{}", sent[1]);
    assert_eq!(sent[1]["top_p"], 0.9, "only the named parameter goes");
    assert_refused_then_answered(&rows(&state).await);

    // Remembered: the next request's first send leaves it out, a stream
    // too, and says so.
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let (status, headers, text) = post(&gw, "/v1/responses", &streamed).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "no second refusal: {sent:#?}");
    assert!(sent[2].get("temperature").is_none(), "{}", sent[2]);
    assert_eq!(sent[2]["top_p"], 0.9);

    // A request without it says nothing.
    let (status, headers, _) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "input": "Reply with OK."}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(refused_header(&headers).is_none());
}

/// A stream's refusal is an HTTP status: the retry happens before any byte
/// is relayed, so its client reads one stream, and the header.
#[tokio::test]
async fn a_streams_refusal_is_retried_before_its_first_byte() {
    let mock = refusing_temperature().await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.", "temperature": 0.5,
        "stream": true});
    let (status, headers, text) = post(&gw, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let evs = events(&text);
    let types: Vec<&str> = evs.iter().filter_map(|(_, d)| d["type"].as_str()).collect();
    assert_eq!(
        types.iter().filter(|t| **t == "response.created").count(),
        1,
        "{text}"
    );
    assert_eq!(types.last(), Some(&"response.completed"), "{text}");
    assert!(
        !text.contains(REFUSAL),
        "the refusal never reached the client"
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert!(sent[1].get("temperature").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    assert_refused_then_answered(&r);
}

/// What the matcher does not take stands, sent once, with no header: a
/// refusal naming a parameter the body did not carry, `stop` (its answer
/// changes, not its sampling), and two parameters at once.
#[tokio::test]
async fn a_refusal_lmgw_cannot_retry_stands() {
    let cases = [
        (
            "not carried",
            refusal(
                "Unsupported parameter: 'top_k' is not supported with this model.",
                Some("top_k"),
            ),
            json!({"temperature": 0.5}),
        ),
        (
            "stop",
            refusal(
                "Unsupported parameter: 'stop' is not supported with this model.",
                Some("stop"),
            ),
            json!({"stop": ["5"]}),
        ),
        (
            "two at once",
            refusal(
                "`temperature` and `top_p` cannot both be specified for this model.",
                None,
            ),
            json!({"temperature": 0.5, "top_p": 0.9}),
        ),
    ];
    for (case, answer, extra) in cases {
        let mock = refusing_all(answer).await;
        let (state, gw) = gateway(&mock, Protocol::Openai).await;
        let mut body = json!({"model": "my-model", "input": "Reply with OK."});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        for stream in [false, true] {
            body["stream"] = json!(stream);
            let (status, headers, text) = post(&gw, "/v1/responses", &body).await;
            assert_eq!(status, 400, "{case}, stream {stream}: {text}");
            assert!(refused_header(&headers).is_none(), "{case}");
        }
        let sent = posted(&mock).await;
        assert_eq!(sent.len(), 2, "{case}: one send per request, {sent:#?}");
        for (k, v) in extra.as_object().unwrap() {
            assert_eq!(&sent[1][k], v, "{case}: nothing left out");
        }
        let r = rows(&state).await;
        assert_eq!(r.len(), 2, "{case}: {r:?}");
        assert!(
            r.iter()
                .all(|(_, note)| !note.as_deref().unwrap_or("").contains("retried")),
            "{case}: {r:?}"
        );
    }
}
