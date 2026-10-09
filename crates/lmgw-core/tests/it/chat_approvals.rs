//! MCP approvals in the Chat (client-apps design §6, §9's approvals list),
//! through the router and the gate, the tools hosted by a fake device over
//! `GET /mcp/host` so every forwarded call's `_meta` is read as a device
//! reads it.
//!
//! - `turns`: the round trip — a gated turn's frames, its saved reply and
//!   pending state, the decision resuming it — the sibling rule, a decline's
//!   words, the route's refusals, the first decision winning, a new message
//!   declining, and `require_approval` checked on write;
//! - `starter`: the resumed turn runs as its starter, whoever approves; `by`
//!   on the row, in the feed's `approval.decided` and in `_meta`; a starter
//!   whose key went away;
//! - `bound`: a bound voice session's `mcp_approval_request` item, its
//!   answer resuming the turn in one response, `lmgw.approval.decided` for
//!   a decision made elsewhere and for a reply edited or deleted, and the
//!   starter's concurrency slot a resume takes;
//! - `reach`: a device approves only within its own reach;
//! - `races`: two deciders at once, a temporary thread's approvals;
//! - `feed`: the records hidden from a device that cannot see the thread;
//! - `responses`: `by` on a `/v1/responses` continuation.

use std::time::Duration;

use serde_json::{json, Value};

use crate::device_chat::{post, sse};
use crate::mcp_host::FakeDevice;
use crate::realtime_chat_thread::World;
use crate::support::realtime_mcp::calls;

mod bound;
mod feed;
mod races;
mod reach;
mod responses;
mod starter;
mod turns;

/// The tool entry every test attaches: the device's label with `notify`
/// gated, `echo` not.
pub(crate) fn gated_label() -> Value {
    json!([{"server_label": "desktop",
            "require_approval": {"always": {"tool_names": ["notify"]}}}])
}

/// Attach [`gated_label`] to thread `tid` as `client`.
pub(crate) async fn attach(w: &World, client: &reqwest::Client, tid: i64) {
    let (s, v) = post(
        w,
        client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": gated_label() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// The next model turn calls `echo` and `notify` (gated) at once.
pub(crate) fn both_calls(w: &World) {
    w.chat.push(calls(
        &[
            (0, "call_1", "desktop__echo", "{}"),
            (1, "call_2", "desktop__notify", r#"{"text":"hi"}"#),
        ],
        "tool_calls",
    ));
}

/// A send of `content` to `tid` as `client`: its frames.
pub(crate) async fn send(
    w: &World,
    client: &reqwest::Client,
    tid: i64,
    content: &str,
) -> Vec<(String, Value)> {
    let (s, frames) = sse(
        w,
        client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": content }),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    frames
}

/// The `tool {event: "approval"}` frames of a turn.
pub(crate) fn approval_frames(frames: &[(String, Value)]) -> Vec<Value> {
    frames
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == "approval")
        .map(|(_, d)| d.clone())
        .collect()
}

/// A turn's `done` frame.
pub(crate) fn done(frames: &[(String, Value)]) -> Value {
    frames
        .iter()
        .rev()
        .find(|(e, _)| e == "done")
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| panic!("no done frame in {frames:?}"))
}

/// `POST …/approvals` as `client`: the status and, for a stream, its frames
/// (else the refusal as one `("refusal", body)` frame).
pub(crate) async fn decide(
    w: &World,
    client: &reqwest::Client,
    tid: i64,
    decisions: Value,
) -> (u16, Vec<(String, Value)>) {
    let resp = client
        .post(format!("{}/chat/api/threads/{tid}/approvals", w.gw))
        .json(&json!({ "decisions": decisions }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = tokio::time::timeout(Duration::from_secs(30), resp.text())
        .await
        .expect("the answer ends")
        .unwrap();
    if status == 200 {
        (status, crate::device_chat::frames(&text))
    } else {
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, vec![("refusal".to_string(), body)])
    }
}

/// The device saw no call within a moment.
pub(crate) async fn no_call(dev: &mut FakeDevice) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dev.calls.try_recv().is_err(), "the device got a call");
}

/// `approved_by` of the tool rows of `tool`, oldest first.
pub(crate) async fn approved_by(w: &World, tool: &str) -> Vec<Option<String>> {
    // A row is written on its own task: give the last one a moment.
    tokio::time::sleep(Duration::from_millis(150)).await;
    sqlx::query_scalar(
        "SELECT approved_by FROM request_logs WHERE mcp_tool = ?1 AND class = 'tool' ORDER BY id",
    )
    .bind(tool)
    .fetch_all(&w.state.db)
    .await
    .unwrap()
}

/// The tool results the model's request `n` carried, as `(call id, text)`.
pub(crate) fn tool_results(w: &World, n: usize) -> Vec<(String, String)> {
    let req = w.chat.seen.chat(n);
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            let text = match &m["content"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (m["tool_call_id"].as_str().unwrap_or("").to_string(), text)
        })
        .collect()
}
