//! A stop on the platform is counted, not estimated (openai egress design
//! §13.4): an in-process turn its consumer stopped,
//! and a dashboard Chat turn stopped or ended without usage, on an `openai`
//! row, write no tokens and no cost; the budgets count lmgw's own count of
//! the request's text and the output relayed, as on the public path. Every
//! other row keeps the ~4 characters a token estimate as its usage and cost.

use std::time::Duration;

use axum::body::{Body, Bytes};

use super::*;
use crate::chat_actions::post;
use crate::chat_golden::{read_until, thread};

/// An OpenAI-shaped upstream that streams a token every 10 ms and never
/// ends by itself, nor reports usage: on the Responses wire (the platform's
/// chat sends) and on the Chat wire (an OpenAI-compatible row's).
async fn endless_upstream() -> String {
    fn endless(frame: fn() -> String) -> axum::response::Response {
        let body = futures::stream::unfold((), move |()| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from(frame())), ()))
        });
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(body))
            .unwrap()
    }
    let responses = || async {
        endless(|| {
            crate::support::responses_wire::sse(&[json!({
                "type": "response.output_text.delta", "output_index": 0, "delta": "x"})])
        })
    };
    let chat = || async {
        endless(|| {
            let line = json!({"choices": [{"index": 0, "delta": {"content": "x"}}]});
            format!("data: {line}\n\n")
        })
    };
    let app = axum::Router::new()
        .route("/responses", axum::routing::post(responses))
        .route("/chat/completions", axum::routing::post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A row's id, error kind, prompt tokens, cost and price note.
type Row = (
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// The one row logged as `proto`, once it is there.
async fn row_of(
    state: &SharedState,
    proto: &str,
) -> (
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
) {
    for _ in 0..1000 {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, error_kind, prompt_tokens, cost_micro, price_note FROM request_logs \
                 WHERE ingress_proto = ? ORDER BY id",
        )
        .bind(proto)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if let Some(r) = rows.into_iter().next() {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no '{proto}' row");
}

/// A streamed `/v1/responses` run kept in the loop (a label lmgw serves),
/// its client gone after the first delta: the loop stops the turn.
async fn stop_a_loop_turn(gw: &Gw) {
    let mut resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .json(
            &json!({"model": ALIAS, "input": "Say hello.", "stream": true,
                      "tools": [{"type": "mcp", "server_label": "docs"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    read_until(&mut resp, "response.output_text.delta").await;
    drop(resp);
}

#[tokio::test]
async fn a_stopped_in_process_turn_on_the_platform_floors_from_a_local_count() {
    let base = endless_upstream().await;
    let (state, gw) = setup_kind(&base, Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    stop_a_loop_turn(&gw).await;
    let (id, kind, prompt, cost, note) = row_of(&state, "responses").await;
    assert_eq!(kind.as_deref(), Some("canceled"));
    assert_eq!(
        (prompt, cost),
        (None, None),
        "no estimate is written as usage"
    );
    let note = note.unwrap_or_default();
    assert!(
        note.starts_with(
            "the caller stopped it before the upstream reported usage; budgets count a local \
             count of "
        ),
        "{note}"
    );
    let floor = floor_of(&state, id).await;
    assert!(floor.is_some_and(|f| f > 0), "{floor:?}");
    assert_eq!(spend(&state).await, floor.unwrap());
}

/// A compatible row keeps the estimate: its tokens and its cost.
#[tokio::test]
async fn a_stopped_in_process_turn_elsewhere_keeps_its_estimate() {
    let base = endless_upstream().await;
    let (state, gw) = setup_kind(&base, Protocol::OpenaiCompatible, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    stop_a_loop_turn(&gw).await;
    let (id, kind, prompt, cost, _) = row_of(&state, "responses").await;
    assert_eq!(kind.as_deref(), Some("canceled"));
    assert!(prompt.is_some() && cost.is_some(), "{prompt:?} {cost:?}");
    assert_eq!(floor_of(&state, id).await, None);
}

/// The dashboard Chat's plain turn, stopped by the page mid-answer.
#[tokio::test]
async fn a_stopped_dashboard_chat_turn_on_the_platform_floors_from_a_local_count() {
    let base = endless_upstream().await;
    let (state, gw) = setup_kind(&base, Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let tid = thread(&gw, json!({"model_alias": ALIAS})).await;
    let mut resp = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    read_until(&mut resp, "event: delta").await;
    drop(resp);
    let (id, kind, prompt, cost, note) = row_of(&state, "chat").await;
    assert_eq!(kind.as_deref(), Some("canceled"));
    assert_eq!((prompt, cost), (None, None));
    let note = note.unwrap_or_default();
    assert!(
        note.starts_with("the caller stopped it before the upstream reported usage; budgets count"),
        "{note}"
    );
    assert!(floor_of(&state, id).await.is_some_and(|f| f > 0));
}

/// The dashboard Chat's turn whose platform stream ended before its usage:
/// counted the same way.
#[tokio::test]
async fn a_dashboard_chat_stream_without_usage_floors_too() {
    let chunks = crate::support::responses_wire::sse(&[json!({
        "type": "response.output_text.delta", "output_index": 0, "delta": "Hello"})]);
    let (_m, state, gw) = platform(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(chunks),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let tid = thread(&gw, json!({"model_alias": ALIAS})).await;
    let resp = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    resp.text().await.unwrap();
    let (id, _, prompt, cost, note) = row_of(&state, "chat").await;
    assert_eq!((prompt, cost), (None, None));
    let note = note.unwrap_or_default();
    assert!(
        note.starts_with(
            "stream ended before the upstream reported usage; budgets count a local count of"
        ),
        "{note}"
    );
    assert!(floor_of(&state, id).await.is_some_and(|f| f > 0));
}
