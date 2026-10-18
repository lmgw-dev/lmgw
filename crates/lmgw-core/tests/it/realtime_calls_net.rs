//! WebRTC calls on the network (calls design §3, §13.1; WP2 part A, WP7):
//! the plain socket group, the device group that follows the device
//! listener (`devices`), the driver, the registry and the offer → answer
//! step, against a str0m client over loopback UDP on ports the OS chooses.
//!
//! The client is str0m itself (`client`), driven in the test's task; the
//! call is lmgw's, read through its handle.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lmgw_core::principal::Principal;
use lmgw_core::realtime::calls::{
    CallEvent, CallEvents, CallHandle, CallParams, CallState, Calls, Ended, Ending, Group,
    GroupState, Message, Offer, OfferError, Sent,
};
use str0m::Event;

use crate::common::patience::within;

pub(crate) mod client;
pub(crate) mod devices;
mod ends;
mod restart;
mod routing;
mod sender_reports;
mod sends;

use client::{ufrag, Client, FRAME_TICKS};

const LO4: &str = "127.0.0.1";

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// A gateway's calls with the plain group bound on `addr`.
async fn calls_on(addr: &str) -> Calls {
    let calls = Calls::default();
    let state = calls
        .bind_group(Group::Plain, &[addr.parse().unwrap()], None)
        .await;
    assert!(matches!(state, GroupState::Bound(_)), "{state:?}");
    calls
}

/// Calls without a never-opened bound: the tests that connect take what
/// time they take.
fn params() -> CallParams {
    CallParams {
        never_opened: None,
        reorder: Duration::from_millis(60),
        max_message: Some(64 << 20),
    }
}

fn offer(sdp: String, local: &str) -> Offer {
    Offer {
        sdp,
        group: Group::Plain,
        local: ip(local),
        served_at: None,
        creator: Principal::Anonymous,
        params: params(),
        keep: None,
    }
}

/// A client on `client_ip`, answered by `calls` as if its POST arrived on
/// `local`.
async fn call(calls: &Calls, client_ip: &str, local: &str) -> (Client, CallHandle, String) {
    let (mut c, sdp) = Client::new(ip(client_ip)).await;
    let answered = calls.offer(offer(sdp, local)).await.unwrap();
    c.accept(&answered.sdp);
    (c, answered.handle, answered.sdp)
}

/// Drive `c` until the call's next event `f` takes.
async fn call_event<T>(
    c: &mut Client,
    events: &mut CallEvents,
    what: &str,
    mut f: impl FnMut(CallEvent) -> Option<T>,
) -> T {
    within(what, async {
        loop {
            tokio::select! {
                e = events.next() => {
                    let e = e.unwrap_or_else(|| panic!("{what}: the call's events ended"));
                    if let Some(v) = f(e) {
                        return v;
                    }
                }
                () = c.run_for(Duration::from_millis(5)) => {}
            }
        }
    })
    .await
}

/// Connect `c`, and see the call connect and open.
async fn connect(c: &mut Client, h: &mut CallHandle) {
    c.connect().await;
    call_event(c, &mut h.events, "the call connected", |e| {
        matches!(e, CallEvent::Connected).then_some(())
    })
    .await;
    call_event(c, &mut h.events, "oai-events open on the call", |e| {
        matches!(e, CallEvent::Open).then_some(())
    })
    .await;
}

fn text(e: CallEvent) -> Option<String> {
    match e {
        CallEvent::Message {
            message: Message::Text(t),
            ..
        } => Some(t),
        _ => None,
    }
}

#[tokio::test]
async fn a_call_connects_and_carries_events_both_ways_and_the_clients_audio() {
    let calls = calls_on("127.0.0.1:0").await;
    let GroupState::Bound(addrs) = calls.group(Group::Plain) else {
        panic!()
    };
    let (mut c, mut h, answer) = call(&calls, LO4, LO4).await;
    assert!(answer.contains("a=ice-lite"), "{answer}");
    let candidate = SocketAddr::new(ip(LO4), addrs[0].port());
    assert!(
        answer.contains(&format!("{} {} typ host", candidate.ip(), candidate.port())),
        "the one candidate is the POST's address on the group's port: {answer}"
    );
    let id = h.id().to_string();
    assert!(id.starts_with("rtc_") && id.len() == 28, "{id}");
    let info = calls.get(&id).unwrap();
    assert_eq!(
        (info.state, info.candidate),
        (CallState::Answered, candidate)
    );

    connect(&mut c, &mut h).await;
    let info = calls.get(&id).unwrap();
    assert_eq!(info.state, CallState::Open);
    assert_eq!(info.remote, Some(c.local), "the client's address");
    assert!(info.connected.is_some() && info.opened.is_some() && info.last_heard.is_some());

    // Client → call.
    c.send_text(r#"{"type":"session.update"}"#);
    let got = call_event(&mut c, &mut h.events, "the client's event", text).await;
    assert_eq!(got, r#"{"type":"session.update"}"#);

    // Call → client.
    let sent = h.control.send(r#"{"type":"session.created"}"#.into()).await;
    assert_eq!(sent, Sent::Sent);
    let got = c
        .until("lmgw's event on the client", |_, e| match e {
            Event::ChannelData(d) => Some(String::from_utf8(d.data).unwrap()),
            _ => None,
        })
        .await;
    assert_eq!(got, r#"{"type":"session.created"}"#);

    // The client's audio arrives frame by frame, raw, in RTP order.
    for k in 0..5u8 {
        c.send_frame(&[k; 40]);
    }
    let mut frames = Vec::new();
    while frames.len() < 5 {
        let f = call_event(&mut c, &mut h.events, "the client's audio", |e| match e {
            CallEvent::Audio(f) => Some(f),
            _ => None,
        })
        .await;
        frames.push(f);
    }
    for (k, f) in frames.iter().enumerate() {
        assert_eq!(&*f.payload, &[k as u8; 40][..], "frame {k}");
        assert_eq!(f.rtp_time - frames[0].rtp_time, k as u64 * FRAME_TICKS);
    }

    // The client closes `oai-events`: the call hears it, after the event
    // sent before it (the End's order, §8.1).
    c.send_text(r#"{"type":"lmgw.response.stop"}"#);
    c.close_channel();
    let last = call_event(&mut c, &mut h.events, "the stop", text).await;
    assert_eq!(last, r#"{"type":"lmgw.response.stop"}"#);
    call_event(&mut c, &mut h.events, "the channel's close", |e| {
        matches!(e, CallEvent::ChannelClosed).then_some(())
    })
    .await;
    assert_eq!(h.control.send("{}".into()).await, Sent::Gone);

    // The owner ends the call: it is gone, and the client hears the close.
    h.control.close().await;
    assert!(calls.get(&id).is_none());
    assert!(matches!(
        h.events.next().await,
        Some(CallEvent::Ended(Ended::Closed))
    ));
    c.until("the client sees the call closed", |_, e| {
        matches!(e, Event::Closed).then_some(())
    })
    .await;
}

/// The texts and audio payloads a call received until its third frame.
async fn seen(c: &mut Client, h: &mut CallHandle) -> (Vec<String>, Vec<Vec<u8>>) {
    let mut texts = Vec::new();
    let mut payloads = Vec::new();
    call_event(c, &mut h.events, "a call's three frames", |e| {
        match e {
            CallEvent::Message {
                message: Message::Text(t),
                ..
            } => texts.push(t),
            CallEvent::Audio(f) => payloads.push(f.payload.to_vec()),
            _ => {}
        }
        (payloads.len() == 3).then_some(())
    })
    .await;
    (texts, payloads)
}

#[tokio::test]
async fn two_calls_on_one_socket_are_kept_apart() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut a, mut ha, _) = call(&calls, LO4, LO4).await;
    let (mut b, mut hb, _) = call(&calls, LO4, LO4).await;
    within("both clients connected", async {
        tokio::join!(a.connect(), b.connect());
    })
    .await;
    for (c, h) in [(&mut a, &mut ha), (&mut b, &mut hb)] {
        call_event(c, &mut h.events, "open", |e| {
            matches!(e, CallEvent::Open).then_some(())
        })
        .await;
    }
    a.send_text("from a");
    b.send_text("from b");
    for _ in 0..3 {
        a.send_frame(b"aaaa");
        b.send_frame(b"bbbbbb");
    }
    let (ta, pa) = seen(&mut a, &mut ha).await;
    let (tb, pb) = seen(&mut b, &mut hb).await;
    assert_eq!(ta, ["from a"]);
    assert_eq!(tb, ["from b"]);
    assert!(pa.iter().all(|p| p == b"aaaa"), "{pa:?}");
    assert!(pb.iter().all(|p| p == b"bbbbbb"), "{pb:?}");
    let infos = calls.list();
    assert_eq!(infos.len(), 2);
    assert_eq!(calls.get(ha.id()).unwrap().remote, Some(a.local));
    assert_eq!(calls.get(hb.id()).unwrap().remote, Some(b.local));
}

#[tokio::test]
async fn a_stray_or_spoofed_datagram_is_dropped_unanswered() {
    let calls = calls_on("127.0.0.1:0").await;
    let GroupState::Bound(addrs) = calls.group(Group::Plain) else {
        panic!()
    };
    let (mut c, mut h, answer) = call(&calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    let before = calls.dropped();

    let stranger = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    stranger.set_nonblocking(true).unwrap();
    let to = addrs[0];
    // Junk, an RTP-looking packet from an address no call knows, and a
    // binding request naming the call and its client exactly as the client's
    // own do, with a wrong password: routed to the call by its ufrag, it
    // fails only str0m's MESSAGE-INTEGRITY check.
    stranger.send_to(b"\x07garbage", to).unwrap();
    let mut rtp = vec![0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7];
    rtp.extend_from_slice(&[0; 40]);
    stranger.send_to(&rtp, to).unwrap();
    let username = format!("{}:{}", ufrag(&answer), c.ufrag());
    let req = str0m::ice::StunMessage::binding_request(
        &username,
        str0m::ice::TransId::new(),
        true,
        1,
        1,
        true,
    );
    let mut buf = [0u8; 512];
    let n = req
        .to_bytes(Some(b"not the password"), &mut buf, |_, _| [0u8; 20])
        .unwrap();
    stranger.send_to(&buf[..n], to).unwrap();

    within("the strays counted", async {
        while calls.dropped() < before + 3 {
            c.run_for(Duration::from_millis(5)).await;
        }
    })
    .await;
    // The call went on through it, and the stranger heard nothing: what it
    // sent was over before the call's next exchange.
    c.send_text("still here");
    let got = call_event(&mut c, &mut h.events, "the event after the strays", text).await;
    assert_eq!(got, "still here");
    // Loopback delivers at once: an answer to the strays, sent as the
    // driver read them, would be waiting by now.
    let mut one = [0u8; 64];
    let nothing = stranger.recv_from(&mut one).unwrap_err();
    assert_eq!(
        nothing.kind(),
        std::io::ErrorKind::WouldBlock,
        "nothing answers a stranger"
    );
    assert_eq!(calls.get(h.id()).unwrap().state, CallState::Open);
}

/// Dropped with the call: what an owner keeps for the call's life.
struct Kept(Arc<AtomicBool>);

impl Drop for Kept {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_call_that_never_connects_is_ended_by_the_bound_and_lets_go_what_it_kept() {
    let calls = calls_on("127.0.0.1:0").await;
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let dropped = Arc::new(AtomicBool::new(false));
    let mut o = offer(sdp, LO4);
    o.params.never_opened = Some(Duration::from_millis(200));
    o.keep = Some(Box::new(Kept(dropped.clone())));
    let mut h = calls.offer(o).await.unwrap().handle;
    let id = h.id().to_string();
    assert!(calls.get(&id).is_some());
    let ended = within("the never-connected call ended", h.events.next()).await;
    assert!(
        matches!(ended, Some(CallEvent::Ended(Ended::NeverConnected))),
        "{ended:?}"
    );
    assert!(calls.get(&id).is_none(), "gone from the registry");
    assert!(dropped.load(Ordering::SeqCst), "what it kept is let go");
}

#[tokio::test]
async fn a_hangup_asks_the_owner_and_answers_once_the_call_let_go() {
    let calls = calls_on("127.0.0.1:0").await;
    let (_c, mut h, _) = call(&calls, LO4, LO4).await;
    assert!(calls.hangup("rtc_000000000000000000000000", None).is_none());
    let released = calls.hangup(h.id(), Some("the dashboard".into())).unwrap();
    let why = within("the hangup heard", h.control.ending()).await;
    assert_eq!(
        why,
        Ending::HungUp {
            by: Some("the dashboard".into())
        }
    );
    // A second hangup asks nothing new; the first reason stands.
    let again = calls.hangup(h.id(), None).unwrap();
    assert!(matches!(
        h.control.ending().await,
        Ending::HungUp { by: Some(_) }
    ));
    // The owner ends it; both hangups answer once it is gone.
    let id = h.id().to_string();
    drop(h);
    within("the call let go", released.wait()).await;
    within("the second hangup's answer", again.wait()).await;
    assert!(calls.get(&id).is_none());
}

#[tokio::test]
async fn the_clients_close_of_its_peer_connection_ends_the_call() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut c, mut h, _) = call(&calls, LO4, LO4).await;
    connect(&mut c, &mut h).await;
    c.send_text("the last word");
    c.rtc.close().unwrap();
    c.run_for(Duration::from_millis(1)).await;
    let mut got = Vec::new();
    let why = call_event(&mut c, &mut h.events, "the call's end", |e| match e {
        CallEvent::Ended(why) => Some(why),
        other => {
            got.extend(text(other));
            None
        }
    })
    .await;
    assert_eq!(why, Ended::PeerClosed);
    assert_eq!(got, ["the last word"], "what came before the close first");
    assert!(calls.get(h.id()).is_none());
}

#[tokio::test]
async fn wildcard_sockets_read_each_datagrams_destination() {
    // `0.0.0.0`: str0m accepts the checks because their destination comes
    // from the packet info, not the socket's own address.
    let v4 = calls_on("0.0.0.0:0").await;
    let (mut c, mut h, answer) = call(&v4, LO4, LO4).await;
    assert!(answer.contains("127.0.0.1"), "{answer}");
    connect(&mut c, &mut h).await;

    // `[::]`: an IPv4 POST's address arrives mapped and is made canonical;
    // IPv6 works beside it on the same socket.
    let v6 = calls_on("[::]:0").await;
    let (mut c4, mut h4, answer) = call(&v6, LO4, "::ffff:127.0.0.1").await;
    assert!(
        answer.contains("127.0.0.1") && !answer.contains("::ffff"),
        "{answer}"
    );
    connect(&mut c4, &mut h4).await;
    let (mut c6, mut h6, _) = call(&v6, "::1", "::1").await;
    connect(&mut c6, &mut h6).await;
    c6.send_text("over ipv6");
    let got = call_event(&mut c6, &mut h6.events, "the IPv6 event", text).await;
    assert_eq!(got, "over ipv6");

    // An IPv4 socket has nothing behind an IPv6 candidate.
    let (_x, sdp) = Client::new(ip("::1")).await;
    let refused = v4.offer(offer(sdp, "::1")).await.err().unwrap();
    assert!(
        matches!(&refused, OfferError::Unavailable(m) if m.contains("::1")),
        "{refused:?}"
    );
}
