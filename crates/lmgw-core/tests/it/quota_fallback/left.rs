//! A client that leaves while its request is handed on (openai egress
//! design §14.7, review N1): the hop's attempt row is written once the walk
//! handed the request on, and the walk admits and cold-starts a local
//! fallback in the handler, which the hang-up cancels there. The row is
//! written all the same, saying where the request went and that the client
//! left — on a unary chat, a stream and a native `/v1/responses` forward.

use std::time::Duration;

use super::*;

/// An unreachable cloud alias `a` whose fallback is the local `m`, whose
/// start waits at the world's run gate; the gate.
async fn handed_to_a_slow_start(
    g: &Gpu,
) -> (tokio::sync::watch::Sender<bool>, tokio::net::TcpSocket) {
    let (port, held) = crate::common::refusing_port();
    upstream_alias(
        g,
        "a",
        &format!("http://127.0.0.1:{port}/v1"),
        Protocol::OpenaiCompatible,
    )
    .await;
    g.model("m", 4 * GIB).await;
    set_fallback(&g.state, "models", "a", Some("m")).await;
    (g.gate_runs(), held)
}

/// The one row of `a`, once the client is gone: the attempt's, saying the
/// request went to `m` and the client left.
async fn the_attempts_row(g: &Gpu) {
    let row = rows(g, "a", 1).await.remove(0);
    assert_eq!(row.status, 502, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("unreachable"), "{row:?}");
    let msg = row.error_msg.as_deref().unwrap_or_default();
    assert!(
        msg.contains("; handed to fallback 'm' (unreachable); the client left before it answered"),
        "{msg}"
    );
}

/// How long the client stays: past the refused connect, while the
/// fallback's start waits at the gate (which holds it as long as needed).
/// Windows answers a connect to a port nobody listens on only after it has
/// retried the SYN, about 2 s on loopback; Linux at once.
const STAYS: Duration = if cfg!(windows) {
    Duration::from_secs(5)
} else {
    Duration::from_millis(400)
};

async fn leaves(gw: &Gw, route: &str, body: Value) {
    let gone = gw
        .client()
        .post(format!("{gw}{route}"))
        .timeout(STAYS)
        .json(&body)
        .send()
        .await;
    assert!(gone.is_err(), "the client left during the fallback's start");
}

#[tokio::test]
async fn a_unary_chat_left_during_the_hand_over_keeps_the_attempts_row() {
    let (g, gw) = world().await;
    let (_gate, _held) = handed_to_a_slow_start(&g).await;
    leaves(
        &gw,
        "/v1/chat/completions",
        json!({"model": "a", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    the_attempts_row(&g).await;
}

#[tokio::test]
async fn a_stream_left_during_the_hand_over_keeps_the_attempts_row() {
    let (g, gw) = world().await;
    let (_gate, _held) = handed_to_a_slow_start(&g).await;
    leaves(
        &gw,
        "/v1/chat/completions",
        json!({"model": "a", "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    the_attempts_row(&g).await;
}

#[tokio::test]
async fn a_native_forward_left_during_the_hand_over_keeps_the_attempts_row() {
    let (g, gw) = world().await;
    let (port, _held) = crate::common::refusing_port();
    upstream_alias(
        &g,
        "a",
        &format!("http://127.0.0.1:{port}"),
        Protocol::Openai,
    )
    .await;
    g.model("m", 4 * GIB).await;
    set_fallback(&g.state, "models", "a", Some("m")).await;
    let _gate = g.gate_runs();
    leaves(&gw, "/v1/responses", json!({"model": "a", "input": "hi"})).await;
    the_attempts_row(&g).await;
}
