//! What lmgw sends on `oai-events` (calls design §6.3): no more than
//! str0m's default send buffer is kept in flight before the next message,
//! whatever the client advertises, and a message over the largest a call
//! sends is refused naming whose bound it is.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use lmgw_core::realtime::calls::{Limit, Sent};
use str0m::Event;

use super::{calls_on, connect, ip, offer, Client, LO4};

#[tokio::test]
async fn sends_past_the_window_wait_for_room_and_arrive_whole_in_order() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut c, sdp) = Client::new(ip(LO4)).await;
    let answered = calls.offer(offer(sdp, LO4)).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    connect(&mut c, &mut h).await;
    // A call of its own, never connected: a send on it goes through the
    // driver behind whatever was asked of it before.
    let (_p, sdp) = Client::new(ip(LO4)).await;
    let probe = calls.offer(offer(sdp, LO4)).await.unwrap().handle;

    // The client is frozen: no SACK comes back. Three 100 KiB messages,
    // more together than the 128 KiB window, each within the client's
    // 256 KiB: with the buffer at the window plus the largest message
    // (384 KiB) all three would fit, and only the window holds the third.
    let big = |ch: char| ch.to_string().repeat(100 * 1024);
    let a = h.control.send(big('a'));
    let b = h.control.send(big('b'));
    let third = h.control.send(big('c'));
    tokio::pin!(a, b, third);
    let mut cx = Context::from_waker(Waker::noop());
    // Each first poll hands its message to the driver, in order.
    assert!(a.as_mut().poll(&mut cx).is_pending());
    assert!(b.as_mut().poll(&mut cx).is_pending());
    assert!(third.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.control.send("probe".into()).await, Sent::Gone);
    // The driver has handled all three by now.
    assert_eq!(a.as_mut().poll(&mut cx), Poll::Ready(Sent::Sent));
    assert_eq!(b.as_mut().poll(&mut cx), Poll::Ready(Sent::Sent));
    assert!(
        third.as_mut().poll(&mut cx).is_pending(),
        "the third waits while more than the window is in flight"
    );

    // The client reads and acknowledges: the third goes out, all whole and
    // in order.
    let mut got = Vec::new();
    while got.len() < 3 {
        let m = c
            .until("a big message on the client", |_, e| match e {
                Event::ChannelData(d) => Some(String::from_utf8(d.data).unwrap()),
                _ => None,
            })
            .await;
        got.push(m);
    }
    for (m, ch) in got.iter().zip(['a', 'b', 'c']) {
        assert_eq!(*m, big(ch), "whole, and in order");
    }
    assert_eq!(third.await, Sent::Sent);
}

#[tokio::test]
async fn a_message_over_the_largest_is_refused_naming_whose_bound() {
    let calls = calls_on("127.0.0.1:0").await;
    // `realtime.max_message_mb` below the client's 256 KiB.
    let (mut c, sdp) = Client::new(ip(LO4)).await;
    let mut o = offer(sdp, LO4);
    o.params.max_message = Some(1000);
    let answered = calls.offer(o).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    connect(&mut c, &mut h).await;
    assert_eq!(
        h.control.send("x".repeat(2000)).await,
        Sent::TooLarge {
            len: 2000,
            limit: 1000,
            by: Limit::Setting
        }
    );
    assert_eq!(h.control.send("x".repeat(1000)).await, Sent::Sent);

    // The client's own bound below lmgw's.
    let (mut c, sdp) = Client::new(ip(LO4)).await;
    let answered = calls.offer(offer(sdp, LO4)).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    connect(&mut c, &mut h).await;
    let len = 300 * 1024;
    assert_eq!(
        h.control.send("x".repeat(len)).await,
        Sent::TooLarge {
            len,
            limit: 256 * 1024,
            by: Limit::Client
        }
    );
}
