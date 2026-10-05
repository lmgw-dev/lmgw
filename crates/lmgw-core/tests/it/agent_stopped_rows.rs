//! A tool-loop turn that ends early still writes its request row (WP2
//! review M1): the loop stops the turn's model call cooperatively and waits
//! for it, so the call logs status 200, `canceled`, and what it cost so far
//! — for a streamed `/v1/responses` run whose client hung up, and for a
//! tool thread the page stopped. A dropped call would write no row, and a
//! client could take a streamed answer budget-free by hanging up.

use std::time::Duration;

use axum::body::{Body, Bytes};
use lmgw_core::state::SharedState;
use serde_json::json;

use crate::chat_actions::{get_json, post};
use crate::chat_agent_live::gateway_on;
use crate::chat_golden::{mcp_stub, read_until, register_stub, tool_thread};

/// An OpenAI-shaped upstream that streams a token every 10 ms and never ends
/// by itself: a model that generates for as long as someone reads.
async fn endless_upstream() -> String {
    let handler = || async {
        let body = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let line = json!({"choices": [{"delta": {"content": "x"}}]});
            Some((
                Ok::<_, std::io::Error>(Bytes::from(format!("data: {line}\n\n"))),
                (),
            ))
        });
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(body))
            .unwrap()
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The `(status, error_kind)` of every model-call row logged as `proto`,
/// once there is one.
async fn rows(state: &SharedState, proto: &str) -> Vec<(i64, Option<String>)> {
    for _ in 0..1000 {
        let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT status, error_kind FROM request_logs WHERE ingress_proto = ? ORDER BY id",
        )
        .bind(proto)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no '{proto}' row: the stopped call wrote none");
}

/// The in-flight gauge settles at 0: the stopped call closed what it opened.
async fn gauge_settles(state: &SharedState) {
    for _ in 0..500 {
        if state.telemetry.stats().active_requests == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the in-flight gauge still counts the stopped call");
}

#[tokio::test]
async fn a_streamed_response_whose_client_hangs_up_mid_turn_writes_its_row() {
    let base = endless_upstream().await;
    let (state, gw) = gateway_on(&base).await;
    let mut resp = reqwest::Client::new()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "m", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    read_until(&mut resp, "response.output_text.delta").await;
    drop(resp);

    let rows = rows(&state, "responses").await;
    assert_eq!(rows, [(200, Some("canceled".to_string()))]);
    gauge_settles(&state).await;
}

#[tokio::test]
async fn a_tool_thread_stopped_mid_turn_writes_its_row() {
    let base = endless_upstream().await;
    let (state, gw) = gateway_on(&base).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;
    let mut resp = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    read_until(&mut resp, "event: delta").await;
    // Stop: the page drops the stream while the model is generating.
    drop(resp);

    let rows = rows(&state, "chat").await;
    assert_eq!(rows, [(200, Some("canceled".to_string()))]);
    gauge_settles(&state).await;
    // …and the partial reply is saved as for any stop.
    let mut reply = serde_json::Value::Null;
    for _ in 0..100 {
        let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
        if detail["messages"].as_array().unwrap().len() == 2 {
            reply = detail["messages"][1].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text = reply["content"].as_str().unwrap_or_default();
    assert!(
        !text.is_empty() && text.chars().all(|c| c == 'x'),
        "{reply}"
    );
}
