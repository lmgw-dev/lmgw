//! The admin-tools switch on the surfaces beside the Chat's threads (the
//! pre-merge review's P-6, P-8 and P-10, decided by the owner 2026-10-07):
//!
//! - a switched device's own tool scope narrows the `lmgw__*` tools where
//!   it names them, and an attach (at `full`) it leaves none of them is
//!   refused;
//! - the self-admin level bounds a switched device's turn as the owner's;
//! - `/v1/mcp/servers`, `/v1/responses` and an unbound realtime session
//!   offer the toolset with the switch, and refuse it without — and the
//!   realtime session lists it again the moment the switch moves;
//! - a switch-off cancels the device's own turn on a toolset thread, and a
//!   call made after the key's row changed is refused before the snapshot
//!   caught up; a disabled or expired device is allowed nothing.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{chat_thread, frames, get, op, pair, post, self_admin_thread, sse, Device};
use crate::realtime_chat_thread::{next, until, world, World};
use crate::support::realtime_fakes::{send, Step, Turn};

/// Attach `lmgw` to `tid` as `d`: the status and body.
async fn attach(w: &World, d: &Device, tid: i64) -> (u16, Value) {
    post(
        w,
        &d.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await
}

/// Attach `lmgw` to `tid` as the owner, `200`: below `full` a device uses a
/// thread with the toolset and does not attach it itself.
async fn owner_attach(w: &World, tid: i64) {
    let (s, v) = post(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// The model's next answer calls `name` once, then says "done".
fn calls(w: &World, name: &'static str, args: &'static str) {
    w.chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name,
        },
        Step::CallArgs { index: 0, args },
        Step::Finish("tool_calls"),
        Step::Usage(10, 5),
    ]));
    w.chat.push(Turn::text(&["done."]));
}

/// The tool result the model was handed in request `n`.
fn tool_result(w: &World, n: usize) -> String {
    let body = w.chat.seen.chat(n);
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool result in {body}"))["content"]
        .to_string()
}

/// The tool names offered in request `n`.
fn offered(w: &World, n: usize) -> Vec<String> {
    w.chat.seen.chat(n)["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Review P-6: the switch grants the label, and the device's own patterns
/// narrow it where they name `lmgw__`; a list that leaves none of its tools
/// refuses the attach, saying so.
#[tokio::test]
async fn a_switched_device_s_own_patterns_narrow_the_admin_tools() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let narrow = pair(
        &w,
        "desktop",
        json!({ "self_admin": "read_only", "tool_scope_mode": "allow",
                "tool_scope_patterns": "docs__*\nlmgw__status" }),
    )
    .await;
    let tid = chat_thread(&w, &narrow.client, "chatty").await;
    owner_attach(&w, tid).await;
    w.chat.push(Turn::text(&["fine."]));
    let before = w.chat.seen.chat_count();
    let (s, _) = sse(
        &w,
        &narrow.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "how is lmgw?" }),
    )
    .await;
    assert_eq!(s, 200);
    let names = offered(&w, before);
    let admin: Vec<&String> = names.iter().filter(|n| n.starts_with("lmgw__")).collect();
    assert_eq!(admin, ["lmgw__status"], "{names:?}");

    for (name, mode, patterns) in [
        ("laptop", "allow", "docs__*\nlmgw__no_such_tool"),
        ("tablet", "deny", "lmgw__*"),
    ] {
        // At full, where a device attaches the toolset itself (V-4).
        let d = pair(
            &w,
            name,
            json!({ "self_admin": "full", "tool_scope_mode": mode,
                    "tool_scope_patterns": patterns }),
        )
        .await;
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let (s, v) = attach(&w, &d, tid).await;
        assert_eq!(
            (s, &v["code"]),
            (403, &json!("tool_label_out_of_scope")),
            "{name}: {v}"
        );
        let message = v["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("none of lmgw's admin tools are within the tool scope of"),
            "{name}: {message}"
        );
    }
}

/// Review P-10: the self-admin level bounds a switched device's turn as it
/// bounds the owner's — at `read_only`, a write tool is not offered, and a
/// call the model makes to it anyway is not run: the turn ends with it, and
/// nothing changed.
#[tokio::test]
async fn a_switched_device_at_read_only_cannot_write() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::ReadOnly).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tid = chat_thread(&w, &desk.client, "chatty").await;
    owner_attach(&w, tid).await;
    let hold_before = w.state.snapshot().settings.hold.active;
    calls(&w, "lmgw__hold_set", r#"{"active": true}"#);
    let before = w.chat.seen.chat_count();
    let (s, said) = sse(
        &w,
        &desk.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hold the GPU" }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(said.iter().any(|(e, _)| e == "done"), "{said:?}");
    let names = offered(&w, before);
    assert!(
        names.iter().any(|n| n == "lmgw__status") && !names.iter().any(|n| n == "lmgw__hold_set"),
        "the reads, and no write tool, at read_only: {names:?}"
    );
    assert_eq!(
        w.chat.seen.chat_count(),
        before + 1,
        "the call was not run, so the model was not asked again: {said:#?}"
    );
    assert_eq!(w.state.snapshot().settings.hold.active, hold_before);
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT requested_alias FROM request_logs WHERE key_id = ?1 AND ingress_proto = \
         'admin-tool'",
    )
    .bind(desk.id)
    .fetch_optional(&w.state.db)
    .await
    .unwrap();
    assert_eq!(row, None, "no admin tool ran");
}

/// Review P-10: `/v1/mcp/servers` lists `lmgw` to a switched device and
/// to no other; `/v1/responses` offers its tools to the one and refuses the
/// label to the other.
#[tokio::test]
async fn mcp_servers_and_responses_follow_the_switch() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let labels = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .or_else(|| v["servers"].as_array())
            .unwrap_or_else(|| panic!("no list in {v}"))
            .iter()
            .filter_map(|s| s["server_label"].as_str().map(str::to_string))
            .collect()
    };
    let (s, v) = get(&w, &desk.client, "/v1/mcp/servers").await;
    assert_eq!(s, 200, "{v}");
    assert!(labels(&v).contains(&"lmgw".to_string()), "{v}");
    let (s, v) = get(&w, &phone.client, "/v1/mcp/servers").await;
    assert_eq!(s, 200, "{v}");
    assert!(!labels(&v).contains(&"lmgw".to_string()), "{v}");
    let (s, v) = get(&w, &phone.client, "/v1/mcp/servers/lmgw").await;
    assert_eq!(s, 404, "{v}");

    let body = json!({
        "model": "chatty",
        "input": "status?",
        "stream": true,
        "tools": [{ "type": "mcp", "server_label": "lmgw", "require_approval": "never" }],
    });
    for (d, allowed) in [(&desk, true), (&phone, false)] {
        let before = w.chat.seen.chat_count();
        let text = d
            .client
            .post(format!("{}/v1/responses", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            w.chat.seen.chat_count() > before,
            "the model was asked: {text}"
        );
        let names = offered(&w, before);
        assert_eq!(
            names.iter().any(|n| n == "lmgw__status"),
            allowed,
            "{allowed}: {names:?} {text}"
        );
        assert_eq!(
            text.contains("not allowed lmgw's admin tools"),
            !allowed,
            "{text}"
        );
    }
}

/// Review P-8 and P-10: an unbound realtime session of a switched device
/// lists `lmgw`; turned off, the session lists it again at once and the
/// listing fails, saying why; turned on, it lists the tools again.
#[tokio::test]
async fn an_unbound_realtime_session_lists_lmgw_again_as_the_switch_moves() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let bearer = format!("Bearer {}", desk.key);
    let mut ws = w
        .connect("model=chatty", &[("authorization", bearer.as_str())])
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "tools": [{"type": "mcp", "server_label": "lmgw"}]}}),
    )
    .await;
    let listed = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    assert!(
        listed
            .iter()
            .any(|e| e["type"] == "mcp_list_tools.completed"),
        "{listed:#?}"
    );

    let (s, _) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": "off" })).await;
    assert_eq!(s, 200);
    let again = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    assert!(
        again.iter().any(|e| e["type"] == "mcp_list_tools.failed"),
        "{again:#?}"
    );
    // Then the reason, as for every failed listing.
    let why = until(&mut ws, |e| e["type"] == "error").await;
    let why = why.last().unwrap();
    assert!(
        why["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not allowed lmgw's admin tools"),
        "{why}"
    );
    let done = again.last().unwrap();
    assert_eq!(done["item"]["tools"], json!([]), "{done}");

    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200);
    let back = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    assert!(
        back.iter().any(|e| e["type"] == "mcp_list_tools.completed"),
        "{back:#?}"
    );
}

/// Review P-8: turned off, the device's own turn on a toolset thread stops
/// at once — its stream says so, its upstream request is dropped, and its
/// partial reply is not saved onto a thread it no longer reaches.
#[tokio::test]
async fn a_switch_off_stops_the_device_s_turn_on_a_toolset_thread() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" more."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    let mut resp = desk
        .client
        .post(format!("{}/chat/api/threads/{}/send", w.gw, tools.id))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut got = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got.contains("Thinking") {
        let c = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the reply begins")
            .unwrap()
            .expect("not ended");
        got.push_str(&String::from_utf8_lossy(&c));
    }

    let (s, _) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": "off" })).await;
    assert_eq!(s, 200);
    let rest = tokio::time::timeout(Duration::from_secs(10), resp.text())
        .await
        .expect("the stream ends")
        .unwrap();
    let said = frames(&rest);
    assert!(
        said.iter()
            .any(|(e, d)| e == "error" && d["code"] == "superseded"),
        "{rest}"
    );
    assert!(!rest.contains(" more."), "{rest}");
    let dropped = tokio::time::timeout(Duration::from_secs(5), async {
        while w.chat.seen.closed_early.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(dropped.is_ok(), "the upstream request was never dropped");
    hold.notify_one();
    let (_, v) = get(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{}", tools.id),
    )
    .await;
    let replies: Vec<&Value> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert!(
        replies.iter().all(|m| !m["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Thinking")),
        "nothing of it saved: {v}"
    );
}

/// Review P-8: a switched device's `lmgw__*` call is checked against its
/// key row as well as the snapshot — one made after the row was switched
/// off or disabled, before the snapshot caught up, is refused — and a
/// disabled or expired device is allowed nothing.
#[tokio::test]
async fn a_call_after_the_row_changed_is_refused_before_the_snapshot_catches_up() {
    for write in [
        "UPDATE api_keys SET self_admin = 0 WHERE id = ?1",
        "UPDATE api_keys SET enabled = 0 WHERE id = ?1",
    ] {
        let w = world(|_| {}).await;
        let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
        let tid = chat_thread(&w, &desk.client, "chatty").await;
        owner_attach(&w, tid).await;
        // The commit a key write makes, without its snapshot reload.
        sqlx::query(write)
            .bind(desk.id)
            .execute(&w.state.db)
            .await
            .unwrap();
        calls(&w, "lmgw__status", "{}");
        let before = w.chat.seen.chat_count();
        let (s, _) = sse(
            &w,
            &desk.client,
            &format!("/chat/api/threads/{tid}/send"),
            json!({ "content": "how is lmgw?" }),
        )
        .await;
        assert_eq!(s, 200);
        let result = tool_result(&w, before + 1);
        assert!(
            result.contains("is no longer allowed lmgw's admin tools"),
            "{write}: {result}"
        );
    }

    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    assert!(lmgw_core::devices::self_admin(&w.state.snapshot(), desk.id).is_on());
    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "expires_at": "2020-01-01" }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(
        !lmgw_core::devices::self_admin(&w.state.snapshot(), desk.id).is_on(),
        "expired"
    );
    let laptop = pair(&w, "laptop", json!({ "self_admin": "read_only" })).await;
    let (s, _) = op(&w, "key_set", json!({ "id": laptop.id, "enabled": false })).await;
    assert_eq!(s, 200);
    assert!(
        !lmgw_core::devices::self_admin(&w.state.snapshot(), laptop.id).is_on(),
        "disabled"
    );
}
