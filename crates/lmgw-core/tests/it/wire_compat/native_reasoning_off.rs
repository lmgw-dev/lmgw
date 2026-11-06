//! The reasoning off on the native `/v1/responses` passthrough
//! (model-capabilities design §5.6, api-compat design §9): fitted to the
//! model before the first send by the chat sends' rule and memory, written
//! into `reasoning.effort`, a refusal of it retried once with what the
//! refusal names and as the last resort with no control, each refused
//! attempt its own row, the form that answered remembered for the route,
//! and `enabled` in `x-lmgw-reasoning-ignored` when the off went out in
//! another form than `"none"`. What the retry cannot fix stands.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::support::responses_wire as rw;

mod edges;

/// `gpt-5-nano` on the Responses API: it reasons and cannot stop.
const NO_NONE: &str = "Unsupported value: 'reasoning.effort' does not support 'none' with this \
    model. Supported values are: 'minimal', 'low', 'medium', and 'high'.";

/// A model without reasoning refusing the control itself.
const NO_EFFORT: &str = "Unsupported parameter: 'reasoning.effort' is not supported with this \
    model.";

const TEMPERATURE: &str = "Unsupported parameter: 'temperature' is not supported with this model.";

/// OpenAI's refusal body.
fn refusal(message: &str, param: Option<&str>, code: Option<&str>) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": {"message": message,
        "type": "invalid_request_error", "param": param, "code": code}}))
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
        .with_priority(5)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rw::response(
            "resp_1",
            "completed",
            json!([rw::message("msg_1", "OK")]),
        )))
        .with_priority(6)
        .mount(mock)
        .await;
}

/// The `gpt-5-nano` platform: `reasoning.effort: "none"` refused with the
/// list of what it takes, everything else answered.
async fn cannot_stop() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"reasoning": {"effort": "none"}})))
        .respond_with(refusal(
            NO_NONE,
            Some("reasoning.effort"),
            Some("unsupported_value"),
        ))
        .with_priority(2)
        .mount(&mock)
        .await;
    answering(&mock).await;
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

fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| v.to_str().unwrap().to_string())
}

/// POST `body` to `route` with `headers`.
async fn post_with(
    gw: &Gw,
    route: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut rb = reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .json(body);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb.send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.unwrap())
}

const OFF: [(&str, &str); 1] = [("x-lmgw-reasoning", "off")];

fn native_body(stream: bool) -> Value {
    json!({"model": "my-model", "input": "Reply with OK.", "stream": stream,
        "reasoning": {"effort": "high", "summary": "auto"}})
}

/// An off a chat send learned for the route (`/v1/chat/completions` on the
/// platform goes out on the Responses wire too) is fitted on the native
/// passthrough before its first send: the model's least level at once, the
/// body's own effort replaced, its summary kept, whole and streamed.
#[tokio::test]
async fn an_off_learned_on_chat_is_fitted_before_the_first_native_send() {
    let mock = cannot_stop().await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let chat = json!({"model": "my-model",
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    let (status, headers, text) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("enabled")
    );
    let sent = posted(&mock).await;
    assert_eq!(
        sent.len(),
        2,
        "the chat's refusal, then its retry: {sent:#?}"
    );
    assert_eq!(sent[1]["reasoning"]["effort"], "minimal");

    for (n, stream) in [(3, false), (4, true)] {
        let (status, headers, text) =
            post_with(&gw, "/v1/responses", &OFF, &native_body(stream)).await;
        assert_eq!(status, 200, "stream {stream}: {text}");
        assert_eq!(
            header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
            Some("enabled"),
            "stream {stream}"
        );
        let sent = posted(&mock).await;
        assert_eq!(sent.len(), n, "one send, no refusal: {sent:#?}");
        assert_eq!(
            sent[n - 1]["reasoning"],
            json!({"effort": "minimal", "summary": "auto"}),
            "stream {stream}"
        );
    }
    let r = rows(&state).await;
    assert_eq!(r.len(), 4, "{r:?}");
    assert_eq!(r[0].0, 400);
    assert!(r[1..].iter().all(|(s, _)| *s == 200), "{r:?}");
}

/// An effort `none` the model refuses on the native passthrough is retried
/// with the least level its refusal lists, on record and remembered — for
/// the chat sends to the same route too.
#[tokio::test]
async fn a_refused_none_is_retried_with_the_fitted_form_and_remembered() {
    let mock = cannot_stop().await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.",
        "reasoning": {"effort": "none"}});
    let (status, headers, text) = post(&gw, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "completed", "{text}");
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("enabled")
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0]["reasoning"]["effort"], "none");
    assert_eq!(sent[1]["reasoning"]["effort"], "minimal");
    let r = rows(&state).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].0, 400);
    let note = r[0].1.as_deref().unwrap();
    assert!(note.contains(NO_NONE), "{note}");
    assert!(
        note.contains(
            r#"reasoning off was sent as reasoning.effort: "none"; lmgw retried once with reasoning.effort: "minimal""#
        ),
        "{note}"
    );
    assert_eq!(r[1], (200, None));

    // Remembered: a streamed request whose header asks for none goes out at
    // the least level at once, and so does a chat send's off.
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    streamed["reasoning"] = json!({"summary": "auto"});
    let (status, headers, _) = post_with(
        &gw,
        "/v1/responses",
        &[("x-lmgw-reasoning-effort", "none")],
        &streamed,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("enabled")
    );
    let chat = json!({"model": "my-model",
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    let (status, _, text) = post_with(&gw, "/v1/chat/completions", &OFF, &chat).await;
    assert_eq!(status, 200, "{text}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 4, "no second refusal: {sent:#?}");
    assert_eq!(
        sent[2]["reasoning"],
        json!({"effort": "minimal", "summary": "auto"})
    );
    assert_eq!(sent[3]["reasoning"]["effort"], "minimal");
    assert_eq!(rows(&state).await.len(), 4);

    // An off that answers as asked says nothing.
    let mock = MockServer::start().await;
    answering(&mock).await;
    let (_state, gw) = gateway(&mock, Protocol::Openai).await;
    let (status, headers, _) = post_with(&gw, "/v1/responses", &OFF, &native_body(false)).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "x-lmgw-reasoning-ignored").is_none());
    assert_eq!(
        posted(&mock).await[0]["reasoning"],
        json!({"effort": "none", "summary": "auto"})
    );
}

/// A model without reasoning refuses the control: the stream is retried with
/// no `effort` key before its first byte, so its client reads one stream.
#[tokio::test]
async fn a_streams_refused_off_is_retried_with_no_control_before_its_first_byte() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("\"effort\""))
        .respond_with(refusal(
            NO_EFFORT,
            Some("reasoning.effort"),
            Some("unsupported_parameter"),
        ))
        .with_priority(2)
        .mount(&mock)
        .await;
    answering(&mock).await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.", "stream": true});
    let (status, headers, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("enabled")
    );
    let evs = events(&text);
    let types: Vec<&str> = evs.iter().filter_map(|(_, d)| d["type"].as_str()).collect();
    assert_eq!(
        types.iter().filter(|t| **t == "response.created").count(),
        1,
        "{text}"
    );
    assert_eq!(types.last(), Some(&"response.completed"), "{text}");
    assert!(
        !text.contains(NO_EFFORT),
        "the refusal never reached the client"
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0]["reasoning"], json!({"effort": "none"}));
    assert!(sent[1].get("reasoning").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].0, 400);
    assert!(
        r[0].1
            .as_deref()
            .unwrap()
            .contains("lmgw retried once with no reasoning control"),
        "{r:?}"
    );
    assert_eq!(r[1], (200, None));

    // Remembered: the next off sends no control at once, whole too.
    let (status, headers, _) = post_with(&gw, "/v1/responses", &OFF, &native_body(false)).await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("enabled")
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2]["reasoning"], json!({"summary": "auto"}));
}

/// What the off's retry cannot fix stands, as on chat: a refusal about
/// something else is sent once more without the off and that answer stands,
/// unlearned; a prompt too long for the context stands at once; an explicit
/// level the model refuses is the provider's error.
#[tokio::test]
async fn a_refusal_the_off_cannot_fix_stands() {
    // About something else.
    let other = "Invalid value for 'input': expected a string or an array of items.";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(refusal(other, Some("input"), None))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    for stream in [false, true] {
        let mut body = native_body(stream);
        body["reasoning"] = json!({"effort": "none"});
        let (status, _, text) = post_with(&gw, "/v1/responses", &[], &body).await;
        assert_eq!(status, 400, "stream {stream}: {text}");
        assert!(text.contains(other), "{text}");
    }
    let sent = posted(&mock).await;
    assert_eq!(
        sent.len(),
        4,
        "each request: the off, then without it: {sent:#?}"
    );
    assert_eq!(
        sent[2]["reasoning"]["effort"], "none",
        "nothing was learned"
    );
    assert!(sent[3].get("reasoning").is_none(), "{}", sent[3]);
    let r = rows(&state).await;
    assert_eq!(r.len(), 4, "{r:?}");
    assert!(r.iter().all(|(s, _)| *s == 400), "{r:?}");
    assert!(r[0].1.as_deref().unwrap().contains("no reasoning control"));

    // A prompt too long for the context, and an explicit level: one send.
    let cases = [
        (
            "context",
            "This model's maximum context length is 128000 tokens. However, your messages \
             resulted in 130000 tokens.",
            json!({"effort": "none"}),
            &OFF[..],
        ),
        ("level", NO_EFFORT, json!({"effort": "high"}), &[][..]),
    ];
    for (case, message, reasoning, headers) in cases {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(refusal(message, None, None))
            .mount(&mock)
            .await;
        let (state, gw) = gateway(&mock, Protocol::Openai).await;
        let mut body = native_body(false);
        body["reasoning"] = reasoning.clone();
        let (status, _, text) = post_with(&gw, "/v1/responses", headers, &body).await;
        assert_eq!(status, 400, "{case}: {text}");
        let sent = posted(&mock).await;
        assert_eq!(sent.len(), 1, "{case}: {sent:#?}");
        assert_eq!(sent[0]["reasoning"], reasoning, "{case}");
        assert_eq!(rows(&state).await.len(), 1, "{case}");
    }
}

/// The parameters' retry and the off's share one loop and both apply: each
/// takes something out or lowers the off, so a request refused for both
/// ends answered, and one refused for ever ends too.
#[tokio::test]
async fn a_parameter_and_an_off_refused_in_one_request_both_retry_and_end() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"temperature": 0.5})))
        .respond_with(refusal(TEMPERATURE, Some("temperature"), None))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({"reasoning": {"effort": "none"}})))
        .respond_with(refusal(
            NO_NONE,
            Some("reasoning.effort"),
            Some("unsupported_value"),
        ))
        .with_priority(2)
        .mount(&mock)
        .await;
    answering(&mock).await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    for stream in [false, true] {
        let body = json!({"model": "my-model", "input": "Reply with OK.", "stream": stream,
            "temperature": 0.5});
        let (status, headers, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
        assert_eq!(status, 200, "stream {stream}: {text}");
        assert_eq!(
            header(&headers, "x-lmgw-params-refused").as_deref(),
            Some("temperature")
        );
        assert_eq!(
            header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
            Some("enabled")
        );
    }
    let sent = posted(&mock).await;
    assert_eq!(
        sent.len(),
        4,
        "the first: both refused in turn, then answered; the second: both learned: {sent:#?}"
    );
    assert_eq!(
        (&sent[0]["temperature"], &sent[0]["reasoning"]["effort"]),
        (&json!(0.5), &json!("none"))
    );
    assert!(sent[1].get("temperature").is_none());
    assert_eq!(sent[1]["reasoning"]["effort"], "none");
    assert!(sent[2].get("temperature").is_none());
    assert_eq!(sent[2]["reasoning"]["effort"], "minimal");
    assert!(sent[3].get("temperature").is_none());
    assert_eq!(sent[3]["reasoning"]["effort"], "minimal");
    let r = rows(&state).await;
    assert_eq!(
        r.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        [400, 400, 200, 200]
    );

    // Refused for ever, naming the parameter every time: the parameter goes,
    // then the off falls to no control, and the refusal stands.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(refusal(TEMPERATURE, Some("temperature"), None))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, Protocol::Openai).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.", "temperature": 0.5});
    let (status, _, text) = post_with(&gw, "/v1/responses", &OFF, &body).await;
    assert_eq!(status, 400, "{text}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert!(sent[1].get("temperature").is_none());
    assert_eq!(sent[1]["reasoning"]["effort"], "none");
    assert!(sent[2].get("reasoning").is_none(), "{}", sent[2]);
    assert_eq!(rows(&state).await.len(), 3);
}

/// A platform whose `/responses` answers the refusal of `effort: "none"`
/// with `status` at once and the rest of its body only once `release` is
/// notified; `sends` counts what it was sent.
async fn refusing_slowly(
    release: Arc<tokio::sync::Notify>,
    sends: Arc<AtomicUsize>,
    status: u16,
) -> String {
    use futures::StreamExt as _;
    let handler = move || {
        let (release, sends) = (release.clone(), sends.clone());
        async move {
            sends.fetch_add(1, Ordering::SeqCst);
            let body = json!({"error": {"message": NO_NONE, "type": "invalid_request_error",
                "param": "reasoning.effort", "code": "unsupported_value"}})
            .to_string();
            let (head, tail) = body.split_at(10);
            let (head, tail) = (head.to_string(), tail.to_string());
            let chunks = futures::stream::once(async move { Ok::<_, std::io::Error>(head) }).chain(
                futures::stream::once(async move {
                    release.notified().await;
                    Ok(tail)
                }),
            );
            axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(axum::body::Body::from_stream(chunks))
                .unwrap()
        }
    };
    let app = axum::Router::new().route("/responses", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A stream whose client leaves after the upstream refused its off, before
/// anything more went out, is sent no retry, and its row is the `499` with
/// nothing charged: an error answer ran nothing upstream.
#[tokio::test]
async fn a_stream_whose_client_left_after_a_refusal_is_sent_no_retry_and_charged_nothing() {
    let release = Arc::new(tokio::sync::Notify::new());
    let sends = Arc::new(AtomicUsize::new(0));
    let base = refusing_slowly(release.clone(), sends.clone(), 400).await;
    let (state, gw) =
        crate::e2e_proxy::setup_kind(&base, Protocol::Openai, UpstreamKind::Generic).await;
    let sent = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap()
        .post(format!("{gw}/v1/responses"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "my-model", "input": "Reply with OK.", "stream": true}))
        .send()
        .await;
    assert!(sent.is_err(), "the client gave up first: {sent:?}");
    let mut r = Vec::new();
    for _ in 0..200 {
        r = rows(&state).await;
        if !r.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    release.notify_waiters();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, 499);
    let msg = r[0].1.as_deref().unwrap();
    assert!(
        msg.contains("while the upstream's refusal (400) was read; nothing was retried"),
        "{msg}"
    );
    let (note, cost): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT price_note, cost_micro FROM request_logs")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(note, None, "no lower bound for a free refusal");
    assert_eq!(cost.unwrap_or(0), 0);
    assert_eq!(sends.load(Ordering::SeqCst), 1, "no retry went out");
}
