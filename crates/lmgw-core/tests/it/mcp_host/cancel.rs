//! lmgw cancels every forwarded call it stops waiting on (§5.4, R14): at the
//! timeout, when a turn is cancelled, before it closes the link; and a link
//! the device drops mid-call reports the call as abandoned.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use super::{host_world, linked, mcp_rpc, mcp_session, set_timeout, World};
use crate::common::patience;
use crate::device_chat::{chat_thread, op, post};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

/// A `/mcp` call of `desktop__echo` as the owner, on a task of its own: its
/// answer, whole, as text.
fn call_in_background(w: &World) -> tokio::task::JoinHandle<String> {
    let (gw, client) = (w.gw.to_string(), w.gw.client());
    tokio::spawn(async move {
        let sid = {
            let resp = client
                .post(format!("{gw}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                               "clientInfo": {"name": "t", "version": "0"}}}))
                .send()
                .await
                .unwrap();
            let sid = resp.headers()["mcp-session-id"]
                .to_str()
                .unwrap()
                .to_string();
            let _ = resp.text().await;
            sid
        };
        client
            .post(format!("{gw}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", sid)
            .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                          "params": {"name": "desktop__echo"}}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
}

#[tokio::test]
async fn a_timeout_cancels_the_call_on_the_device() {
    let (w, d) = host_world().await;
    set_timeout(&w, d.id, 300).await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.auto.store(false, Ordering::SeqCst);
    let owner = w.gw.client();
    let sid = mcp_session(&w, &owner).await;
    let v = mcp_rpc(
        &w,
        &owner,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert!(v.to_string().contains("timed out after 300ms"), "{v}");
    let call = dev.next_call().await;
    assert_eq!(call["params"]["_meta"]["lmgw/timeout_ms"], 300, "{call}");
    let note = dev.next_note("notifications/cancelled").await;
    assert_eq!(note["params"]["requestId"], call["id"], "{note}");
    assert!(
        note["params"]["reason"]
            .as_str()
            .unwrap()
            .contains("timeout_ms"),
        "{note}"
    );
}

/// A Chat turn's cancel — a new message in its thread — cancels the call
/// its tool loop was waiting on.
#[tokio::test]
async fn a_turn_s_cancel_cancels_the_call_on_the_device() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.auto.store(false, Ordering::SeqCst);
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat
        .push(calls(&[(0, "call_1", "desktop__echo", "{}")], "tool_calls"));
    let first = {
        let (gw, owner) = (w.gw.to_string(), owner.clone());
        tokio::spawn(async move {
            owner
                .post(format!("{gw}/chat/api/threads/{tid}/send"))
                .json(&json!({"content": "look"}))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };
    let call = dev.next_call().await;
    assert_eq!(
        call["params"]["_meta"]["lmgw/caller"]["kind"], "owner",
        "the turn's starter: {call}"
    );
    // A new message cancels the running turn.
    w.chat.push(Turn::text(&["Fine."]));
    let (s, _) = crate::device_chat::sse(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "never mind"}),
    )
    .await;
    assert_eq!(s, 200);
    let note = dev.next_note("notifications/cancelled").await;
    assert_eq!(note["params"]["requestId"], call["id"], "{note}");
    let _ = patience::within("the first turn's stream ends", first).await;
}

/// A link lmgw closes — a revocation here — cancels every call open on it
/// first, and the call reports why lmgw closed it, not that the device
/// disconnected.
#[tokio::test]
async fn a_link_lmgw_closes_cancels_its_open_calls_first() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.auto.store(false, Ordering::SeqCst);
    let pending = call_in_background(&w);
    let call = dev.next_call().await;
    let (s, v) = op(&w, "key_set", json!({"id": d.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (code, _) = dev.closed().await;
    assert_eq!(code, 4003);
    // Sent before the close frame, so already read.
    let note: Value = loop {
        match dev.notes.try_recv() {
            Ok(n) if n["method"] == "notifications/cancelled" => break n,
            Ok(_) => continue,
            Err(e) => panic!("no cancel before the close: {e:?}"),
        }
    };
    assert_eq!(note["params"]["requestId"], call["id"], "{note}");
    let answer = patience::within("the call's answer", pending)
        .await
        .unwrap();
    assert!(
        answer.contains("lmgw closed device 'desktop''s host link while the call ran")
            && answer.contains("device_disabled: ")
            && answer.contains("may or may not have run"),
        "{answer}"
    );
}

/// A takeover mid-call: the call on the older link says it was taken over.
#[tokio::test]
async fn a_takeover_mid_call_says_so() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.auto.store(false, Ordering::SeqCst);
    let pending = call_in_background(&w);
    dev.next_call().await;
    let _newer = super::FakeDevice::connect(&w.addr(), &d.key, vec![super::tool("echo")]).await;
    let (code, _) = dev.closed().await;
    assert_eq!(code, 4000);
    let answer = patience::within("the call's answer", pending)
        .await
        .unwrap();
    assert!(
        answer.contains("another connection of device 'desktop' took over")
            && !answer.contains("disconnected while"),
        "{answer}"
    );
}

/// A link the device drops mid-call: the call is abandoned, in the Chat's
/// words, so the model does not retry blindly.
#[tokio::test]
async fn a_drop_mid_call_is_reported_as_abandoned() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.auto.store(false, Ordering::SeqCst);
    let pending = call_in_background(&w);
    dev.next_call().await;
    dev.vanish();
    let answer = patience::within("the call's answer", pending)
        .await
        .unwrap();
    assert!(
        answer.contains("device 'desktop' disconnected while the call ran")
            && answer.contains("may or may not have run"),
        "{answer}"
    );
}
