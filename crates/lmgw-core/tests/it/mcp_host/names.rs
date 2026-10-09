//! Whose a name is (review findings 2 and 3): a device's by the server that
//! serves it, not by its spelling — a bare server's own `desktop__x` is the
//! bare server's, for an `all` caller and against the hosting device alike;
//! hosting labels and tool prefixes whose namespaces run into each other are
//! refused; and an `all` scope is narrowed for a foreign device row only, so
//! its other servers — a bare one not connected — stay listed and writable
//! without connecting anything.

use serde_json::{json, Value};

use super::{host_world, linked, mcp_rpc, mcp_session, mcp_tools};
use crate::device_chat::{chat_thread, get, op, pair, post};
use crate::support::mcp_stub::{register, stub};

/// A bare server `bare` (no prefix) whose own tool is named `desktop__x`.
async fn bare_with_a_device_spelled_tool(w: &super::World) -> crate::support::mcp_stub::McpStub {
    let s = stub(
        json!([{"name": "desktop__x", "description": "a bare server's own tool",
                "inputSchema": {"type": "object"}}]),
        false,
    )
    .await;
    register(&w.state, "bare", "", &s.url, true, None).await;
    w.state.mcp.list_tools(&w.state.snapshot()).await;
    s
}

#[tokio::test]
async fn a_name_is_the_device_s_by_who_serves_it() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let bare = bare_with_a_device_spelled_tool(&w).await;

    // Anonymous: the bare server's tool, not the device's.
    let anonymous = reqwest::Client::new();
    let names = mcp_tools(&w, &anonymous).await;
    assert!(names.contains(&"desktop__x".to_string()), "{names:?}");
    assert!(!names.contains(&"desktop__echo".to_string()), "{names:?}");
    let sid = mcp_session(&w, &anonymous).await;
    let v = mcp_rpc(
        &w,
        &anonymous,
        &sid,
        "tools/call",
        json!({"name": "desktop__x", "arguments": {"text": "hi"}}),
    )
    .await;
    assert!(v["result"].is_object(), "{v}");
    bare.wait_calls(1).await;
    assert!(dev.calls.try_recv().is_err(), "the device was called");

    // The hosting device, its own list naming nothing of the bare server:
    // its own tools whole, the bare server's `desktop__x` not.
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": d.id, "tool_scope_mode": "allow", "tool_scope_patterns": "zzz__*"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let names = mcp_tools(&w, &d.client).await;
    assert!(names.contains(&"desktop__echo".to_string()), "{names:?}");
    assert!(!names.contains(&"desktop__x".to_string()), "{names:?}");
}

/// A label ending in `_`, a label running into a server's prefix, and a
/// prefix running into a device's label are refused, naming the other.
#[tokio::test]
async fn namespaces_that_run_into_each_other_are_refused() {
    let (w, _d) = host_world().await;
    for prefix in ["desktop_", "desktop__x", "DESKTOP"] {
        let (s, v) = op(
            &w,
            "mcp_server_set",
            json!({"action": "create", "name": format!("s-{}", prefix.len()),
                   "transport": "http", "url": "http://127.0.0.1:1/mcp",
                   "tool_prefix": prefix}),
        )
        .await;
        assert_ne!(s, 200, "{prefix}: {v}");
        assert!(
            v.to_string().contains("device 'desktop''s hosting label"),
            "{prefix}: {v}"
        );
    }
    let (s, v) = op(
        &w,
        "mcp_server_set",
        json!({"action": "create", "name": "web", "transport": "http",
               "url": "http://127.0.0.1:1/mcp", "tool_prefix": "web_"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let phone = pair(&w, "phone", json!({})).await;
    for (label, word) in [
        ("tab_", "cannot end in '_'"),
        ("web", "'web'"),
        ("desktop", "one label, one device"),
    ] {
        let (s, v) = op(&w, "key_set", json!({"id": phone.id, "hosts_label": label})).await;
        assert_ne!(s, 200, "{label}: {v}");
        assert!(v.to_string().contains(word), "{label}: {v}");
    }
}

/// With a device row on the gateway, an `all` caller still sees a bare
/// server that lists nothing yet, and an `all` device writes its label
/// without lmgw connecting it (review finding 3) — while another device's
/// label stays refused.
#[tokio::test]
async fn an_all_scope_is_narrowed_for_a_foreign_device_row_only() {
    let (w, _d) = host_world().await;
    let quiet = stub(json!([]), false).await;
    register(&w.state, "quiet", "", &quiet.url, true, None).await;
    let before = quiet.hits();

    let (s, v) = get(&w, &reqwest::Client::new(), "/v1/mcp/servers").await;
    assert_eq!(s, 200, "{v}");
    let labels: Vec<&str> = v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .filter_map(|e| e["server_label"].as_str())
        .collect();
    assert!(labels.contains(&"quiet"), "{labels:?}");
    assert!(!labels.contains(&"desktop"), "{labels:?}");

    let phone = pair(&w, "phone", json!({})).await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "quiet"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(quiet.hits(), before, "the write connected the server");
    let (s, v): (u16, Value) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("tool_label_out_of_scope")),
        "{v}"
    );
}
