//! The link itself (§5.1–§5.3, §5.5): the handshake and the row's status, a
//! call with `_meta`, a takeover, the refusals before the 101, the limits,
//! a revocation, sampling, and the device's own `list_changed`.

use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{device_row, host_world, linked, mcp_rpc, mcp_session, mcp_tools, status, tool};
use super::{FakeDevice, World};
use crate::common::patience;
use crate::device_chat::{op, pair};

/// The grant made a `device` row: the label its prefix, the key's name its
/// name, never reaped, no sampling; offline until the device connects.
#[tokio::test]
async fn the_grant_makes_the_row_and_it_waits_offline() {
    let (w, d) = host_world().await;
    let row = device_row(&w, d.id);
    assert_eq!(row.transport.as_str(), "device");
    assert_eq!(
        (row.name.as_str(), row.tool_prefix.as_str()),
        ("device:desktop", "desktop")
    );
    assert_eq!(
        (row.idle_seconds, row.allow_sampling, row.timeout_ms),
        (0, false, 60_000)
    );
    let v = status(&w, d.id).await;
    assert_eq!(v["status"], "stopped", "{v}");
    assert_eq!(v["device"], "desktop", "{v}");
    assert_eq!(v["transport"], "device", "{v}");
    assert!(
        v["status_detail"]
            .as_str()
            .is_some_and(|s| s.starts_with("device offline")),
        "{v}"
    );

    // The grant renamed, cleared, and the key deleted: the row follows.
    let (s, r) = op(&w, "key_set", json!({"id": d.id, "hosts_label": "desk"})).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(device_row(&w, d.id).tool_prefix, "desk");
    let (s, r) = op(&w, "key_set", json!({"id": d.id, "hosts_label": ""})).await;
    assert_eq!(s, 200, "{r}");
    assert!(w
        .state
        .snapshot()
        .mcp_servers
        .values()
        .all(|s| s.device_key_id.is_none()));
    let (s, r) = op(&w, "key_set", json!({"id": d.id, "hosts_label": "desktop"})).await;
    assert_eq!(s, 200, "{r}");
    let (s, r) = op(&w, "key_delete", json!({"id": d.id})).await;
    assert_eq!(s, 200, "{r}");
    assert!(w
        .state
        .snapshot()
        .mcp_servers
        .values()
        .all(|s| s.device_key_id.is_none()));
}

/// The row is the grant's: its prefix, name and transport are not the MCP
/// page's to change, it is not deleted by hand, and it never samples
/// (§5.2, R26). Its timeout and its switch are the page's.
#[tokio::test]
async fn the_row_is_the_grant_s_and_never_samples() {
    let (w, d) = host_world().await;
    let id = device_row(&w, d.id).id;
    for (patch, word) in [
        (json!({"allow_sampling": true}), "sample"),
        (json!({"tool_prefix": "other"}), "hosting label"),
        (json!({"name": "renamed"}), "renamed"),
        (json!({"url": "http://127.0.0.1:1/mcp"}), "own link"),
    ] {
        let mut body = json!({"action": "update", "id": id});
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, v) = op(&w, "mcp_server_set", body).await;
        assert_ne!(s, 200, "{v}");
        assert!(v.to_string().contains(word), "{word}: {v}");
    }
    let (s, v) = op(&w, "mcp_server_set", json!({"action": "delete", "id": id})).await;
    assert_ne!(s, 200, "{v}");
    let (s, v) = op(
        &w,
        "mcp_server_set",
        json!({"action": "create", "name": "mine", "transport": "device"}),
    )
    .await;
    assert_ne!(s, 200, "{v}");
    let (s, v) = op(
        &w,
        "mcp_server_set",
        json!({"action": "create", "name": "device:x", "transport": "http",
               "url": "http://127.0.0.1:1/mcp"}),
    )
    .await;
    assert_ne!(s, 200, "{v}");
    // The page's whole form sent back as it reads is no change.
    let row = status(&w, d.id).await;
    let (s, v) = op(
        &w,
        "mcp_server_set",
        json!({"action": "update", "id": id, "name": row["name"], "transport": "device",
               "tool_prefix": "desktop", "url": "", "command": "", "headers": "", "env": "",
               "args": "", "extra_run_args": "", "idle_seconds": 0, "autostart": false,
               "allow_sampling": false, "timeout_ms": 1234}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(device_row(&w, d.id).timeout_ms, 1234);

    // A sampling request from the device is refused -32601, with why.
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.send_json(
        json!({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage",
        "params": {"messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}],
                   "maxTokens": 8}}),
    );
    let answer = patience::within("the sampling answer", dev.answers.recv())
        .await
        .unwrap();
    assert_eq!(answer["error"]["code"], -32601, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside the device's own key"),
        "{answer}"
    );
}

/// `initialize`, `notifications/initialized` and the list (§5.1): the row is
/// `Ready`, the tools are in the aggregate under the label, the device is
/// online with a tools link; on disconnect they leave again (§5.3).
#[tokio::test]
async fn initialize_and_listing_make_the_row_ready() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "see_screen"]).await;
    dev.next_note("notifications/initialized").await;
    let v = status(&w, d.id).await;
    assert_eq!(
        (v["status"].as_str(), v["tool_count"].as_i64()),
        (Some("ready"), Some(2)),
        "{v}"
    );
    let names = mcp_tools(&w, &w.gw.client()).await;
    assert!(names.contains(&"desktop__echo".to_string()), "{names:?}");
    assert!(
        names.contains(&"desktop__see_screen".to_string()),
        "{names:?}"
    );
    let (_, keys) = crate::device_chat::get(&w, &w.gw.client(), "/api/usage/keys").await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == d.id)
        .unwrap()
        .clone();
    assert!(
        row["online"].as_array().unwrap().contains(&json!("tools")),
        "{row}"
    );
    assert_eq!(row["reaches"][0], "owner", "{row}");
    assert_eq!(row["reaches"][1], "device 'desktop' (hosts it)", "{row}");

    // Its own `list_changed`: lmgw lists again.
    dev.change_tools(vec![tool("echo"), tool("see_screen"), tool("type_text")]);
    patience::until_async("the new tool is listed", || async {
        mcp_tools(&w, &w.gw.client())
            .await
            .contains(&"desktop__type_text".to_string())
    })
    .await;

    dev.vanish();
    let id = device_row(&w, d.id).id;
    patience::until_async("the row is offline again", || async {
        !w.state.mcp.is_ready(id).await
    })
    .await;
    let names = mcp_tools(&w, &w.gw.client()).await;
    assert!(
        !names.iter().any(|n| n.starts_with("desktop__")),
        "{names:?}"
    );
    let v = status(&w, d.id).await;
    assert_eq!(v["status"], "stopped", "{v}");
    assert!(
        v["status_detail"]
            .as_str()
            .is_some_and(|s| s.starts_with("device offline (last seen")),
        "{v}"
    );
}

/// `initialize` tells the device the link's real limits in its `_meta`
/// (`lmgw/host_limits`, §5.1), from the effective settings and the row.
#[tokio::test]
async fn initialize_tells_the_device_its_limits() {
    use lmgw_api_types::mcp_host::{HostLimits, META_HOST_LIMITS};
    let (w, d) = host_world().await;
    crate::realtime_chat_thread::settings(&w.state, |s| {
        s.mcp.host_max_message_mb = 8;
        s.mcp.host_max_frame_mb = 2;
        s.mcp.host_ping_interval_s = 7;
    })
    .await;
    super::set_timeout(&w, d.id, 4_321).await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let init = patience::within("the initialize", dev.inits.recv())
        .await
        .unwrap();
    let meta = init["params"]["_meta"]
        .as_object()
        .unwrap_or_else(|| panic!("no _meta: {init}"));
    assert!(meta.contains_key(META_HOST_LIMITS), "{init}");
    assert_eq!(
        HostLimits::from_meta(meta),
        Some(HostLimits {
            max_message_bytes: Some(8 << 20),
            max_frame_bytes: Some(2 << 20),
            ping_interval_s: Some(7),
            call_timeout_ms: Some(4_321),
        }),
        "{init}"
    );
}

/// Every forwarded call carries `_meta` (§5.5): who it runs as — the owner
/// on `/mcp` with the dashboard's key, the device itself with its own — no
/// approval, and the row's timeout.
#[tokio::test]
async fn a_call_carries_meta() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let owner = w.gw.client();
    let sid = mcp_session(&w, &owner).await;
    let v = mcp_rpc(
        &w,
        &owner,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo", "arguments": {"text": "hi"}}),
    )
    .await;
    assert_eq!(v["result"]["content"][0]["text"], "ran echo", "{v}");
    let call = dev.next_call().await;
    assert_eq!(call["params"]["name"], "echo", "{call}");
    assert_eq!(call["params"]["arguments"]["text"], "hi", "{call}");
    let meta = &call["params"]["_meta"];
    assert_eq!(
        meta["lmgw/caller"],
        json!({"kind": "owner", "name": "dashboard"}),
        "{call}"
    );
    assert_eq!(meta["lmgw/approval"], Value::Null, "{call}");
    assert_eq!(meta["lmgw/timeout_ms"], 60_000, "{call}");

    // The hosting device calls its own tools as itself.
    let sid = mcp_session(&w, &d.client).await;
    let v = mcp_rpc(
        &w,
        &d.client,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert_eq!(v["result"]["content"][0]["text"], "ran echo", "{v}");
    let call = dev.next_call().await;
    assert_eq!(
        call["params"]["_meta"]["lmgw/caller"],
        json!({"kind": "device", "name": "desktop"}),
        "{call}"
    );
}

/// A second link of the same device takes over (§5.3): the older one closes
/// 4000 naming the device, and the newer one serves.
#[tokio::test]
async fn a_second_link_takes_over() {
    let (w, d) = host_world().await;
    let mut first = linked(&w, &d, &["echo"]).await;
    let _second = FakeDevice::connect(&w.addr(), &d.key, vec![tool("echo"), tool("other")]).await;
    let (code, reason) = first.closed().await;
    assert_eq!(
        (code, reason.as_str()),
        (4000, "another connection of device 'desktop' took over")
    );
    patience::until_async("the newer link lists", || async {
        mcp_tools(&w, &w.gw.client())
            .await
            .contains(&"desktop__other".to_string())
    })
    .await;
}

/// The refusals before the 101 (§5.1, §1.8): an `Origin` header, a device
/// without a grant, an owner key; and a client key is no Chat principal.
#[tokio::test]
async fn the_refusals_come_before_the_upgrade() {
    let (w, d) = host_world().await;
    let auth = format!("Bearer {}", d.key);
    let (s, v) = super::device::refused(
        &w.addr(),
        &[("authorization", &auth), ("origin", "http://evil.example")],
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("cross_origin_refused")),
        "{v}"
    );

    let plain = pair(&w, "phone", json!({})).await;
    let auth = format!("Bearer {}", plain.key);
    let (s, v) = super::device::refused(&w.addr(), &[("authorization", &auth)]).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("host_not_granted")),
        "{v}"
    );

    let auth = format!("Bearer {}", w.gw.key);
    let (s, v) = super::device::refused(&w.addr(), &[("authorization", &auth)]).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("host_not_granted")),
        "{v}"
    );

    let (s, v) = super::device::refused(&w.addr(), &[]).await;
    assert_eq!(s, 401, "{v}");
}

/// A frame over the limits closes the link 1009 naming the setting (§5.1).
#[tokio::test]
async fn an_overrun_closes_naming_the_setting() {
    let (w, d) = host_world().await;
    crate::realtime_chat_thread::settings(&w.state, |s| {
        s.mcp.host_max_message_mb = 1;
        s.mcp.host_max_frame_mb = 0;
    })
    .await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.send(Message::Text("x".repeat(2 * 1024 * 1024).into()));
    let (code, reason) = dev.closed().await;
    assert_eq!(code, 1009, "{reason}");
    assert!(reason.contains("mcp.host_max_message_mb"), "{reason}");
}

/// Revocation closes the link 4003 with the device's token (§1.6, L18).
#[tokio::test]
async fn a_revocation_closes_the_link_4003() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let (s, v) = op(&w, "key_set", json!({"id": d.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (code, reason) = dev.closed().await;
    assert_eq!(code, 4003, "{reason}");
    assert!(reason.starts_with("device_disabled: "), "{reason}");
}

/// A world's device row, whatever its state, for a test that only reads it.
#[allow(dead_code)]
fn row_id(w: &World, key_id: i64) -> i64 {
    device_row(w, key_id).id
}
