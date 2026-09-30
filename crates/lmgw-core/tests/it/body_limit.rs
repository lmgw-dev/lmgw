//! The `max_body_mb` setting (§13): the `/v1` request body ceiling is visible,
//! hot-applied, and names itself when it refuses a request.
//!
//! Before it existed, the JSON `/v1` routes inherited axum's 2 MiB
//! `DefaultBodyLimit` — a cap nothing in the dashboard or the API mentioned,
//! and whose 413 said only "payload too large". A base64 image a shade over the
//! line therefore failed with nothing to act on. These tests pin the three
//! properties that fix: the default is roomy, the refusal names the setting,
//! and changing the setting works without a restart.

use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

/// A gateway serving on an ephemeral port with `max_body_mb` set to `mb`.
async fn serve(mb: u32) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = mb;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, base)
}

/// A chat request whose single message is `bytes` long — the shape of the real
/// case, a base64 blob inlined into the conversation.
fn body_of(bytes: usize) -> Value {
    json!({
        "model": "no-such-model",
        "messages": [{"role": "user", "content": "x".repeat(bytes)}],
    })
}

async fn post(base: &str, path: &str, body: &Value, anthropic: bool) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(body);
    if anthropic {
        req = req.header("anthropic-version", "2023-06-01");
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let json = resp.json().await.unwrap_or(Value::Null);
    (status, json)
}

/// The refusal has to be actionable: 413, a `body_limit` code, and a message
/// that names both the setting and the value it is currently set to. A client
/// seeing only "too large" cannot tell whose limit it hit.
#[tokio::test]
async fn oversize_body_names_the_setting() {
    let (_state, base) = serve(1).await;

    let (status, err) = post(
        &base,
        "/v1/chat/completions",
        &body_of(2 * 1024 * 1024),
        false,
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(err["error"]["code"], "body_limit");
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("max_body_mb") && msg.contains("1 MiB") && msg.contains("Settings"),
        "413 does not say which knob to turn: {msg}"
    );
}

/// Anthropic clients get the Anthropic error envelope, as everywhere else.
#[tokio::test]
async fn oversize_body_keeps_the_clients_protocol_shape() {
    let (_state, base) = serve(1).await;

    let (status, err) = post(&base, "/v1/messages", &body_of(2 * 1024 * 1024), true).await;
    assert_eq!(status, 413);
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "invalid_request_error");
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_body_mb"));
}

/// The regression this whole setting exists for: a body over axum's old 2 MiB
/// default now reaches the router. Reaching it means being told the *model* is
/// unknown (404) — proof the body was read and parsed, not silently refused.
#[tokio::test]
async fn body_over_the_old_hidden_default_now_reaches_the_router() {
    let (_state, base) = serve(64).await;

    let (status, err) = post(
        &base,
        "/v1/chat/completions",
        &body_of(3 * 1024 * 1024),
        false,
    )
    .await;
    assert_eq!(
        status, 404,
        "a 3 MiB body should route and fail on the alias, not on its size: {err}"
    );
    assert_eq!(err["error"]["code"], "unknown_alias");
}

/// `0` is the visible way to say "no ceiling" — the same escape hatch
/// `/v1/audio/*` has always had, chosen deliberately rather than inherited.
#[tokio::test]
async fn zero_means_unlimited() {
    let (_state, base) = serve(0).await;

    let (status, err) = post(
        &base,
        "/v1/chat/completions",
        &body_of(8 * 1024 * 1024),
        false,
    )
    .await;
    assert_eq!(status, 404, "0 should impose no bound at all: {err}");
    assert_eq!(err["error"]["code"], "unknown_alias");
}

/// Hot-apply: the limit is read per request, so raising it takes effect on the
/// next request without restarting the gateway.
#[tokio::test]
async fn raising_the_limit_applies_without_a_restart() {
    let (state, base) = serve(1).await;
    let body = body_of(2 * 1024 * 1024);

    let (status, _) = post(&base, "/v1/chat/completions", &body, false).await;
    assert_eq!(status, 413);

    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 8;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, err) = post(&base, "/v1/chat/completions", &body, false).await;
    assert_eq!(
        status, 404,
        "same router, raised limit, still refusing: {err}"
    );
    assert_eq!(err["error"]["code"], "unknown_alias");
}

/// A chunked body declares no length, so the header check cannot see it coming
/// and the `Limited` wrapper is what actually stops it. That path arrives at the
/// handler as a body *rejection*, and it has to be translated back into the same
/// named error rather than surfacing as a generic 400 "malformed JSON".
#[tokio::test]
async fn chunked_body_without_content_length_is_bounded_too() {
    let (_state, base) = serve(1).await;

    // A stream body has no known length, so reqwest sends it chunked.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        (0..4).map(|_| Ok(vec![b'x'; 512 * 1024])).collect();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 413);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "body_limit");
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_body_mb"));
}

/// The audio and task routes carry whole audio files and were never bounded by
/// this setting; lowering it must not start bounding them.
#[tokio::test]
async fn audio_routes_stay_unbounded() {
    let (_state, base) = serve(1).await;

    let big = json!({"model": "no-such-model", "input": "x".repeat(2 * 1024 * 1024)});
    let (status, err) = post(&base, "/v1/audio/speech", &big, false).await;
    assert_ne!(
        status, 413,
        "audio route picked up the /v1 body limit: {err}"
    );
    assert_eq!(err["error"]["code"], "unknown_alias");
}
