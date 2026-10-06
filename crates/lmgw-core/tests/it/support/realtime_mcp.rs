//! Helpers for the realtime MCP call suites (realtime-server-tools design
//! §5): a session whose `mcp` labels are listed, the chat fake's turns that
//! call tools, the always-written fields of an `mcp_call`, and the rows its
//! calls leave.

#![allow(dead_code)]

use lmgw_core::state::SharedState;
use lmgw_core::store::{self, RequestLogRow};
use serde_json::Value;

use super::realtime_fakes::{events_until, next_event, open, send, Step, Turn, Ws};

/// `session.update` with these `tools`, past its `session.updated`.
pub async fn set_tools(ws: &mut Ws, tools: Value) {
    send(
        ws,
        serde_json::json!({"type": "session.update",
                           "session": {"type": "realtime", "tools": tools}}),
    )
    .await;
    let ev = next_event(ws).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
}

/// One label's listing, from its `conversation.item.added` to its
/// `conversation.item.done`; it must have completed.
pub async fn listed(ws: &mut Ws) -> Vec<Value> {
    let events = events_until(ws, "conversation.item.done").await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "mcp_list_tools.completed"),
        "{events:?}"
    );
    events
}

/// A text session on `chatty` — presenting `bearer` when given — with
/// `tools`, each of its `labels` listed.
pub async fn tools_session(addr: &str, bearer: Option<&str>, tools: Value, labels: usize) -> Ws {
    let auth = bearer.map(|b| format!("Bearer {b}"));
    let headers: Vec<(&str, &str)> = auth
        .as_deref()
        .map(|a| vec![("authorization", a)])
        .unwrap_or_default();
    let mut ws = open(addr, "/v1/realtime?model=chatty", &headers).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        serde_json::json!({"type": "session.update", "session": {"type": "realtime",
                           "output_modalities": ["text"], "tools": tools}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    for _ in 0..labels {
        listed(&mut ws).await;
    }
    ws
}

/// A model turn that calls tools: per call its upstream ordinal, id, name
/// and arguments (in one chunk), then `finish` and usage 6/4.
pub fn calls(
    calls: &[(u32, &'static str, &'static str, &'static str)],
    finish: &'static str,
) -> Turn {
    let mut steps = Vec::new();
    for (index, id, name, args) in calls {
        steps.push(Step::CallStart {
            index: *index,
            id: Some(id),
            name,
        });
        steps.push(Step::CallArgs {
            index: *index,
            args,
        });
    }
    steps.push(Step::Finish(finish));
    steps.push(Step::Usage(6, 4));
    Turn::Stream(steps)
}

/// Every field §1.3 says an `mcp_call` always carries is there.
pub fn assert_mcp_call(item: &Value) {
    assert_eq!(item["type"], "mcp_call", "{item}");
    for key in [
        "id",
        "server_label",
        "name",
        "arguments",
        "approval_request_id",
        "output",
        "error",
    ] {
        assert!(item.get(key).is_some(), "{key} missing: {item}");
    }
    assert!(item["arguments"].is_string(), "{item}");
}

/// Every `mcp_call` among `events` — carried as `item`, or in a
/// `response.output` — has every field.
pub fn assert_every_mcp_call(events: &[Value]) {
    for e in events {
        let mut items: Vec<&Value> = Vec::new();
        if let Some(i) = e.get("item") {
            items.push(i);
        }
        if let Some(out) = e.pointer("/response/output").and_then(Value::as_array) {
            items.extend(out);
        }
        for i in items.into_iter().filter(|i| i["type"] == "mcp_call") {
            assert_mcp_call(i);
        }
    }
}

/// The index of the first event of `kind` whose `item_id` (or `item.id`)
/// is `id`.
pub fn at(events: &[Value], kind: &str, id: &str) -> usize {
    events
        .iter()
        .position(|e| {
            e["type"] == kind
                && (e["item_id"] == id || e.pointer("/item/id").and_then(Value::as_str) == Some(id))
        })
        .unwrap_or_else(|| panic!("no {kind} for {id} in {events:#?}"))
}

/// The `request_logs` rows under `proto`, oldest first.
pub async fn rows(state: &SharedState, proto: &str) -> Vec<RequestLogRow> {
    let mut rows: Vec<RequestLogRow> = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 500,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_iter()
    .filter(|l| l.ingress_proto == proto)
    .collect();
    rows.reverse();
    rows
}

/// The `request_logs` rows under `proto` once there are `n`: a dropped
/// call's row is written as its responder lets go of it, after the events
/// that close it (realtime-server-tools §2.5). Fails after 5 s.
pub async fn rows_at_least(state: &SharedState, proto: &str, n: usize) -> Vec<RequestLogRow> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let found = rows(state, proto).await;
        if found.len() >= n {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} {proto} rows, not {n}: {found:?}",
            found.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The chat fake's request `n` as `(role, text or tool calls)` per message:
/// an assistant's tool call as `call:<id>:<name>:<args>`, a tool result as
/// `result:<tool_call_id>:<content>`.
pub fn shape(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap().to_string();
            let text = match role.as_str() {
                "tool" => format!(
                    "result:{}:{}",
                    m["tool_call_id"].as_str().unwrap(),
                    m["content"].as_str().unwrap()
                ),
                _ => {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
                        parts.push(t.to_string());
                    }
                    for c in m["tool_calls"].as_array().into_iter().flatten() {
                        parts.push(format!(
                            "call:{}:{}:{}",
                            c["id"].as_str().unwrap(),
                            c["function"]["name"].as_str().unwrap(),
                            c["function"]["arguments"].as_str().unwrap()
                        ));
                    }
                    parts.join(" | ")
                }
            };
            (role, text)
        })
        .collect()
}
