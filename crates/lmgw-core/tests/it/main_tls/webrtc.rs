//! The TLS listener's WebRTC socket group (headless server design D22): on
//! `realtime.webrtc_tls_port`, unset the TLS port's number, beside the
//! plain group on the desktop; alone in container mode behind TLS, where
//! the plain listener is the container's loopback.

use std::time::Duration;

use lmgw_core::config::deployment::{Deployment, StartFacts};
use lmgw_core::principal::Principal;
use lmgw_core::realtime::calls::{CallParams, Group, GroupState, Offer};
use lmgw_core::state::AppState;

use super::{serve_on, Pki};
use crate::common::patience::until;
use crate::realtime_calls_net::client::Client;

fn calls_on(s: &mut lmgw_core::config::Settings) {
    s.realtime.webrtc.webrtc_enabled = true;
    // The plain group on a port the OS picks: a parallel suite may hold the
    // HTTP port's number for UDP.
    s.realtime.webrtc.webrtc_port = Some(0);
}

#[tokio::test]
async fn a_desktop_binds_both_groups_and_a_tls_call_gets_the_tls_port() {
    let pki = Pki::new();
    pki.write(&["localhost", "127.0.0.1"], 1);
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve_on(state, &pki, calls_on).await;
    let tls_udp = std::net::SocketAddr::new(gw.tls.ip(), gw.tls.port());
    until("the TLS group binds on the TLS port's number", || {
        gw.state.calls.group(Group::Tls) == GroupState::Bound(vec![tls_udp])
    })
    .await;
    assert!(
        matches!(gw.state.calls.group(Group::Plain), GroupState::Bound(_)),
        "{:?}",
        gw.state.calls.group(Group::Plain)
    );
    // A call POSTed on the TLS listener is answered on the TLS group's
    // socket: its one candidate is that port.
    let (_client, sdp) = Client::new("127.0.0.1".parse().unwrap()).await;
    let answered = gw
        .state
        .calls
        .offer(Offer {
            sdp,
            group: Group::of(lmgw_core::server::Via::Tls),
            local: "127.0.0.1".parse().unwrap(),
            served_at: None,
            creator: Principal::Anonymous,
            params: CallParams {
                never_opened: None,
                reorder: Duration::from_millis(60),
                max_message: Some(64 << 20),
            },
            keep: None,
        })
        .await
        .unwrap();
    let candidate = answered
        .sdp
        .lines()
        .find(|l| l.starts_with("a=candidate"))
        .unwrap()
        .to_string();
    assert!(
        candidate.contains(&format!(" 127.0.0.1 {} typ host", gw.tls.port())),
        "{candidate}"
    );
    drop(answered);
    // Stopped with the listener: the group is down once the release fired.
    let state = gw.stop().await;
    assert_eq!(state.calls.group(Group::Tls), GroupState::Down);
}

#[tokio::test]
async fn behind_tls_in_container_mode_only_the_tls_group_is_bound() {
    let pki = Pki::new();
    pki.write(&["localhost", "127.0.0.1"], 1);
    let state = AppState::init_for_tests().await.unwrap();
    state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        ..Default::default()
    });
    let gw = serve_on(state, &pki, |s| {
        calls_on(s);
        s.realtime.webrtc.webrtc_tls_port = Some(0);
    })
    .await;
    until("the TLS group binds", || {
        matches!(gw.state.calls.group(Group::Tls), GroupState::Bound(_))
    })
    .await;
    assert_eq!(
        gw.state.calls.group(Group::Plain),
        GroupState::Down,
        "the plain listener is the container's loopback: no UDP reaches it from outside"
    );
    let status = gw.state.calls.status();
    let groups: Vec<_> = status.groups.iter().map(|g| g.group).collect();
    use lmgw_api_types::realtime::WebRtcGroup;
    assert_eq!(
        groups,
        [WebRtcGroup::Plain, WebRtcGroup::Tls, WebRtcGroup::Devices]
    );
    gw.stop().await;
}

#[test]
fn a_tls_group_on_another_groups_port_is_refused_by_a_save() {
    let mut s = lmgw_core::config::Settings {
        bind_addr: "0.0.0.0:8787".into(),
        ..Default::default()
    };
    s.tls.cert_path = "/c".into();
    s.tls.key_path = "/k".into();
    s.tls.listen = "0.0.0.0:8443".into();
    lmgw_core::config::webrtc_port_collision(&s).unwrap();
    s.realtime.webrtc.webrtc_tls_port = Some(8787);
    let e = lmgw_core::config::webrtc_port_collision(&s).unwrap_err();
    assert!(
        e.contains("the WebRTC calls UDP port 8787")
            && e.contains("the WebRTC calls TLS UDP port 8787 (Settings → Realtime"),
        "{e}"
    );
    // Against the device group (on by default once the listener is): its
    // port is the HTTP port's number plus one.
    s.realtime.webrtc.webrtc_tls_port = None;
    s.devices.listener = true;
    s.devices.listen = lmgw_core::config::DevicesListen::All;
    s.tls.listen = "0.0.0.0:8788".into();
    let e = lmgw_core::config::webrtc_port_collision(&s).unwrap_err();
    assert!(
        e.contains("the WebRTC calls TLS UDP port 8788")
            && e.contains("the device listener's port 8788"),
        "{e}"
    );
}
