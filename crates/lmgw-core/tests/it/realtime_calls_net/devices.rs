//! The device socket group (calls design §3.1, §13.1; WP7): UDP sockets on
//! the device listener's addresses and port, bound as it starts, moved as
//! it moves or widens, closed as it stops, each change ending the device
//! calls alone while the plain group and its calls serve on; the status
//! block's `webrtc` shows both groups.
//!
//! The device listener is armed as `server::serve` arms it, so its own
//! follower binds the group; the calls are offered on it as the route
//! offers a call POSTed there (`Group::Devices`, the POST's local address).

use std::net::SocketAddr;
use std::time::Duration;

use futures::FutureExt;
use lmgw_api_types::device_link::DeviceListenerState;
use lmgw_api_types::realtime::{WebRtcGroup, WebRtcGroupState};
use lmgw_core::config::{DevicesListen, Settings, WebRtcSettings};
use lmgw_core::realtime::calls::{CallHandle, Ending, Group, GroupState, Offer, OfferError, Sent};
use lmgw_core::server::device_listener::Applied;
use lmgw_core::state::{AppState, SharedState};
use str0m::Event;

use super::{call, call_event, connect, ip, offer, text, Client, LO4};
use crate::common::patience::until;
use crate::support::device_link::{arm, arm_free, free_port};

/// Whether a bind failed because another socket holds its port.
fn in_use(why: &str) -> bool {
    why.contains("Address already in use")
}

/// The device group once it follows the device listener as it serves now:
/// bound on the listener's own addresses, or its bind error there.
async fn settled(state: &SharedState) -> Result<Vec<SocketAddr>, String> {
    let wanted = state.devices.listener.bound();
    assert!(!wanted.is_empty(), "the device listener serves");
    let port = wanted[0].port();
    let mut out = None;
    until("the device group following the device listener", || {
        match state.calls.group(Group::Devices) {
            GroupState::Bound(a) if a == wanted => out = Some(Ok(a)),
            GroupState::Failed(why) if why.contains(&format!(":{port}: ")) => out = Some(Err(why)),
            _ => return false,
        }
        true
    })
    .await;
    out.unwrap()
}

/// Save `edit` to the settings and apply it as a save of the device
/// listener's settings does.
pub(crate) async fn save(state: &SharedState, edit: impl FnOnce(&mut Settings)) -> Applied {
    let mut s = state.snapshot().settings.clone();
    edit(&mut s);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state.devices.listener.reconcile(state).await
}

/// Apply `edit` to the device listener's settings, and see it serve with
/// its device group bound where it serves: the port. A port another socket
/// holds (another suite's, TCP or UDP) is let go and another one asked for,
/// as the owner would after reading the bind error.
pub(crate) async fn move_devices(state: &SharedState, edit: impl FnOnce(&mut Settings)) -> u16 {
    let mut applied = save(state, edit).await;
    for _ in 0..16 {
        match &applied {
            Applied::Listening { .. } => match settled(state).await {
                Ok(addrs) => return addrs[0].port(),
                Err(why) if in_use(&why) => {}
                Err(why) => panic!("the device group did not bind: {why}"),
            },
            Applied::Failed { error, .. } if in_use(error) => {}
            other => panic!("the device listener does not serve: {other:?}"),
        }
        let port = free_port();
        applied = save(state, |s| s.devices.port = Some(port)).await;
    }
    panic!("no port free for TCP and UDP held for the device listener in 16 tries");
}

/// The device listener on loopback, armed as `server::serve` arms it, with
/// its device group bound: its port.
pub(crate) async fn devices_listening(state: &SharedState) -> u16 {
    let (port, _) = arm_free(state, |_| {}).await;
    match settled(state).await {
        Ok(_) => port,
        Err(why) if in_use(&why) => {
            let port = free_port();
            move_devices(state, |s| s.devices.port = Some(port)).await
        }
        Err(why) => panic!("the device group did not bind: {why}"),
    }
}

/// [`offer`] on the device group: a call POSTed to the device listener on
/// `local`.
fn device_offer(state: &SharedState, sdp: String, local: &str) -> Offer {
    Offer {
        group: Group::Devices,
        // The start of the device listener the POST's connection belongs to.
        served_at: state
            .devices
            .listener
            .watch()
            .borrow()
            .as_ref()
            .map(|b| b.served_at),
        ..offer(sdp, local)
    }
}

/// A client on `client_ip`, answered on the device group as if its POST
/// came in on the device listener's `local`.
async fn device_call(
    state: &SharedState,
    client_ip: &str,
    local: &str,
) -> (Client, CallHandle, String) {
    let (mut c, sdp) = Client::new(ip(client_ip)).await;
    let answered = state
        .calls
        .offer(device_offer(state, sdp, local))
        .await
        .unwrap();
    c.accept(&answered.sdp);
    (c, answered.handle, answered.sdp)
}

/// The call's owner, which says `last` and closes when asked to end: why
/// it was asked.
fn owner(mut h: CallHandle, last: &'static str) -> tokio::task::JoinHandle<Ending> {
    tokio::spawn(async move {
        let why = h.control.ending().await;
        assert_eq!(h.control.send(last.into()).await, Sent::Sent);
        h.control.close().await;
        why
    })
}

/// Run `f` to its end with `c` driven meanwhile.
async fn driving<T>(c: &mut Client, f: impl std::future::Future<Output = T>) -> T {
    tokio::pin!(f);
    loop {
        tokio::select! {
            v = &mut f => return v,
            () = c.run_for(Duration::from_millis(5)) => {}
        }
    }
}

/// What lmgw sent `c` last on `oai-events` before the call closed.
async fn last_word(c: &mut Client) -> String {
    let mut last = None;
    c.until("the call closed on the client", |_, e| match e {
        Event::ChannelData(d) => {
            last = Some(String::from_utf8(d.data).unwrap());
            None
        }
        Event::Closed => Some(()),
        _ => None,
    })
    .await;
    last.expect("a last word before the close")
}

/// The plain call `(c, h)` runs on, never asked to end.
async fn runs_on(c: &mut Client, h: &mut CallHandle, word: &str) {
    assert!(h.control.ending().now_or_never().is_none(), "never asked");
    c.send_text(word);
    let got = call_event(c, &mut h.events, "the plain call's event", text).await;
    assert_eq!(got, word);
}

/// A loopback device listener's group: a socket on each address of the
/// pair, on the device port, and a call over each connects on its own, its
/// candidate the POST's address on that port.
#[tokio::test]
async fn the_device_group_binds_the_device_listeners_addresses_and_port() {
    let state = AppState::init_for_tests().await.unwrap();
    let port = devices_listening(&state).await;
    let GroupState::Bound(addrs) = state.calls.group(Group::Devices) else {
        panic!("{:?}", state.calls.group(Group::Devices))
    };
    assert_eq!(addrs, state.devices.listener.bound());
    assert!(addrs
        .iter()
        .all(|a| a.port() == port && a.ip().is_loopback()));
    assert!(addrs.iter().any(SocketAddr::is_ipv4), "{addrs:?}");
    assert_eq!(state.calls.group(Group::Plain), GroupState::Down, "apart");
    // The status block shows both groups, each with its rule.
    let status = lmgw_core::ops::status(&state).await.unwrap().webrtc;
    assert!(status.enabled);
    let [plain, devices] = &status.groups[..] else {
        panic!("{status:?}")
    };
    assert_eq!(
        (plain.group, plain.state),
        (WebRtcGroup::Plain, WebRtcGroupState::Down)
    );
    assert!(plain.rule.contains("realtime.webrtc_port"), "{plain:?}");
    assert_eq!(
        (devices.group, devices.state, &devices.error),
        (WebRtcGroup::Devices, WebRtcGroupState::Bound, &None)
    );
    assert_eq!(
        devices.addrs,
        addrs
            .iter()
            .map(|a| format!("udp://{a}"))
            .collect::<Vec<_>>()
    );
    assert!(devices.rule.contains("devices.port"), "{devices:?}");
    for a in &addrs {
        let lo = a.ip().to_string();
        let (mut c, mut h, answer) = device_call(&state, &lo, &lo).await;
        assert!(
            answer.contains(&format!("{lo} {port} typ host")),
            "{answer}"
        );
        let info = state.calls.get(h.id()).unwrap();
        assert_eq!((info.group, info.candidate), (Group::Devices, *a));
        connect(&mut c, &mut h).await;
        c.send_text("over the device group");
        let got = call_event(&mut c, &mut h.events, "the device call's event", text).await;
        assert_eq!(got, "over the device group");
    }
}

/// A move to another port, a widening to every address on one port, and
/// the off each end the device calls (their owners asked, their last word
/// out) and move the device group with the listener; a stock call on the
/// plain group is never asked and runs on, and the next device call
/// connects on the new socket.
#[tokio::test]
async fn each_change_of_the_device_listener_moves_its_group_and_ends_its_calls_alone() {
    let state = AppState::init_for_tests().await.unwrap();
    let calls = &state.calls;
    let plain = calls
        .bind_group(Group::Plain, &["127.0.0.1:0".parse().unwrap()], None)
        .await;
    assert!(matches!(plain, GroupState::Bound(_)), "{plain:?}");
    let (mut p, mut hp, _) = call(calls, LO4, LO4).await;
    connect(&mut p, &mut hp).await;
    devices_listening(&state).await;
    let (mut d, mut hd, _) = device_call(&state, LO4, LO4).await;
    connect(&mut d, &mut hd).await;
    let id = hd.id().to_string();
    let asked = owner(hd, "moving");

    // To another port.
    let to = free_port();
    let moved = driving(&mut d, move_devices(&state, |s| s.devices.port = Some(to))).await;
    assert_eq!(asked.await.unwrap(), Ending::ShuttingDown);
    assert_eq!(last_word(&mut d).await, "moving");
    assert!(calls.get(&id).is_none());
    runs_on(&mut p, &mut hp, "after the move").await;
    let (mut d, mut hd, answer) = device_call(&state, LO4, LO4).await;
    assert!(
        answer.contains(&format!("127.0.0.1 {moved} typ host")),
        "the new port: {answer}"
    );
    connect(&mut d, &mut hd).await;
    let asked = owner(hd, "widening");

    // To every address on the same port: the loopback pair's sockets close
    // before `[::]` binds the port they held.
    let wide = driving(
        &mut d,
        move_devices(&state, |s| s.devices.listen = DevicesListen::All),
    )
    .await;
    assert_eq!(asked.await.unwrap(), Ending::ShuttingDown);
    assert_eq!(last_word(&mut d).await, "widening");
    assert_eq!(
        calls.group(Group::Devices),
        GroupState::Bound(vec![SocketAddr::new(ip("::"), wide)])
    );
    runs_on(&mut p, &mut hp, "after the widening").await;
    // `[::]` is dual-stack, as the device listener's own socket is: an IPv4
    // call and an IPv6 one beside it.
    let (mut d4, mut hd4, answer) = device_call(&state, LO4, LO4).await;
    assert!(
        answer.contains(&format!("127.0.0.1 {wide} typ host")),
        "{answer}"
    );
    connect(&mut d4, &mut hd4).await;
    let (mut d6, mut hd6, _) = device_call(&state, "::1", "::1").await;
    connect(&mut d6, &mut hd6).await;
    let (asked4, asked6) = (owner(hd4, "off"), owner(hd6, "off"));

    // Off: the device calls end and the group closes.
    let off = async {
        let applied = save(&state, |s| s.devices.listener = false).await;
        assert!(matches!(applied, Applied::Stopped { .. }), "{applied:?}");
        until("the device group closed", || {
            calls.group(Group::Devices) == GroupState::Down
        })
        .await;
    };
    driving(&mut d6, driving(&mut d4, off)).await;
    assert_eq!(asked4.await.unwrap(), Ending::ShuttingDown);
    assert_eq!(asked6.await.unwrap(), Ending::ShuttingDown);
    assert_eq!(last_word(&mut d4).await, "off");
    assert_eq!(last_word(&mut d6).await, "off");
    assert!(calls
        .list()
        .iter()
        .all(|c| c.group == Group::Plain && c.id.as_ref() == hp.id()));
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let refused = calls
        .offer(device_offer(&state, sdp, LO4))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains("not running")),
        "{refused:?}"
    );
    runs_on(&mut p, &mut hp, "after the off").await;
}

/// A fresh gateway whose device listener listens on `127.0.0.1` at a port
/// whose UDP side the returned socket holds: another suite may take the TCP
/// side between the pick and the bind, and then another port and another
/// gateway are tried, a bounded number of times. Never skips.
async fn listening_over_a_held_udp_port() -> (SharedState, u16, std::net::UdpSocket) {
    for _ in 0..32 {
        let state = AppState::init_for_tests().await.unwrap();
        let port = free_port();
        let Ok(holder) = std::net::UdpSocket::bind(("127.0.0.1", port)) else {
            continue;
        };
        arm(&state, port, |s| {
            s.devices.listen = DevicesListen::Addr(ip(LO4))
        })
        .await;
        if state.devices.listener.status(&state).state == DeviceListenerState::Listening {
            return (state, port, holder);
        }
    }
    panic!("no port with its TCP side free for the device listener in 32 tries");
}

/// A UDP port another socket holds fails the device group, not the device
/// listener: device calls are refused naming the bind error and the
/// remedy, the listener serves on, the answer of a save says the group
/// failed, and the same settings saved again — once the port is free — bind
/// it. A move binds it too.
#[tokio::test]
async fn a_failed_device_group_leaves_the_listener_serving_and_a_save_retries_it() {
    let (state, port, holder) = listening_over_a_held_udp_port().await;
    let why = settled(&state).await.unwrap_err();
    assert!(
        in_use(&why) && why.contains(&format!("127.0.0.1:{port}")),
        "{why}"
    );
    assert!(
        why.contains("free the port") && why.contains("save the device settings again"),
        "the remedy: {why}"
    );
    assert_eq!(
        state.devices.listener.status(&state).state,
        DeviceListenerState::Listening,
        "the device listener serves on"
    );
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let refused = state
        .calls
        .offer(device_offer(&state, sdp, LO4))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains(&format!("127.0.0.1:{port}"))),
        "{refused:?}"
    );
    let devices = &state.calls.status().groups[1];
    assert_eq!(devices.state, WebRtcGroupState::Failed, "{devices:?}");
    assert!(
        devices
            .error
            .as_deref()
            .is_some_and(|e| e.contains(&format!("127.0.0.1:{port}"))),
        "{devices:?}"
    );

    // The same settings saved with the port still held: nothing changes for
    // the listener, the answer says the group is failed, and the retry
    // fails again.
    let note = lmgw_core::ops::apply_devices_listener(&state)
        .await
        .expect("a note");
    assert!(
        note.contains("UDP group")
            && note.contains("failed")
            && note.contains(&format!("127.0.0.1:{port}")),
        "{note}"
    );
    let why = settled(&state).await.unwrap_err();
    assert!(in_use(&why), "{why}");

    // The port is free, and the same settings saved again bind the group.
    drop(holder);
    let applied = save(&state, |_| {}).await;
    assert_eq!(applied, Applied::Unchanged);
    until("the retry bound the device group", || {
        matches!(state.calls.group(Group::Devices), GroupState::Bound(_))
    })
    .await;
    assert_eq!(
        state.calls.group(Group::Devices),
        GroupState::Bound(state.devices.listener.bound())
    );
    let (mut c, mut h, _) = device_call(&state, LO4, LO4).await;
    connect(&mut c, &mut h).await;
}

/// A held port fails the group and the next move binds it again.
#[tokio::test]
async fn the_next_move_binds_a_failed_device_group() {
    let (state, _port, holder) = listening_over_a_held_udp_port().await;
    settled(&state).await.unwrap_err();
    drop(holder);
    // Another suite may have taken the TCP half too; either way the move
    // binds both.
    let to = free_port();
    move_devices(&state, |s| s.devices.port = Some(to)).await;
    let (mut c, mut h, _) = device_call(&state, LO4, LO4).await;
    connect(&mut c, &mut h).await;
}

/// While the follower moves the group (its calls asked to end, the sockets
/// closed after them), a device call is refused as the gap, not as the
/// listener being off; and a call on a connection of an earlier start of the
/// listener is refused the same way once the group is bound for the next.
#[tokio::test]
async fn the_gap_of_a_move_and_an_earlier_start_s_connection_are_refused_as_moving() {
    let state = AppState::init_for_tests().await.unwrap();
    devices_listening(&state).await;
    let (mut d, mut hd, _) = device_call(&state, LO4, LO4).await;
    connect(&mut d, &mut hd).await;
    let earlier = state
        .devices
        .listener
        .watch()
        .borrow()
        .clone()
        .unwrap()
        .served_at;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let asked = tokio::spawn(async move {
        let why = hd.control.ending().await;
        let _ = released.await;
        assert_eq!(hd.control.send("moving".into()).await, Sent::Sent);
        hd.control.close().await;
        why
    });
    let to = free_port();
    let probe = async {
        until("the device group moving", || {
            state.calls.group(Group::Devices) == GroupState::Moving
        })
        .await;
        let (_c, sdp) = Client::new(ip(LO4)).await;
        let refused = state
            .calls
            .offer(device_offer(&state, sdp, LO4))
            .await
            .err()
            .unwrap();
        assert_eq!(refused, OfferError::Moving);
        assert!(!state.calls.serves(Group::Devices));
        release.send(()).unwrap();
    };
    let (_, ()) = driving(&mut d, async {
        tokio::join!(move_devices(&state, |s| s.devices.port = Some(to)), probe)
    })
    .await;
    assert_eq!(asked.await.unwrap(), Ending::ShuttingDown);

    // The group is bound for the new start: the earlier start's POST is
    // refused, the new one's answered.
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let stale = Offer {
        served_at: Some(earlier),
        ..device_offer(&state, sdp, LO4)
    };
    assert_eq!(
        state.calls.offer(stale).await.err().unwrap(),
        OfferError::Moving
    );
    let (mut c, mut h, _) = device_call(&state, LO4, LO4).await;
    connect(&mut c, &mut h).await;
}

/// With `realtime.webrtc_enabled` off at the gateway's start, the device
/// group binds nothing and a call is refused naming the setting, whatever
/// the device listener does: it still reads off after a move.
#[tokio::test]
async fn a_device_group_switched_off_at_the_start_stays_off_beside_the_listener() {
    let state = AppState::init_for_tests().await.unwrap();
    let off = WebRtcSettings {
        webrtc_enabled: false,
        webrtc_port: None,
    };
    assert!(state
        .calls
        .serve_plain(&off, "127.0.0.1:0".parse().unwrap(), None)
        .await
        .is_none());
    // Off before the listener ever serves, too (a restart that flipped the
    // switch while the listener was off).
    assert_eq!(state.calls.group(Group::Devices), GroupState::Off);
    arm_free(&state, |_| {}).await;
    until("the device group off", || {
        state.calls.group(Group::Devices) == GroupState::Off
    })
    .await;
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let refused = state
        .calls
        .offer(device_offer(&state, sdp, LO4))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains("realtime.webrtc_enabled")),
        "{refused:?}"
    );
    let status = state.calls.status();
    assert!(!status.enabled, "{status:?}");
    assert!(
        status
            .groups
            .iter()
            .all(|g| g.state == WebRtcGroupState::Off),
        "{status:?}"
    );
    // The listener moves once more: the group binds nothing.
    let to = free_port();
    let applied = save(&state, |s| s.devices.port = Some(to)).await;
    assert!(matches!(applied, Applied::Listening { .. }), "{applied:?}");
    // The follower has applied the move once the group has been through
    // its gap; a bind would show as Bound.
    until("the follower through the move", || {
        state.calls.group(Group::Devices) == GroupState::Off
    })
    .await;
    tokio::task::yield_now().await;
    assert_eq!(state.calls.group(Group::Devices), GroupState::Off);
}

/// A save that would make `realtime.webrtc_port` and `devices.port` one UDP
/// port on overlapping addresses is refused naming both settings, from
/// either side, and saves nothing.
#[tokio::test]
async fn a_save_that_makes_the_two_udp_ports_collide_is_refused_either_way() {
    use lmgw_core::ops::{settings_set, SettingsPatch};
    let state = AppState::init_for_tests().await.unwrap();
    let patch = |v: serde_json::Value| serde_json::from_value::<SettingsPatch>(v).unwrap();
    let http: SocketAddr = state.snapshot().settings.bind_addr.parse().unwrap();
    let plain = http.port();
    let both = |e: &str| e.contains("realtime.webrtc_port") && e.contains("devices.port");

    // The devices save: the device port onto the plain group's.
    let e = settings_set(
        &state,
        patch(serde_json::json!({"devices": {"listener": true, "port": plain}})),
    )
    .await
    .unwrap_err();
    assert!(both(&e), "{e}");
    assert!(!state.snapshot().settings.devices.listener, "nothing saved");

    // A device port beside it saves; then the realtime save: the plain
    // group's port onto the device listener's.
    let apart = if plain < 60000 { plain + 7 } else { plain - 7 };
    settings_set(
        &state,
        patch(serde_json::json!({"devices": {"listener": true, "port": apart}})),
    )
    .await
    .unwrap();
    let e = settings_set(
        &state,
        patch(serde_json::json!({"realtime": {"webrtc_port": apart}})),
    )
    .await
    .unwrap_err();
    assert!(both(&e), "{e}");
    assert_eq!(state.snapshot().settings.realtime.webrtc.webrtc_port, None);
    settings_set(
        &state,
        patch(serde_json::json!({"realtime": {"webrtc_port": apart + 1}})),
    )
    .await
    .unwrap();
}
