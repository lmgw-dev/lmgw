//! The plain group's life beside the HTTP listener (calls design §3.1): it
//! binds at start on `realtime.webrtc_port` — by default the HTTP port's
//! number, on the listener's IP — and is closed with the listener before
//! the server's release fires, so a restart binds it again; off, nothing
//! binds and a call is refused naming the setting.

use std::net::SocketAddr;

use lmgw_core::config::WebRtcSettings;
use lmgw_core::realtime::calls::{Calls, Ending, Group, GroupState, OfferError, Sent};
use lmgw_core::state::AppState;
use lmgw_core::store;
use str0m::Event;

use super::{call, connect, ip, offer, Client, LO4};
use crate::common::patience::{until, within};

/// Serve `state` on `addr` until the returned stop is sent; the receiver
/// fires once the listener is released.
async fn serve(
    state: &lmgw_core::state::SharedState,
    addr: SocketAddr,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    SocketAddr,
) {
    let listener = lmgw_core::server::bind(state, addr).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    (stop, released, bound)
}

/// Serve `state` on a fresh HTTP port whose number is free for UDP too:
/// the plain group takes the HTTP port's number, which a parallel test may
/// hold, so a pair whose UDP half is taken is let go and another tried.
async fn serve_fresh(
    state: &lmgw_core::state::SharedState,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    SocketAddr,
) {
    for _ in 0..50 {
        let (stop, released, http) = serve(state, "127.0.0.1:0".parse().unwrap()).await;
        let udp = SocketAddr::new(http.ip(), http.port());
        let mut bound = false;
        until("the plain group's bind", || {
            match state.calls.group(Group::Plain) {
                GroupState::Bound(a) if a == [udp] => bound = true,
                GroupState::Failed(why) if why.contains(&udp.to_string()) => {}
                _ => return false,
            }
            true
        })
        .await;
        if bound {
            return (stop, released, http);
        }
        stop.send(()).unwrap();
        within("the release of a taken pair", released)
            .await
            .unwrap();
    }
    panic!("no ephemeral port free for both TCP and UDP in 50 tries");
}

#[tokio::test]
async fn the_plain_group_binds_beside_the_listener_and_is_released_for_a_restart() {
    let state = AppState::init_for_tests().await.unwrap();
    // The HTTP port's number, on the listener's IP (the default rule).
    let (stop, released, http) = serve_fresh(&state).await;
    let udp = SocketAddr::new(http.ip(), http.port());
    let (mut c, mut h, _) = call(&state.calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    // The owner hears the stop and has room for its last word.
    let owner = tokio::spawn(async move {
        let why = h.control.ending().await;
        let sent = h.control.send("the last word".into()).await;
        h.control.close().await;
        (why, sent)
    });

    stop.send(()).unwrap();
    let last = c
        .until("the last word on the client", |_, e| match e {
            Event::ChannelData(d) => Some(String::from_utf8(d.data).unwrap()),
            _ => None,
        })
        .await;
    assert_eq!(last, "the last word");
    assert_eq!(owner.await.unwrap(), (Ending::ShuttingDown, Sent::Sent));
    within("the release", released).await.unwrap();
    // Released means closed: the group's call ended, and the port binds.
    assert_eq!(state.calls.group(Group::Plain), GroupState::Down);
    assert!(state.calls.list().is_empty());
    let taken = std::net::UdpSocket::bind(udp).expect("the released UDP port binds again");
    drop(taken);

    // The restart binds the same address again, and calls connect on it.
    let (stop, released, again) = serve(&state, http).await;
    assert_eq!(again, http);
    until("the plain group bound again", || {
        state.calls.group(Group::Plain) == GroupState::Bound(vec![udp])
    })
    .await;
    let (mut c, mut h, _) = call(&state.calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    drop(h);
    stop.send(()).unwrap();
    within("the second release", released).await.unwrap();
}

#[tokio::test]
async fn the_port_setting_and_the_switch_apply_at_start() {
    let state = AppState::init_for_tests().await.unwrap();
    let mut s = state.snapshot().settings.clone();
    s.realtime.webrtc.webrtc_port = Some(0);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let (stop, released, http) = serve(&state, "127.0.0.1:0".parse().unwrap()).await;
    until("bound on a port the OS chose", || {
        matches!(state.calls.group(Group::Plain), GroupState::Bound(a) if a[0].ip() == http.ip())
    })
    .await;
    stop.send(()).unwrap();
    within("the release", released).await.unwrap();

    // Off: nothing binds, the release is the listener's own, and a call is
    // refused naming the setting.
    let calls = Calls::default();
    let off = WebRtcSettings {
        webrtc_enabled: false,
        webrtc_port: None,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let passed = calls.serve_plain(&off, http, Some(tx)).await;
    assert_eq!(calls.group(Group::Plain), GroupState::Off);
    passed.unwrap().send(()).unwrap();
    rx.await.unwrap();
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let refused = calls.offer(offer(sdp, LO4)).await.err().unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains("realtime.webrtc_enabled")),
        "{refused:?}"
    );

    // A port another socket holds: the bind fails, said by the group and
    // the refusal; lmgw goes on.
    let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let held = holder.local_addr().unwrap();
    let on = WebRtcSettings {
        webrtc_enabled: true,
        webrtc_port: Some(held.port()),
    };
    let passed = calls.serve_plain(&on, http, None).await;
    assert!(passed.is_none(), "nothing bound, nothing to release");
    let GroupState::Failed(why) = calls.group(Group::Plain) else {
        panic!("{:?}", calls.group(Group::Plain))
    };
    assert!(why.contains(&held.to_string()), "{why}");
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let refused = calls.offer(offer(sdp, LO4)).await.err().unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains(&held.to_string())),
        "{refused:?}"
    );
}
