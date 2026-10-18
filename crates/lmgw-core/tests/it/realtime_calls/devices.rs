//! Calls by paired devices through the device listener (calls design
//! §3.1, §3.2, §13.1; WP7): the device POSTs over mutual TLS
//! (`support::device_pair`), its call's candidate is the device listener's
//! address on its port, and its media runs on the device socket group. A
//! device's call takes the thread in the device's name, holds the device's
//! slot, may be hung up by the device and the owner alone, and ends
//! `shutting_down` when the device listener moves or stops, while a stock
//! call on the plain group serves on.

use std::net::SocketAddr;

use lmgw_api_types::realtime::WebRtcGroup;
use lmgw_core::devices::LinkKind;
use lmgw_core::realtime::calls::{Group, GroupState};
use serde_json::json;

use super::binding::ws_close;
use super::{calls_world, post_sdp, Phone};
use crate::common::patience::until;
use crate::device_chat::pair;
use crate::realtime_calls_net::devices::{devices_listening, move_devices, save};
use crate::support::device_link::free_port;

/// The call `p`'s last event before lmgw closed it: its `kind`.
async fn closed_kind(p: &mut Phone) -> String {
    let events = p.closed().await;
    let closed = events
        .last()
        .unwrap_or_else(|| panic!("no event before the close"));
    assert_eq!(closed["type"], "lmgw.session.closed", "{events:?}");
    closed["kind"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_paired_device_calls_through_the_device_listener() {
    let w = calls_world(|_| {}).await;
    let port = devices_listening(&w.state).await;
    let tid = w.thread("chatty", json!({})).await;
    let (mut ws, _) = w.bind(tid).await;
    let phone = pair(&w, "phone", json!({})).await;
    let mut p = Phone::call(&w, &phone.client, &format!("chat_thread={tid}")).await;
    // The device listener's address on its port, on the device group.
    let info = w.state.calls.get(&p.id).unwrap();
    assert_eq!(info.group, Group::Devices);
    assert_eq!(info.candidate, SocketAddr::from(([127, 0, 0, 1], port)));
    assert_eq!(info.creator.key_id(), Some(phone.id));
    assert!(matches!(
        w.state.calls.group(Group::Devices),
        GroupState::Bound(a) if a.contains(&info.candidate)
    ));
    // The open takes the thread in the device's name.
    p.c.connect().await;
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        tid
    );
    assert_eq!(
        ws_close(&mut ws).await,
        (4000, "voice mode moved to device 'phone'".into())
    );
    assert_eq!(
        w.state.calls.get(&p.id).unwrap().thread,
        Some(tid),
        "running, bound"
    );
    // The Devices card's voice link is the call, and the status block lists
    // it on the device group in the device's name (calls design §9).
    let links = w.state.devices.links.details(phone.id);
    assert!(
        links
            .iter()
            .any(|l| l.kind == LinkKind::Voice && l.call.as_deref() == Some(p.id.as_str())),
        "{links:?}"
    );
    let listed = w.state.calls.status().calls;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        (listed[0].group, listed[0].key.as_deref(), listed[0].thread),
        (WebRtcGroup::Devices, Some("device:phone"), Some(tid)),
        "{listed:?}"
    );
    // Another device gets the 404 of a call that is not there; the owner
    // hangs the device's call up, and its last event names who.
    let tablet = pair(&w, "tablet", json!({})).await;
    assert_eq!(p.hangup(&w, &tablet.client).await, 404);
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
    assert!(w.state.calls.list().is_empty());
}

#[tokio::test]
async fn a_device_at_its_limit_hangs_up_its_own_call_and_its_slot_comes_back() {
    let w = calls_world(|_| {}).await;
    devices_listening(&w.state).await;
    let phone = pair(&w, "phone", json!({ "concurrency_limit": 1 })).await;
    let p = Phone::call(&w, &phone.client, "model=chatty").await;
    // The call holds the device's one slot from its POST.
    let (_c, sdp) = Phone::offer().await;
    let second = post_sdp(&w, &phone.client, "model=chatty", &sdp).await;
    assert_eq!(second.status, 429, "{}", second.body);
    // Its own hangup takes none, and once it answers the slot is back.
    assert_eq!(p.hangup(&w, &phone.client).await, 200);
    assert!(w.state.calls.get(&p.id).is_none());
    let third = post_sdp(&w, &phone.client, "model=chatty", &sdp).await;
    assert_eq!(third.status, 201, "{}", third.body);
}

/// §13.1's live rebind end to end: moving the device listener ends a
/// running device call (`shutting_down`), the stock call on the plain group
/// runs on, the device's next call connects on the new port's socket, and
/// turning the device listener off ends that one too and closes the group
/// while the plain group serves on.
#[tokio::test]
async fn the_device_listeners_move_and_stop_end_device_calls_and_leave_stock_ones() {
    let w = calls_world(|_| {}).await;
    devices_listening(&w.state).await;
    let (device_thread, stock_thread) = (
        w.thread("chatty", json!({})).await,
        w.thread("chatty", json!({})).await,
    );
    let phone = pair(&w, "phone", json!({})).await;
    let mut p = Phone::connected(&w, &phone.client, &format!("chat_thread={device_thread}")).await;
    assert_eq!(p.next_but_states().await["type"], "session.created");
    let mut stock =
        Phone::connected(&w, &w.gw.client(), &format!("chat_thread={stock_thread}")).await;
    assert_eq!(stock.next_but_states().await["type"], "session.created");
    assert_eq!(w.state.calls.get(&stock.id).unwrap().group, Group::Plain);

    // The move: the device call's last event says why.
    let to = free_port();
    let (moved, kind) = tokio::join!(
        move_devices(&w.state, |s| s.devices.port = Some(to)),
        closed_kind(&mut p)
    );
    assert_eq!(kind, "shutting_down");
    assert!(w.state.calls.get(&p.id).is_none());
    // The stock call was never asked.
    stock.send(json!({"type": "session.update", "session": {"type": "realtime"}}));
    assert_eq!(stock.next_but_states().await["type"], "session.updated");

    // The device's next call reaches the listener on its new port, and its
    // media the device group's new socket.
    let phone = phone.at_port(moved).await;
    let mut p = Phone::connected(&w, &phone.client, &format!("chat_thread={device_thread}")).await;
    assert_eq!(p.next_but_states().await["type"], "session.created");
    assert_eq!(
        w.state.calls.get(&p.id).unwrap().candidate,
        SocketAddr::from(([127, 0, 0, 1], moved))
    );

    // The off: that call ends too, the device group closes.
    let (applied, kind) = tokio::join!(
        save(&w.state, |s| s.devices.listener = false),
        closed_kind(&mut p)
    );
    assert!(
        matches!(
            applied,
            lmgw_core::server::device_listener::Applied::Stopped { .. }
        ),
        "{applied:?}"
    );
    assert_eq!(kind, "shutting_down");
    until("the device group closed", || {
        w.state.calls.group(Group::Devices) == GroupState::Down
    })
    .await;
    stock.send(json!({"type": "session.update", "session": {"type": "realtime"}}));
    assert_eq!(stock.next_but_states().await["type"], "session.updated");
    assert_eq!(w.state.calls.list().len(), 1, "the stock call alone");
}
