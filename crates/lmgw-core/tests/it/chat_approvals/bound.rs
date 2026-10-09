//! A session bound to the thread (client-apps design §6.4): OpenAI's
//! approval items, and a decision made elsewhere.

use serde_json::{json, Value};

use super::{approval_frames, attach, both_calls, decide, send as chat_send};
use crate::device_chat::{chat_thread, op, pair, post, rows_of};
use crate::mcp_host::{host_world, linked};
use crate::realtime_chat_thread::{next, of_type, say, until_type};
use crate::support::realtime_fakes::Ws;
use crate::support::realtime_fakes::{send, Turn};
use crate::support::realtime_mcp::{calls, tools_session};

/// A bound text session with manual turns, its gated turn spoken: the
/// events up to its `response.done`.
async fn gated_voice_turn(w: &crate::realtime_chat_thread::World, tid: i64) -> (Ws, Vec<Value>) {
    let (mut ws, _) = w.bind(tid).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    both_calls(w);
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    (ws, events)
}

/// The request item, by its `.done`.
fn request_item(events: &[Value]) -> Value {
    of_type(events, "conversation.item.done")
        .into_iter()
        .find(|e| e["item"]["type"] == "mcp_approval_request")
        .map(|e| e["item"].clone())
        .unwrap_or_else(|| panic!("no mcp_approval_request in {events:#?}"))
}

/// A bound turn's gated call is an `mcp_approval_request` item and its
/// response ends; the client's `mcp_approval_response` and
/// `response.create` resume the thread's turn, the session's principal the
/// approver, and the approved call and the answer come in that response.
#[tokio::test]
async fn a_bound_session_shows_the_item_and_its_answer_resumes_the_turn() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    attach(&w, &owner, tid).await;
    let (mut ws, events) = gated_voice_turn(&w, tid).await;
    let item = request_item(&events);
    let id = item["id"].as_str().unwrap().to_string();
    assert!(
        id.starts_with("mcpr_"),
        "the item's id is the approval id: {item}"
    );
    assert_eq!(item["server_label"], "desktop");
    assert_eq!(item["name"], "notify");
    assert_eq!(item["arguments"], r#"{"text":"hi"}"#);
    // OpenAI's item, nothing added: the call's id is on the
    // `lmgw.chat.frame` relaying the turn's `approval` frame, after it.
    assert_eq!(
        item.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["arguments", "id", "name", "server_label", "type"],
        "{item}"
    );
    let at = |pred: &dyn Fn(&Value) -> bool| events.iter().position(pred);
    let item_at = at(&|e| e["type"] == "conversation.item.done" && e["item"]["id"] == id.as_str());
    let frame_at = at(&|e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "tool" && e["data"]["event"] == "approval"
    })
    .unwrap_or_else(|| panic!("no approval frame: {events:#?}"));
    let frame = &events[frame_at]["data"];
    assert_eq!(frame["approval_request_id"], id.as_str(), "{frame}");
    assert_eq!(frame["call_id"], "call_2", "{frame}");
    assert!(item_at.is_some_and(|i| i < frame_at), "{events:#?}");
    let ready = events
        .iter()
        .find(|e| {
            e["type"] == "lmgw.chat.frame"
                && e["data"]["event"] == "ready"
                && e["data"]["name"] == "desktop__notify"
        })
        .unwrap_or_else(|| panic!("no ready frame: {events:#?}"));
    assert_eq!(ready["data"]["call_id"], frame["call_id"]);
    assert!(
        of_type(&events, "conversation.item.added")
            .iter()
            .any(|e| e["item"]["id"] == id.as_str()),
        "{events:#?}"
    );
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "completed", "{done}");

    w.chat.push(Turn::text(&["Done."]));
    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_approval_response", "approval_request_id": id, "approve": true}}),
    )
    .await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    assert!(
        of_type(&events, "conversation.item.done")
            .iter()
            .any(|e| e["item"]["type"] == "mcp_approval_response"),
        "the answer is echoed: {events:#?}"
    );
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "completed", "{done}");
    let text = done["response"].to_string();
    assert!(text.contains("Done."), "{done}");
    let notify = loop {
        let c = dev.next_call().await;
        if c["params"]["name"] == "notify" {
            break c;
        }
    };
    assert_eq!(
        notify["params"]["_meta"]["lmgw/approval"],
        json!({"decision": "approved", "by": {"kind": "owner", "name": "dashboard"}}),
        "{notify}"
    );
    // One reply in the thread, the answer appended to it.
    let msgs = w.messages(tid).await;
    assert_eq!(msgs.last().unwrap().0, "assistant");
    assert!(msgs.last().unwrap().1.contains("Done."), "{msgs:?}");
}

/// A decision made on another client while the session's request item is
/// open: the session says `lmgw.approval.decided`, naming who.
#[tokio::test]
async fn a_decision_made_elsewhere_is_said_to_the_bound_session() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    attach(&w, &owner, tid).await;
    let (mut ws, events) = gated_voice_turn(&w, tid).await;
    let id = request_item(&events)["id"].clone();

    w.chat.push(Turn::text(&["Done."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": false, "reason": "no"}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let events = until_type(&mut ws, "lmgw.approval.decided").await;
    let said = events.last().unwrap();
    assert_eq!(said["approval_request_id"], id, "{said}");
    assert_eq!(said["approve"], false);
    assert_eq!(said["by"], "the dashboard");
}

/// An answer on a session bound to no thread is refused by name.
#[tokio::test]
async fn an_unbound_session_refuses_an_approval_answer() {
    let (w, _d) = host_world().await;
    let mut ws =
        crate::support::realtime_mcp::tools_session(&w.addr(), Some(&w.gw.key), json!([]), 0).await;
    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_approval_response", "approval_request_id": "mcpr_1", "approve": true}}),
    )
    .await;
    let ev = until_type(&mut ws, "error").await;
    let e = &ev.last().unwrap()["error"];
    assert_eq!(e["code"], "invalid_value", "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("bound to a chat thread"),
        "{e}"
    );
}

/// The gated reply's id: the thread's last message.
async fn last_id(w: &crate::realtime_chat_thread::World, tid: i64) -> i64 {
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    v["messages"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Deleting or editing the gated reply while a session shows its call: the
/// session hears it, as `lmgw.approval.decided` with `approve: false` and
/// `by: null` — nobody decided it, and it never runs.
#[tokio::test]
async fn a_gated_reply_deleted_or_edited_is_said_to_the_bound_session() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    for action in ["delete", "edit"] {
        let tid = w.thread("chatty", json!({})).await;
        attach(&w, &owner, tid).await;
        let (mut ws, events) = gated_voice_turn(&w, tid).await;
        let id = request_item(&events)["id"].clone();
        let reply = last_id(&w, tid).await;
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/messages/{reply}/{action}"),
            json!({"content": "something else"}),
        )
        .await;
        assert_eq!(s, 200, "{action}: {v}");
        let events = until_type(&mut ws, "lmgw.approval.decided").await;
        let said = events.last().unwrap();
        assert_eq!(said["approval_request_id"], id, "{action}: {said}");
        assert_eq!(said["approve"], false, "{action}: {said}");
        assert_eq!(said["by"], Value::Null, "{action}: {said}");
    }
}

/// A resume through a bound session runs as its starter and holds the
/// starter's concurrency slot, as a send of that key's would: while the
/// starter's one slot is taken, the session's answer fails with the key's
/// `key_rate` and nothing is decided — as the route's does.
#[tokio::test]
async fn a_resume_through_a_bound_session_takes_its_starters_slot() {
    let (w, _d) = host_world().await;
    crate::realtime_chat_thread::settings(&w.state, |s| {
        s.self_admin = lmgw_core::config::SelfAdmin::Full;
    })
    .await;
    let phone = pair(&w, "phone", json!({ "self_admin": "full" })).await;
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": phone.id, "concurrency_limit": 1}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "lmgw", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat
        .push(calls(&[(0, "call_1", "lmgw__status", "{}")], "tool_calls"));
    let frames = chat_send(&w, &phone.client, tid, "how are you").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();

    // The phone's one slot, held by a session of its own (realtime §10.3).
    let held = tools_session(&w.addr(), Some(&phone.key), json!([]), 0).await;
    let (mut ws, _) = w.bind(tid).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_approval_response", "approval_request_id": id, "approve": true}}),
    )
    .await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "failed", "{done}");
    assert_eq!(
        done["response"]["status_details"]["error"]["code"], "key_rate",
        "{done}"
    );
    let owner = w.gw.client();
    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 429, "the route takes it too: {r:?}");
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let last = v["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["pending_approvals"][0]["approval_request_id"], id,
        "nothing decided: {v}"
    );

    // Once the slot is free, the answer resumes the turn as the phone.
    drop(held);
    let before = rows_of(&w, phone.id).await.len();
    w.chat.push(Turn::text(&["All well."]));
    crate::common::patience::until_async("the phone's slot frees", || async {
        let r = phone
            .client
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap();
        r.status().as_u16() != 429
    })
    .await;
    send(
        &mut ws,
        json!({"type": "conversation.item.create", "item": {
            "type": "mcp_approval_response", "approval_request_id": id, "approve": true}}),
    )
    .await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "completed", "{done}");
    assert!(
        rows_of(&w, phone.id).await.len() > before,
        "the resumed turn's model call is the phone's"
    );
}
