//! Cancelling a response whose server-side calls run, text mode
//! (realtime-server-tools design §2.5; WP4): the core closes each open
//! `mcp_call` itself — a sent call as abandoned, one never made as such —
//! with `response.mcp_call.failed` and its done events; the next response
//! reads that error; the responder writes a `canceled` row for each call it
//! dropped and none for a call it never made; a hang-up does the same with
//! nobody listening; and an unfinished call cannot be deleted.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::mcp_stub::{held_call_stub, register, McpStub};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, send, types, user_text, Step, Turn, Ws,
};
use crate::support::realtime_mcp::{
    assert_every_mcp_call, at, calls, rows, rows_at_least, shape, tools_session,
};
use lmgw_core::state::SharedState;

/// What an abandoned call's item, its row and the model say
/// (`agent::ABANDONED_CALL`).
const ABANDONED: &str =
    "abandoned when the run was cancelled; it had already been sent, so it may still have run";
/// …and a call the response ended before making (`agent::UNMADE_CALL`).
const UNMADE: &str = "not run: the turn ended before this call was made";

/// The label `a`, every tool of it.
fn label_a() -> Value {
    json!([{"type": "mcp", "server_label": "a", "require_approval": "never"}])
}

/// The registered server `alpha` (tool prefix `a`) behind `stub`.
async fn alpha(state: &SharedState, stub: McpStub) -> McpStub {
    register(state, "alpha", "a", &stub.url, true, None).await;
    stub
}

/// A user turn, and `response.create`.
async fn ask(ws: &mut Ws, text: &str) {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
}

/// The `mcp_call` item every `response.output_item.done` among `events`
/// closed, in order.
fn closed_calls(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "mcp_call")
        .map(|e| e["item"].clone())
        .collect()
}

/// `item` closed with `output: null` and a `tool_execution_error` saying
/// `message`, and said so with `.failed`, `output_item.done` and
/// `conversation.item.done`, in that order.
fn failed_with(events: &[Value], item: &Value, message: &str) {
    let id = item["id"].as_str().unwrap();
    assert_eq!(item["output"], Value::Null, "{item}");
    assert_eq!(item["error"]["type"], "tool_execution_error", "{item}");
    assert_eq!(item["error"]["message"], message, "{item}");
    let failed = at(events, "response.mcp_call.failed", id);
    let output_done = at(events, "response.output_item.done", id);
    let item_done = at(events, "conversation.item.done", id);
    assert!(
        failed < output_done && output_done < item_done,
        "{events:#?}"
    );
    assert_eq!(events[item_done]["item"], *item);
}

/// A `response.cancel` while the call runs: the core abandons it at once —
/// the server has it, so it may have run — and `response.done` carries it
/// with that error. Its row says `canceled`, and the next response reads
/// the call and the error, not "(no result yet)".
#[tokio::test]
async fn a_cancel_while_a_call_runs_abandons_it_and_the_next_response_reads_the_error() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_c1", "a__echo", r#"{"text":"slow"}"#)],
        "tool_calls",
    ));
    fake.push(Turn::text(&["It ", "was cut."]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    let stub = alpha(&state, stub).await;
    let mut ws = tools_session(&addr, None, label_a(), 1).await;
    ask(&mut ws, "slow one").await;
    events_until(&mut ws, "response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;

    send(
        &mut ws,
        json!({"type": "response.cancel", "event_id": "stop"}),
    )
    .await;
    let events = events_until(&mut ws, "response.done").await;
    assert!(!types(&events).contains(&"error"), "{events:#?}");
    let closed = closed_calls(&events);
    assert_eq!(closed.len(), 1, "{events:#?}");
    failed_with(&events, &closed[0], ABANDONED);
    let done = &events.last().unwrap()["response"];
    assert_eq!(
        (&done["status"], &done["status_details"]["reason"]),
        (&json!("cancelled"), &json!("client_cancelled"))
    );
    assert_eq!(done["output"], json!([closed[0]]));
    assert_every_mcp_call(&events);

    // One row, written by the responder for the call its stop dropped.
    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(tool_rows.len(), 1, "{tool_rows:?}");
    let row = &tool_rows[0];
    assert_eq!(
        (
            row.status,
            row.error_kind.as_deref(),
            row.error_msg.as_deref()
        ),
        (200, Some("canceled"), Some(ABANDONED))
    );
    assert_eq!(row.mcp_tool.as_deref(), Some("a__echo"));
    assert_eq!(row.upstream_name.as_deref(), Some("alpha"));
    assert_eq!(row.class.as_deref(), Some("tool"));
    assert!(row.total_ms.is_some() && row.prompt_tokens.is_none());

    // The model reads the call and what became of it.
    ask(&mut ws, "and now?").await;
    let next = events_until(&mut ws, "response.done").await;
    assert_eq!(next.last().unwrap()["response"]["status"], "completed");
    assert_eq!(
        shape(&fake.seen.chat(1)),
        [
            ("user".to_string(), "slow one".to_string()),
            (
                "assistant".into(),
                r#"call:call_c1:a__echo:{"text":"slow"}"#.into()
            ),
            ("tool".into(), format!("result:call_c1:{ABANDONED}")),
            ("user".into(), "and now?".into()),
        ]
    );
}

/// A cancel while the model still writes the call: it was never made, so
/// it closes as such, the server sees nothing, and there is no tool row.
#[tokio::test]
async fn a_cancel_before_a_call_is_made_closes_it_unmade_and_nothing_runs() {
    let fake = chat_fake().await;
    let never = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_u1"),
            name: "a__echo",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"text":"x"}"#,
        },
        Step::Wait(never.clone()),
        Step::Finish("tool_calls"),
    ]));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    let stub = alpha(&state, stub).await;
    let mut ws = tools_session(&addr, None, label_a(), 1).await;
    ask(&mut ws, "never mind").await;
    events_until(&mut ws, "response.mcp_call_arguments.delta").await;

    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let closed = closed_calls(&events);
    assert_eq!(closed.len(), 1, "{events:#?}");
    failed_with(&events, &closed[0], UNMADE);
    assert!(
        !types(&events).contains(&"response.mcp_call.in_progress"),
        "{events:#?}"
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "cancelled");

    // The model call's own row is written once its stream is stopped; by
    // then any tool row would be too.
    rows_at_least(&state, "realtime", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(stub.calls().is_empty(), "{:?}", stub.calls());
    assert!(rows(&state, "realtime-tool").await.is_empty());
}

/// With `parallel_tool_calls: false` a cancel finds one call running and
/// the next not made: abandoned and never made, in model order — and only
/// the call the server saw has a row.
#[tokio::test]
async fn a_cancel_between_sequential_calls_abandons_the_running_one_and_leaves_the_next_unmade() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[
            (0, "call_s1", "a__echo", r#"{"text":"one"}"#),
            (1, "call_s2", "a__echo", r#"{"text":"two"}"#),
        ],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    let stub = alpha(&state, stub).await;
    let mut ws = tools_session(&addr, None, label_a(), 1).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "parallel_tool_calls": false}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    ask(&mut ws, "two in a row").await;
    events_until(&mut ws, "response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;

    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let closed = closed_calls(&events);
    assert_eq!(closed.len(), 2, "{events:#?}");
    failed_with(&events, &closed[0], ABANDONED);
    failed_with(&events, &closed[1], UNMADE);

    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rows(&state, "realtime-tool").await.len(), 1);
    assert_eq!(tool_rows[0].error_kind.as_deref(), Some("canceled"));
    assert_eq!(
        stub.calls(),
        vec![("echo".to_string(), json!({"text": "one"}))]
    );
}

/// A client that hangs up while a call runs: nobody is told, but the
/// responder still lets go of the call — it does not wait on it — and
/// writes its `canceled` row, beside the model call's own.
#[tokio::test]
async fn a_hang_up_while_a_call_runs_drops_it_and_writes_its_canceled_row() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_h1", "a__echo", r#"{"text":"bye"}"#)],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (stub, _gate) = held_call_stub().await;
    let stub = alpha(&state, stub).await;
    let mut ws = tools_session(&addr, None, label_a(), 1).await;
    ask(&mut ws, "and hang up").await;
    events_until(&mut ws, "response.mcp_call.in_progress").await;
    stub.wait_calls(1).await;
    drop(ws);

    let tool_rows = rows_at_least(&state, "realtime-tool", 1).await;
    assert_eq!(
        (tool_rows[0].status, tool_rows[0].error_kind.as_deref()),
        (200, Some("canceled"))
    );
    assert_eq!(tool_rows[0].mcp_tool.as_deref(), Some("a__echo"));
    let model = rows_at_least(&state, "realtime", 1).await;
    assert_eq!(model.len(), 1, "{model:?}");
}

/// `conversation.item.delete` of an `mcp_call` its response has not closed
/// is refused — its result would name an item the client was told is gone
/// — and goes through once the call is done.
#[tokio::test]
async fn an_unfinished_call_cannot_be_deleted() {
    let fake = chat_fake().await;
    fake.push(calls(
        &[(0, "call_d1", "a__echo", r#"{"text":"keep"}"#)],
        "tool_calls",
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let (stub, gate) = held_call_stub().await;
    let _stub = alpha(&state, stub).await;
    let mut ws = tools_session(&addr, None, label_a(), 1).await;
    ask(&mut ws, "keep it").await;
    let events = events_until(&mut ws, "response.mcp_call.in_progress").await;
    let id = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added")
        .map(|e| e["item"]["id"].as_str().unwrap().to_string())
        .unwrap();

    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "item_id": id, "event_id": "del"}),
    )
    .await;
    let refused = next_event(&mut ws).await;
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(refused["error"]["code"], "invalid_value");
    assert_eq!(refused["error"]["param"], "item_id");
    assert_eq!(refused["error"]["event_id"], "del");

    gate.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"][0]["output"], "echo: keep");
    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "item_id": id}),
    )
    .await;
    let deleted = next_event(&mut ws).await;
    assert_eq!(deleted["type"], "conversation.item.deleted", "{deleted}");
    assert_eq!(deleted["item_id"], id.as_str());
}
