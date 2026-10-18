//! `takeover=ask` (§2.8, C21): a free thread binds at once; a held one gets
//! `lmgw.session.bind_pending {held_by}`, every other event an `error`
//! `bind_pending`; `lmgw.session.take_over` takes the thread; the holder's
//! end lets the call bind by itself; a new holder brings another
//! `bind_pending`; a hangup ends the wait, and so does its bound, also for a
//! client that stopped taking what lmgw sends.

use futures::StreamExt;
use lmgw_core::realtime::calls::CallState;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{calls_world, post_sdp, Phone};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::common::patience::{until, within};
use crate::device_chat::pair;
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::{next_event, Ws};

/// A device's session bound to `tid`.
pub(super) async fn holder(w: &World, tid: i64, name: &str) -> Ws {
    let d = pair(w, name, json!({})).await;
    let mut ws = w
        .connect_as(&d, &format!("chat_thread={tid}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("{s}: {b}"));
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    ws
}

/// Read `ws` on its own task, answering its pings, until its close: the
/// close's code and reason.
fn drained(mut ws: Ws) -> tokio::task::JoinHandle<(u16, String)> {
    tokio::spawn(async move {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(Some(c)))) => {
                    return (u16::from(c.code), c.reason.to_string())
                }
                Some(Ok(_)) => continue,
                other => panic!("the holder's session ended without its close: {other:?}"),
            }
        }
    })
}

/// A call bound to `tid` with `takeover=ask` by the dashboard, connected:
/// its first event.
async fn ask(w: &World, tid: i64) -> (Phone, Value) {
    let mut p = Phone::connected(
        w,
        &w.gw.client(),
        &format!("chat_thread={tid}&takeover=ask"),
    )
    .await;
    let first = p.next_but_states().await;
    (p, first)
}

#[tokio::test]
async fn a_free_thread_binds_at_once() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (_p, first) = ask(&w, tid).await;
    assert_eq!(first["type"], "session.created", "{first}");
}

#[tokio::test]
async fn a_held_thread_is_asked_about_and_take_over_takes_it() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let ws = holder(&w, tid, "desktop").await;
    let holder = drained(ws);
    let (mut p, first) = ask(&w, tid).await;
    assert_eq!(
        first,
        json!({"type": "lmgw.session.bind_pending", "event_id": first["event_id"],
               "held_by": "device 'desktop'"})
    );
    let info = w.state.calls.get(&p.id).unwrap();
    assert_eq!(
        (info.state, info.held_by.as_deref()),
        (CallState::Pending, Some("device 'desktop'"))
    );
    // Anything else waits for the answer, refused by name.
    p.send(json!({"type": "response.create", "event_id": "evt_mine"}));
    let refused = p.next().await;
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(refused["error"]["code"], "bind_pending");
    assert_eq!(refused["error"]["event_id"], "evt_mine");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'response.create'"),
        "{refused}"
    );

    p.send(json!({"type": "lmgw.session.take_over"}));
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        tid
    );
    assert_eq!(
        within("the holder's close", holder).await.unwrap(),
        (4000, "voice mode moved to the dashboard".into())
    );
    assert_eq!(w.state.calls.get(&p.id).unwrap().state, CallState::Running);
    // A take_over that comes after the bind is ignored: the session answers
    // the next event as ever.
    p.send(json!({"type": "lmgw.session.take_over"}));
    p.send(json!({"type": "session.update", "session": {"type": "realtime"}}));
    let next = p.next_but_states().await;
    assert_eq!(next["type"], "session.updated", "{next}");
}

#[tokio::test]
async fn the_holders_end_lets_the_call_bind_and_a_new_holder_is_named() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let desktop = holder(&w, tid, "desktop").await;
    let (mut p, first) = ask(&w, tid).await;
    assert_eq!(first["held_by"], "device 'desktop'", "{first}");
    // A third client takes the thread from the holder: asked again, about it.
    let desktop = drained(desktop);
    let tablet = holder(&w, tid, "tablet").await;
    assert_eq!(
        within("the desktop's close", desktop).await.unwrap().0,
        4000
    );
    let again = p.next_but_states().await;
    assert_eq!(
        (&again["type"], &again["held_by"]),
        (
            &json!("lmgw.session.bind_pending"),
            &json!("device 'tablet'")
        ),
        "{again}"
    );
    // The holder leaves: the call binds with no word from the client.
    drop(tablet);
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        tid
    );
}

#[tokio::test]
async fn a_hangup_ends_the_wait_and_says_who() {
    let w = calls_world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let _desktop = drained(holder(&w, tid, "desktop").await);
    let (mut p, first) = ask(&w, tid).await;
    assert_eq!(first["type"], "lmgw.session.bind_pending");
    assert_eq!(p.hangup(&w, &w.gw.client()).await, 200);
    let events = p.closed().await;
    let closed = events.last().unwrap();
    assert_eq!(
        (&closed["type"], &closed["kind"], &closed["by"]),
        (
            &json!("lmgw.session.closed"),
            &json!("closed"),
            &json!("the dashboard")
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn the_wait_ends_after_one_ping_interval_naming_the_holder() {
    let w = calls_world(|s| s.realtime.ping_interval_s = 1).await;
    let tid = w.thread("chatty", json!({})).await;
    let _desktop = drained(holder(&w, tid, "desktop").await);
    let (mut p, first) = ask(&w, tid).await;
    assert_eq!(first["type"], "lmgw.session.bind_pending");
    let events = p.closed().await;
    let closed = events.last().unwrap();
    assert_eq!(
        (&closed["type"], &closed["kind"], &closed["by"]),
        (
            &json!("lmgw.session.closed"),
            &json!("thread_bound"),
            &json!("device 'desktop'")
        ),
        "{events:?}"
    );
    assert!(
        closed["message"]
            .as_str()
            .unwrap()
            .contains("realtime.ping_interval_s"),
        "{closed}"
    );
    // The wait has acted, and the holder still holds the thread, not ending:
    // a takeover=never call is refused by it.
    let (_c, sdp) = Phone::offer().await;
    let query = format!("chat_thread={tid}&takeover=never");
    let a = post_sdp(&w, &w.gw.client(), &query, &sdp).await;
    assert_eq!(a.status, 409, "{}", a.body);
    assert_eq!(
        a.json()["error"]["held_by"],
        "device 'desktop'",
        "{}",
        a.body
    );
}

/// The pending wait's sends never hold up its bounds (§2.8): a client that
/// sent events whose answers fill lmgw's send window, and then took nothing
/// more (no SCTP acknowledgement), is still ended at the wait's bound. The
/// folder's lock holds the open's bind back until the events are in, so the
/// answers all come after the client stopped.
#[tokio::test]
async fn a_client_that_stops_taking_while_pending_still_meets_the_bound() {
    let w = calls_world(|s| s.realtime.ping_interval_s = 1).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &owner, folder, false).await;
    let _desktop = drained(holder(&w, tid, "desktop").await);
    let query = format!("chat_folder={folder}&takeover=ask");
    let mut p = Phone::call(&w, &owner, &query).await;
    let lock = w.state.hold_chat_folder_for_tests(folder).await;
    p.c.connect().await;
    // Each answer quotes its event's type: three of 100 000 bytes are more
    // than str0m's 128 KiB window.
    for _ in 0..3 {
        p.send(json!({"type": "t".repeat(100_000)}));
        p.c.until_acknowledged().await;
    }
    // The client is frozen from here: nothing drives it.
    drop(lock);
    until("the pending call ended at its bound", || {
        w.state.calls.get(&p.id).is_none()
    })
    .await;
}
