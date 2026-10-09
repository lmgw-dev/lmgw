//! A device approves only within its own reach (client-apps design §6.6,
//! review finding 1): the resumed turn runs as its starter, whose reach may
//! be wider, so a call beyond the decider's tool scope — or one of lmgw's
//! admin tools while its own may not do everything — is refused, and
//! nothing of the batch is decided. Declining stays open.

use serde_json::{json, Value};

use super::{approval_frames, attach, both_calls, decide, no_call, send, tool_results};
use crate::device_chat::{chat_thread, op, pair, post};
use crate::mcp_host::{host_world, linked};
use crate::realtime_chat_thread::{next, of_type, settings, until_type, World};
use crate::support::realtime_fakes::{send as ws_send, Turn};
use crate::support::realtime_mcp::calls;

/// The last reply's waiting calls, as the thread's read lists them.
async fn waiting(w: &World, tid: i64) -> Value {
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    v["messages"].as_array().unwrap().last().unwrap()["pending_approvals"].clone()
}

/// A device whose key's tool scope leaves `desktop__notify` out cannot
/// approve the owner's gated call to it — nothing is decided and nothing
/// runs — but may decline it.
#[tokio::test]
async fn a_device_approves_only_what_its_own_tool_scope_admits() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let phone = pair(&w, "phone", json!({})).await;
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": phone.id, "tool_scope_mode": "deny",
               "tool_scope_patterns": "desktop__notify"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();

    let (s, r) = decide(
        &w,
        &phone.client,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 403, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_out_of_scope", "{r:?}");
    let message = r[0].1["message"].as_str().unwrap();
    assert!(message.contains("desktop__notify"), "{message}");
    assert!(message.contains("phone"), "{message}");
    assert_eq!(waiting(&w, tid).await[0]["approval_request_id"], id);
    no_call(&mut dev).await;

    // Declining is open to anyone who sees the thread.
    w.chat.push(Turn::text(&["Fine."]));
    let (s, frames) = decide(
        &w,
        &phone.client,
        tid,
        json!([{"approval_request_id": id, "approve": false, "reason": "not now"}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let only = dev.next_call().await;
    assert_eq!(only["params"]["name"], "echo", "the sibling ran: {only}");
    no_call(&mut dev).await;
    assert!(
        tool_results(&w, 1)[1]
            .1
            .contains("The user declined this tool call: not now"),
        "{:?}",
        tool_results(&w, 1)
    );
}

/// The owner's thread with lmgw's admin tools, every one gated, stopped on
/// `lmgw__status`: the thread and the call's approval id.
async fn gated_admin_turn(w: &World) -> (i64, Value) {
    let owner = w.gw.client();
    let tid = chat_thread(w, &owner, "chatty").await;
    let (s, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "lmgw", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat
        .push(calls(&[(0, "call_1", "lmgw__status", "{}")], "tool_calls"));
    let frames = send(w, &owner, tid, "how are you").await;
    let asks = approval_frames(&frames);
    assert_eq!(asks.len(), 1, "{frames:?}");
    (tid, asks[0]["approval_request_id"].clone())
}

/// An lmgw admin call runs at the starter's level, the owner's `full`
/// here: a device whose own admin tools are `read_only` cannot approve it,
/// one whose are `full` can.
#[tokio::test]
async fn an_admin_call_needs_a_device_whose_admin_tools_may_do_everything() {
    let (w, _d) = host_world().await;
    settings(&w.state, |s| {
        s.self_admin = lmgw_core::config::SelfAdmin::Full;
    })
    .await;
    let ro = pair(&w, "reader", json!({ "self_admin": "read_only" })).await;
    let rw = pair(&w, "writer", json!({ "self_admin": "full" })).await;
    let (tid, id) = gated_admin_turn(&w).await;

    let (s, r) = decide(
        &w,
        &ro.client,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 403, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_out_of_scope", "{r:?}");
    assert!(
        r[0].1["message"].as_str().unwrap().contains("lmgw__status"),
        "{r:?}"
    );
    assert_eq!(waiting(&w, tid).await[0]["approval_request_id"], id);

    w.chat.push(Turn::text(&["All well."]));
    let (s, frames) = decide(
        &w,
        &rw.client,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let results = tool_results(&w, 1);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        !results[0].1.contains("declined"),
        "the call ran: {results:?}"
    );
}

/// A bound session decides through the same code: its answer approving a
/// call beyond its device's reach fails the response with the route's
/// code, and nothing is decided.
#[tokio::test]
async fn a_bound_session_approving_beyond_its_reach_is_refused() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let phone = pair(&w, "phone", json!({})).await;
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": phone.id, "tool_scope_mode": "deny",
               "tool_scope_patterns": "desktop__notify"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();

    let bearer = format!("Bearer {}", phone.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    ws_send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    ws_send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_approval_response", "approval_request_id": id, "approve": true}}),
    )
    .await;
    ws_send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "failed", "{done}");
    assert_eq!(
        done["response"]["status_details"]["error"]["code"], "approval_out_of_scope",
        "{done}"
    );
    assert_eq!(waiting(&w, tid).await[0]["approval_request_id"], id);
    no_call(&mut dev).await;
}
