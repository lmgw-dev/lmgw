//! A device's level of lmgw's admin tools (the owner's decision of
//! 2026-10-07 on the pre-merge review's P-3): `off`, `read_only` or `full`,
//! capped by the gateway's own self-admin level. A device below `full`
//! reaches no write tool — among them every tool that registers a program
//! this machine runs — and lowering its level narrows it at once.
//!
//! The calls are made through the executors a device's turn runs its tools
//! with (`ScopedExecutor` over `SelfAdminExecutor`, as the device): a model
//! is never offered a write tool below `full`, so only a direct call reaches
//! the refusal behind that.

use lmgw_core::agent::ToolExecutor;
use lmgw_core::config::{ApiKeyKind, SelfAdmin};
use lmgw_core::ir::ToolResultBlock;
use lmgw_core::mcp::exec::SelfAdminExecutor;
use lmgw_core::mcp::scope::ScopedExecutor;
use lmgw_core::principal::Principal;
use lmgw_core::proxy::RequestCtx;
use serde_json::{json, Value};

use super::{chat_thread, op, pair, post, sse, Device};
use crate::chat_feed::Feed;
use crate::realtime_chat_thread::{next, until, world, World};
use crate::support::realtime_fakes::{send, Turn};

/// `name(args)` called as device `d`, as its turn calls it: whether it was
/// refused, and what it said.
pub(super) async fn call_as(w: &World, d: &Device, name: &str, args: Value) -> (bool, String) {
    let key = w
        .state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == d.id)
        .cloned()
        .expect("the device's row");
    let ctx = RequestCtx {
        principal: Principal::Key {
            id: d.id,
            name: key.name.clone(),
            kind: ApiKeyKind::Device,
            agent_id: None,
            fingerprint: key.fingerprint(),
        },
        ..Default::default()
    };
    let exec = ScopedExecutor::new(
        SelfAdminExecutor::new(w.state.clone(), ctx.clone()),
        w.state.clone(),
        ctx,
    );
    let out = exec.call(name, &args).await;
    let text = out
        .blocks
        .iter()
        .map(|b| match b {
            ToolResultBlock::Text { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (out.is_error, text)
}

/// The `lmgw__*` tools device `d`'s turn on a thread with the toolset is
/// offered. The owner attaches it to the device's thread: below `full` a
/// device does not attach it itself.
async fn offered(w: &World, d: &Device) -> Vec<String> {
    let tid = chat_thread(w, &d.client, "chatty").await;
    let (s, v) = post(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat.push(Turn::text(&["fine."]));
    let before = w.chat.seen.chat_count();
    let (s, _) = sse(
        w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "how is lmgw?" }),
    )
    .await;
    assert_eq!(s, 200);
    w.chat.seen.chat(before)["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .filter(|n| n.starts_with("lmgw__"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// At `read_only`, under a gateway at `full`: the reads, never a write —
/// not offered, and refused when called, naming the device's level.
#[tokio::test]
async fn a_read_only_device_cannot_run_a_mutating_tool() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let names = offered(&w, &desk).await;
    assert!(names.contains(&"lmgw__status".to_string()), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n == "lmgw__hold_set" || n == "lmgw__settings_set"),
        "{names:?}"
    );
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused, "{said}");
    assert!(
        said.contains("this device's admin tools are read only"),
        "{said}"
    );
    assert!(!w.state.snapshot().settings.hold.active, "nothing changed");
    let (refused, said) = call_as(&w, &desk, "lmgw__status", json!({})).await;
    assert!(!refused, "a read runs: {said}");

    // The same device at full writes.
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "full" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": false })).await;
    assert!(!refused, "{said}");
}

/// A device at `full` under a gateway at `read_only` reads only: the
/// gateway's level caps it.
#[tokio::test]
async fn full_under_a_global_read_only_is_still_read_only() {
    let w = world(|s| s.self_admin = SelfAdmin::ReadOnly).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let names = offered(&w, &desk).await;
    assert!(names.contains(&"lmgw__status".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n == "lmgw__hold_set"), "{names:?}");
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused && said.contains("read_only"), "{said}");
    assert!(!w.state.snapshot().settings.hold.active);
}

/// Registering a program this machine runs — an MCP server with a stdio
/// command — needs `full`: a read-only device is refused, and no server is
/// registered.
#[tokio::test]
async fn a_stdio_mcp_server_from_a_read_only_device_is_refused() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let (refused, said) = call_as(
        &w,
        &desk,
        "lmgw__mcp_server_set",
        json!({ "action": "create", "name": "runner", "transport": "stdio",
                "command": "sh", "args": "-c\ntouch /tmp/lmgw-must-not-exist",
                "autostart": false }),
    )
    .await;
    assert!(refused, "{said}");
    assert!(
        said.contains("this device's admin tools are read only"),
        "{said}"
    );
    assert!(
        !w.state
            .snapshot()
            .mcp_servers
            .values()
            .any(|s| s.name == "runner"),
        "nothing registered"
    );
}

/// Lowering the level narrows the device at once: its unbound realtime
/// session lists `lmgw` again without the write tools, its feed's `state`
/// says the level (and no thread leaves it: read only still sees them), a
/// write call is refused — and so is one made after the row changed, before
/// the snapshot caught up.
#[tokio::test]
async fn lowering_the_level_mid_session_narrows_the_device_at_once() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let mut feed = Feed::open(&w, &desk.client, "", None).await;
    assert_eq!(feed.hello()["self_admin"], json!("full"));
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
    let tools = |events: &[Value]| -> Vec<String> {
        events.last().unwrap()["item"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    };
    let first = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    let names = tools(&first);
    assert!(names.contains(&"mcp_server_set".to_string()), "{names:?}");

    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let again = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    assert!(
        again
            .iter()
            .any(|e| e["type"] == "mcp_list_tools.completed"),
        "{again:#?}"
    );
    let names = tools(&again);
    assert!(names.contains(&"status".to_string()), "{names:?}");
    assert!(!names.contains(&"mcp_server_set".to_string()), "{names:?}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "state" && f.data["self_admin"] == json!("read_only"))
    })
    .await;
    assert!(
        feed.frames.iter().all(|f| f.event != "thread.deleted"),
        "read only still sees the toolset's threads: {:#?}",
        feed.frames
    );
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused && said.contains("read only"), "{said}");

    // Full again, then lowered by a commit the snapshot has not caught up
    // with: the row decides.
    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "full" }),
    )
    .await;
    assert_eq!(s, 200);
    sqlx::query("UPDATE api_keys SET self_admin = 1 WHERE id = ?1")
        .bind(desk.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused && said.contains("read only"), "{said}");
    assert!(!w.state.snapshot().settings.hold.active);
    // The table holds the level to the three it knows.
    let bad = sqlx::query("UPDATE api_keys SET self_admin = 7 WHERE id = ?1")
        .bind(desk.id)
        .execute(&w.state.db)
        .await;
    assert!(bad.is_err(), "{bad:?}");
}

/// The key ops take the level by name, and refuse another word for it.
#[tokio::test]
async fn the_key_ops_take_the_level_by_name() {
    let w = world(|_| {}).await;
    let (s, v) = op(
        &w,
        "key_create",
        json!({ "kind": "device", "name": "tablet", "self_admin": "root" }),
    )
    .await;
    assert!(s >= 400, "{v}");
    assert!(v.to_string().contains("off|read_only|full"), "{v}");
    let desk = pair(&w, "desktop", json!({})).await;
    let (s, v) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": true })).await;
    assert!(s >= 400, "a switch is no level: {v}");
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "full" }),
    )
    .await;
    assert_eq!((s, &v["changed"]), (200, &json!(["self_admin"])), "{v}");
    let (_, keys) = super::get(&w, &w.gw.client(), "/api/usage/keys").await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == desk.id)
        .cloned()
        .unwrap();
    assert_eq!(row["self_admin"], json!("full"));
}
