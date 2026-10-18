//! Where datagrams go when addresses move (calls design §3.3, §3.6): a
//! client that changes its source port keeps its call, a new call from the
//! address a stale call last heard from is not swallowed by it, and a
//! binding request forged as the client proves nothing.

use std::time::Duration;

use lmgw_core::realtime::calls::{CallEvent, Group, GroupState, Sent};
use str0m::ice::{StunMessage, TransId};
use str0m::Event;

use super::{call, call_event, calls_on, connect, offer, text, ufrag, Client, LO4};
use crate::common::patience::{until, within};

#[tokio::test]
async fn a_client_that_moves_to_a_new_port_keeps_its_call() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut c, mut h, _) = call(&calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    let before = c.local;
    assert_eq!(calls.get(h.id()).unwrap().remote, Some(before));

    c.move_port();
    assert_ne!(c.local, before);
    // Its audio keeps arriving, now from the new port, which the call
    // follows once the client's checks from it have proved it.
    let id = h.id().to_string();
    let mut after = 0;
    within("the call follows the client to its new port", async {
        loop {
            c.send_frame(&[9; 40]);
            c.run_for(Duration::from_millis(20)).await;
            while let Some(e) = h.events.try_next() {
                if matches!(e, CallEvent::Audio(_)) {
                    after += 1;
                }
            }
            if after >= 5 && calls.get(&id).unwrap().remote == Some(c.local) {
                break;
            }
        }
    })
    .await;
    // Both ways: lmgw's event reaches the client on its new port, and the
    // client's reaches the call. lmgw's direction takes str0m's time: the
    // new pair is peer-reflexive, of a lower priority than the host pair
    // the client left, and str0m's ICE-lite agent sends on the highest
    // nominated pair until the old one has gone 15 s without a check
    // (calls design §3.7, as built).
    let control = h.control;
    let mut events = h.events;
    let sender = tokio::spawn(async move {
        let sent = control.send("after the move".into()).await;
        (control, sent)
    });
    let got = c
        .until("lmgw's event on the new port", |_, e| match e {
            Event::ChannelData(d) => Some(String::from_utf8(d.data).unwrap()),
            _ => None,
        })
        .await;
    assert_eq!(got, "after the move");
    let (_control, sent) = sender.await.unwrap();
    assert_eq!(sent, Sent::Sent);
    c.send_text("from the new port");
    let got = call_event(&mut c, &mut events, "the client's event", text).await;
    assert_eq!(got, "from the new port");
}

#[tokio::test]
async fn a_stale_call_does_not_take_a_new_calls_traffic_from_its_old_address() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut a, mut ha, _) = call(&calls, LO4, LO4).await;
    connect(&mut a, &mut ha).await;
    let addr = a.local;
    // The client goes away without a word: its call stays, its remote the
    // address a new client is about to use.
    drop(a);
    let (mut b, sdp) = Client::at(addr).await;
    let answered = calls.offer(offer(sdp, LO4)).await.unwrap();
    b.accept(&answered.sdp);
    let mut hb = answered.handle;
    connect(&mut b, &mut hb).await;
    b.send_text("for b");
    let got = call_event(&mut b, &mut hb.events, "b's event", text).await;
    assert_eq!(got, "for b");
    assert_eq!(calls.get(hb.id()).unwrap().remote, Some(addr));
    while let Some(e) = ha.events.try_next() {
        assert!(
            !matches!(e, CallEvent::Message { .. }),
            "the stale call took b's traffic: {e:?}"
        );
    }
}

#[tokio::test]
async fn a_forged_binding_request_from_the_clients_own_address_proves_nothing() {
    let calls = calls_on("127.0.0.1:0").await;
    let GroupState::Bound(addrs) = calls.group(Group::Plain) else {
        panic!()
    };
    let (mut c, mut h, answer) = call(&calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    // The client is driven no further. A datagram that parses as nothing,
    // from its own socket, is read behind everything it sent before: once
    // it is counted, the call's state holds only the client's real traffic.
    let d0 = calls.dropped();
    c.send_raw(b"\x07junk", addrs[0]);
    until("the junk read", || calls.dropped() > d0).await;
    let before = calls.get(h.id()).unwrap();
    // As an on-path attacker would: the ufrags seen in the clear, sent as
    // the client, with a MESSAGE-INTEGRITY made without the password.
    let username = format!("{}:{}", ufrag(&answer), c.ufrag());
    let req = StunMessage::binding_request(&username, TransId::new(), true, 1, 1, true);
    let mut buf = [0u8; 512];
    let n = req
        .to_bytes(Some(b"not the password"), &mut buf, |_, _| [7u8; 20])
        .unwrap();
    c.send_raw(&buf[..n], addrs[0]);
    // More junk behind it. A second drop is the forged request's (it is
    // dropped) or this junk's (it was not): either way the forged request
    // has been read.
    c.send_raw(b"\x07junk", addrs[0]);
    until("the forged request read", || calls.dropped() > d0 + 1).await;
    let after = calls.get(h.id()).unwrap();
    assert_eq!(after.last_heard, before.last_heard, "no liveness evidence");
    assert_eq!(after.remote, before.remote);
    until("both junk and the forged request dropped", || {
        calls.dropped() > d0 + 2
    })
    .await;
}
