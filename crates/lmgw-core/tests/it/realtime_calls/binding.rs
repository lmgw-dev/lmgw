//! The bind when `oai-events` opens (§2.8, C21): the POST takes nothing
//! over; the open binds by `takeover`, taking the thread then; a thread
//! held at the open under `never`, out of reach, a policy refusal and a key
//! revoked since the POST each end the call with `lmgw.session.closed`
//! before any session; a hangup or a listener's stop before the open closes
//! the call quietly, and a running bound call's ends say why.

use std::time::Duration;

use futures::StreamExt;
use lmgw_core::realtime::calls::Group;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{calls_world, key_client, Phone};
use crate::common::patience::within;
use crate::device_chat::{op, pair, post};
use crate::realtime_calls_net::devices::devices_listening;
use crate::realtime_chat_thread::World;
use crate::support::device_pair::Paired;
use crate::support::realtime_fakes::{next_event, send, Ws};

/// The WebSocket session's close: code and reason, the events before it
/// read past.
pub(super) async fn ws_close(ws: &mut Ws) -> (u16, String) {
    within("the WebSocket's close", async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(Some(c)))) => {
                    return (u16::from(c.code), c.reason.to_string())
                }
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await
}

/// A bound WebSocket session on `tid` as the paired device `device`.
async fn device_ws(w: &World, tid: i64, device: &Paired) -> Ws {
    let mut ws = w
        .connect_as(device, &format!("chat_thread={tid}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("{s}: {b}"));
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    ws
}

/// The `lmgw.session.closed` that ends `events`, which must be the last.
fn closed_of(events: &[Value]) -> &Value {
    let last = events
        .last()
        .unwrap_or_else(|| panic!("no event before the close"));
    assert_eq!(last["type"], "lmgw.session.closed", "{events:?}");
    assert!(
        !events.iter().any(|e| e["type"] == "session.created"),
        "no session ran: {events:?}"
    );
    last
}

#[tokio::test]
async fn the_post_takes_nothing_over_and_the_open_does() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (mut ws, _) = w.bind(tid).await;
    let (_, phone) = key_client(&w, "owner", "phone", json!({})).await;
    let mut p = Phone::call(&w, &phone, &format!("chat_thread={tid}")).await;
    // Answered, and the dashboard's session runs on.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let ev = crate::realtime_chat_thread::next(&mut ws).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
    // The channel opens: the call takes the thread.
    p.c.connect().await;
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        tid
    );
    assert_eq!(created["session"]["lmgw"]["turn_mode"], "automatic");
    assert_eq!(
        ws_close(&mut ws).await,
        (4000, "voice mode moved to the dashboard".into())
    );
}

#[tokio::test]
async fn a_thread_taken_from_a_bound_call_says_who_took_it() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut p = Phone::connected(&w, &w.gw.client(), &format!("chat_thread={tid}")).await;
    assert_eq!(p.next_but_states().await["type"], "session.created");
    let phone = pair(&w, "phone", json!({})).await;
    let _ws = device_ws(&w, tid, &phone).await;
    let events = p.closed().await;
    let closed = events.last().unwrap();
    assert_eq!(
        (&closed["type"], &closed["kind"], &closed["by"]),
        (
            &json!("lmgw.session.closed"),
            &json!("taken_over"),
            &json!("device 'phone'")
        ),
        "{events:?}"
    );
    assert_eq!(closed["message"], "voice mode moved to device 'phone'");
    assert!(events
        .iter()
        .any(|e| e["error"]["code"] == "chat_thread_taken_over"));
}

#[tokio::test]
async fn never_ends_the_call_when_the_thread_is_held_at_the_open() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (_, phone) = key_client(&w, "owner", "phone", json!({})).await;
    // Free at the POST: answered.
    let mut p = Phone::call(&w, &phone, &format!("chat_thread={tid}&takeover=never")).await;
    // Held by the time the channel opens.
    let (mut ws, _) = w.bind(tid).await;
    p.c.connect().await;
    let events = p.closed().await;
    let closed = closed_of(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"]),
        (&json!("thread_bound"), &json!("the dashboard"))
    );
    assert!(
        closed["message"]
            .as_str()
            .unwrap()
            .contains("takeover=never"),
        "{closed}"
    );
    let read: lmgw_api_types::realtime::SessionClosed =
        serde_json::from_value(closed.clone()).unwrap();
    assert_eq!(
        lmgw_client::realtime::call_close(&read).kind,
        lmgw_client::realtime::CloseKind::ThreadBound
    );
    // The holder runs on.
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let ev = crate::realtime_chat_thread::next(&mut ws).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
}

#[tokio::test]
async fn a_thread_out_of_reach_at_the_open_ends_the_call_so() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (_, phone) = key_client(&w, "owner", "phone", json!({})).await;
    let mut p = Phone::call(&w, &phone, &format!("chat_thread={tid}")).await;
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    p.c.connect().await;
    let events = p.closed().await;
    let closed = closed_of(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"]),
        (&json!("out_of_reach"), &Value::Null),
        "{events:?}"
    );
}

#[tokio::test]
async fn another_refusal_at_the_open_is_its_error_then_closed() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let folder = crate::chat_ongoing::ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    // No current thread: the POST decides on a new one, and makes nothing.
    let mut p = Phone::call(&w, &owner, &format!("chat_folder={folder}")).await;
    sqlx::query("UPDATE chat_folders SET defaults = '{}' WHERE id = ?1")
        .bind(folder)
        .execute(&w.state.db)
        .await
        .unwrap();
    p.c.connect().await;
    let events = p.closed().await;
    let closed = closed_of(&events);
    assert_eq!(closed["kind"], "closed", "{events:?}");
    let error = &events[events.len() - 2];
    assert_eq!(
        (&error["type"], &error["error"]["code"]),
        (&json!("error"), &json!("folder_no_model")),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_key_revoked_between_the_post_and_the_open_ends_the_call_revoked() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (id, phone) = key_client(&w, "owner", "phone", json!({})).await;
    let mut p = Phone::call(&w, &phone, &format!("chat_thread={tid}")).await;
    let (s, v) = op(&w, "key_set", json!({ "id": id, "enabled": false })).await;
    assert_eq!(s, 200, "{v}");
    p.c.connect().await;
    let events = p.closed().await;
    let closed = closed_of(&events);
    assert_eq!(closed["kind"], "revoked", "{events:?}");
    assert_eq!(closed["message"], "revoked: owner key 'phone' was disabled");
    let read: lmgw_api_types::realtime::SessionClosed =
        serde_json::from_value(closed.clone()).unwrap();
    assert_eq!(
        lmgw_client::realtime::call_close(&read).kind,
        lmgw_client::realtime::CloseKind::Revoked {
            // A non-device key's kind; a device's is the next test's.
            kind: lmgw_client::realtime::RevokeKind::Revoked
        }
    );
    // Nothing was bound: another key's takeover=never bind is not refused.
    let cookie = w.cookie();
    let bound = w
        .connect(
            &format!("chat_thread={tid}&takeover=never"),
            &[("cookie", &cookie)],
        )
        .await;
    assert!(bound.is_ok(), "{:?}", bound.err());
}

/// The device form: a paired device calls through the device listener (the
/// device socket group, calls WP7) and is disabled before the open; its
/// call's last event is `revoked`, of the kind `device_disabled`.
#[tokio::test]
async fn a_device_disabled_between_the_post_and_the_open_ends_the_call_device_disabled() {
    let w = calls_world(|_| {}).await;
    devices_listening(&w.state).await;
    let tid = w.thread("chatty", json!({})).await;
    let phone = pair(&w, "phone", json!({})).await;
    let mut p = Phone::call(&w, &phone.client, &format!("chat_thread={tid}")).await;
    assert_eq!(w.state.calls.get(&p.id).unwrap().group, Group::Devices);
    let (s, v) = op(&w, "key_set", json!({ "id": phone.id, "enabled": false })).await;
    assert_eq!(s, 200, "{v}");
    p.c.connect().await;
    let events = p.closed().await;
    let closed = closed_of(&events);
    assert_eq!(closed["kind"], "revoked", "{events:?}");
    assert_eq!(
        closed["message"],
        "device_disabled: device 'phone' was disabled"
    );
    let read: lmgw_api_types::realtime::SessionClosed =
        serde_json::from_value(closed.clone()).unwrap();
    assert_eq!(
        lmgw_client::realtime::call_close(&read).kind,
        lmgw_client::realtime::CloseKind::Revoked {
            kind: lmgw_client::realtime::RevokeKind::DeviceDisabled
        }
    );
}

#[tokio::test]
async fn a_hangup_or_a_stop_before_the_open_ends_the_call_quietly() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    let p = Phone::call(&w, &owner, &format!("chat_thread={tid}")).await;
    assert_eq!(p.hangup(&w, &owner).await, 200);
    assert!(w.state.calls.get(&p.id).is_none());

    // A stop of the listener's group: the open path ends its call when
    // asked, so the close does not wait out its bound.
    let q = Phone::call(&w, &owner, &format!("chat_thread={tid}")).await;
    within(
        "the group's close",
        w.state
            .calls
            .close_group_within(Group::Plain, Duration::from_secs(24 * 3600)),
    )
    .await;
    assert!(w.state.calls.get(&q.id).is_none());
}

#[tokio::test]
async fn a_running_bound_call_hears_its_listener_stop() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut p = Phone::connected(&w, &w.gw.client(), &format!("chat_thread={tid}")).await;
    assert_eq!(p.next_but_states().await["type"], "session.created");
    let calls = w.state.calls.clone();
    let closing = tokio::spawn(async move {
        calls
            .close_group_within(Group::Plain, Duration::from_secs(24 * 3600))
            .await
    });
    let events = p.closed().await;
    let closed = events.last().unwrap();
    assert_eq!(
        (&closed["type"], &closed["kind"]),
        (&json!("lmgw.session.closed"), &json!("shutting_down")),
        "{events:?}"
    );
    within("the group's close", closing).await.unwrap();
}
