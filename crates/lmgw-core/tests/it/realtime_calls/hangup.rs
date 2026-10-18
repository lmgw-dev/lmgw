//! The hangup (§2.6, C3): the session ends as if the client had closed, a
//! bound call's last event `lmgw.session.closed {kind: "closed", by}`; the
//! 200 comes once the call let go, so a `takeover=never` call right after it
//! is not refused by the call it ended; no slot is taken, so a key at its
//! limit can hang up, and the slot is back after; only the call's key and
//! the owner may, everyone else gets the 404 of an ended call. A hangup
//! whose owner never reacts is `realtime_calls_net::ends`' (the paused
//! clock, `Calls::hangup_within`). A hangup that lands while the call binds
//! at its open lets the binding that comes back go, and no session starts.

use lmgw_core::realtime::calls::CallState;
use serde_json::json;

use super::{anonymous, calls_world, hangup, key_client, post_sdp, Phone};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::common::captured_log::capture_log_at;
use crate::common::patience::{until, within};

#[tokio::test]
async fn a_bound_call_hung_up_says_so_and_a_never_call_right_after_binds() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let tid = w.thread("chatty", json!({})).await;
    let mut p = Phone::connected(&w, &owner, &format!("chat_thread={tid}")).await;
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        tid
    );
    let info = w.state.calls.get(&p.id).unwrap();
    assert_eq!(info.thread, Some(tid));
    // Created first, so the call below follows the 200 at once.
    let (_, other) = key_client(&w, "owner", "phone", json!({})).await;

    assert_eq!(p.hangup(&w, &owner).await, 200);
    // Let go once the 200 is here: closing until the phone acknowledged its
    // last event (calls design §8.5), or gone.
    let state = w.state.calls.get(&p.id).map(|i| i.state);
    assert!(state.is_none_or(|s| s == CallState::Closing), "{state:?}");
    // Right after the 200, another key's takeover=never call is not
    // refused by the call it ended, and binds at its open.
    let mut q = Phone::connected(&w, &other, &format!("chat_thread={tid}&takeover=never")).await;
    let created = q.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");

    let last = p.closed().await;
    let closed = last
        .last()
        .unwrap_or_else(|| panic!("no event before the close"));
    assert_eq!(closed["type"], "lmgw.session.closed", "{last:?}");
    assert_eq!(closed["kind"], "closed");
    assert_eq!(closed["by"], "the dashboard");
    // What lmgw-client makes of it.
    let read: lmgw_api_types::realtime::SessionClosed =
        serde_json::from_value(closed.clone()).unwrap();
    let close = lmgw_client::realtime::call_close(&read);
    assert_eq!(close.by.as_deref(), Some("the dashboard"));
}

#[tokio::test]
async fn only_the_calls_key_and_the_owner_may_hang_it_up() {
    let w = calls_world(|_| {}).await;
    let (_, phone) = key_client(&w, "client", "phone", json!({})).await;
    let (_, tablet) = key_client(&w, "client", "tablet", json!({})).await;
    let p = Phone::call(&w, &phone, "model=chatty").await;
    assert_eq!(
        hangup(&w, &w.gw.client(), "rtc_000000000000000000000000").await,
        404
    );
    // Another key, and no key at all: the 404 of a call that is not
    // there, so neither learns that it is.
    assert_eq!(p.hangup(&w, &tablet).await, 404);
    assert_eq!(p.hangup(&w, &anonymous()).await, 404);
    assert!(w.state.calls.get(&p.id).is_some());
    // Its own key may; so may the owner, for another call.
    assert_eq!(p.hangup(&w, &phone).await, 200);
    assert!(w.state.calls.get(&p.id).is_none());
    assert_eq!(p.hangup(&w, &phone).await, 404, "ended already");
    let q = Phone::call(&w, &phone, "model=chatty").await;
    assert_eq!(q.hangup(&w, &w.gw.client()).await, 200);
    assert!(w.state.calls.list().is_empty());
}

#[tokio::test]
async fn a_key_at_its_limit_can_hang_up_and_its_slot_comes_back() {
    let w = calls_world(|_| {}).await;
    let (_, phone) = key_client(&w, "client", "phone", json!({ "concurrency_limit": 1 })).await;
    let p = Phone::call(&w, &phone, "model=chatty").await;
    // The call holds the key's one slot from its POST.
    let (_c, sdp) = Phone::offer().await;
    let second = post_sdp(&w, &phone, "model=chatty", &sdp).await;
    assert_eq!(second.status, 429, "{}", second.body);
    // The hangup takes none, and once it answers the slot is back.
    assert_eq!(p.hangup(&w, &phone).await, 200);
    let third = post_sdp(&w, &phone, "model=chatty", &sdp).await;
    assert_eq!(third.status, 201, "{}", third.body);
}

#[tokio::test]
async fn a_call_that_never_opens_gives_its_slot_back_at_the_bound() {
    let w = calls_world(|s| s.realtime.ping_interval_s = 1).await;
    let (_, phone) = key_client(&w, "client", "phone", json!({ "concurrency_limit": 1 })).await;
    // Answered, never connected: the driver ends it one interval on.
    let p = Phone::call(&w, &phone, "model=chatty").await;
    let (_c, sdp) = Phone::offer().await;
    crate::common::patience::until_async("the never-opened call's slot back", || {
        let (w, phone, sdp, id) = (&w, &phone, &sdp, &p.id);
        async move {
            if w.state.calls.get(id).is_some() {
                return false;
            }
            let a = post_sdp(w, phone, "model=chatty", sdp).await;
            assert_eq!(a.status, 201, "{}", a.body);
            true
        }
    })
    .await;
}

/// A hangup that lands while the open's bind runs (§2.8, review F-5): the
/// folder's lock holds the bind inside `thread::check_and_bind` until the
/// hangup is in. The binding that comes back is let go before the call does,
/// so nothing holds the thread once it let go; no session starts, and the
/// call's last event is the hangup's.
#[tokio::test]
async fn a_hangup_while_the_call_binds_lets_its_binding_go() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &owner, folder, false).await;
    let mut p = Phone::call(&w, &owner, &format!("chat_folder={folder}")).await;
    let lock = w.state.hold_chat_folder_for_tests(folder).await;
    p.c.connect().await;
    let binding = format!("WebRTC call {}: binding", p.id);
    until("the open's bind began", || log.text().contains(&binding)).await;
    let released = w
        .state
        .calls
        .hangup(&p.id, Some("the dashboard".into()))
        .expect("the call is live");
    drop(lock);
    within("the call let go", released.wait()).await;
    // Nothing holds the thread: a takeover=never bind is not refused.
    let cookie = w.cookie();
    let bound = w
        .connect(
            &format!("chat_thread={tid}&takeover=never"),
            &[("cookie", &cookie)],
        )
        .await;
    assert!(bound.is_ok(), "{:?}", bound.err());
    let events = p.closed().await;
    assert!(
        !events.iter().any(|e| e["type"] == "session.created"),
        "no session ran: {events:?}"
    );
    let closed = events
        .last()
        .unwrap_or_else(|| panic!("no event before the close"));
    assert_eq!(
        (&closed["type"], &closed["kind"], &closed["by"]),
        (
            &json!("lmgw.session.closed"),
            &json!("closed"),
            &json!("the dashboard")
        ),
        "{events:?}"
    );
    assert!(
        !log.text().contains("closed by force"),
        "the call let go by itself"
    );
}
