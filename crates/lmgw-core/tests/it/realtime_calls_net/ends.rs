//! How a call ends besides its owner's close and the client's: the
//! never-opened bound, an offer dropped after the driver took its call, and
//! a group's close, which asks the owners first and closes the rest after
//! `STOP_WITHIN`; and the budget of what waits for the owner, past which the
//! owner is told and ends the call itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use lmgw_core::realtime::calls::{CallEvent, Ended, Ending, Group, GroupState, Message, Sent};
use lmgw_core::server::STOP_WITHIN;
use str0m::Event;

use super::{call, call_event, calls_on, connect, ip, offer, Client, Kept, LO4};
use crate::common::patience::{until, within};

#[tokio::test]
async fn a_call_whose_events_channel_never_opens_is_ended_by_the_bound() {
    let calls = calls_on("127.0.0.1:0").await;
    // Its one channel is not `oai-events`: it opens, and the call waits on.
    let (mut c, sdp) = Client::labelled(ip(LO4), "not-oai-events").await;
    let mut o = offer(sdp, LO4);
    o.params.never_opened = Some(Duration::from_secs(1));
    let answered = calls.offer(o).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    call_event(&mut c, &mut h.events, "the call connected", |e| {
        matches!(e, CallEvent::Connected).then_some(())
    })
    .await;
    let why = call_event(&mut c, &mut h.events, "the call's end", |e| match e {
        CallEvent::Open => panic!("a channel that is not oai-events opened the call"),
        CallEvent::Ended(why) => Some(why),
        _ => None,
    })
    .await;
    assert_eq!(why, Ended::NeverOpened);
    assert!(calls.get(h.id()).is_none());
}

/// Past the budget the driver queues nothing more and hands the owner the
/// overrun behind what it queued; the call stays up until the owner ends
/// it, so a bound call's session can say why first (§8.2).
#[tokio::test]
async fn what_waits_for_the_owner_past_its_budget_is_handed_over_and_the_owner_ends_the_call() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut c, sdp) = Client::new(ip(LO4)).await;
    let mut o = offer(sdp, LO4);
    // `realtime.max_message_mb`'s bytes, small for the test.
    o.params.max_message = Some(4000);
    let answered = calls.offer(o).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    connect(&mut c, &mut h).await;
    // The owner reads nothing while the client sends 4 × 1500 bytes: the
    // third passes the budget, the fourth is dropped.
    for k in 0..4 {
        c.send_text(&k.to_string().repeat(1500));
    }
    c.until_acknowledged().await;
    let mut messages = 0;
    let over = within("the overrun", async {
        loop {
            match h.events.next().await {
                Some(CallEvent::Message {
                    message: Message::Text(_),
                    ..
                }) => messages += 1,
                Some(CallEvent::TooBig { queued, limit }) => return (queued, limit),
                Some(_) => {}
                None => panic!("no overrun"),
            }
        }
    })
    .await;
    assert_eq!(over, (4500, 4000));
    assert_eq!(messages, 3, "what was queued is still handed out");
    assert!(
        h.events.try_next().is_none(),
        "nothing after the overrun but the end"
    );
    assert!(calls.get(h.id()).is_some(), "the call is up");
    assert!(!c.met(|e| matches!(e, Event::Closed)), "and not closed");
    h.control.close_delivered().await;
    c.until("the client sees the call closed", |_, e| {
        matches!(e, Event::Closed).then_some(())
    })
    .await;
    let end = within("the call's end", h.events.next()).await;
    assert!(
        matches!(end, Some(CallEvent::Ended(Ended::Closed))),
        "{end:?}"
    );
    assert!(calls.get(h.id()).is_none());
}

/// The handle's own path for a POST that went away after the driver took
/// its call: the driver heard its answer taken, so only the handle's drop
/// (inside the dropped offer) closes the call. The driver's path for a
/// registration nobody hears is `calls::tests`'.
#[tokio::test]
async fn an_offer_dropped_after_the_driver_took_its_call_leaves_no_call() {
    let calls = calls_on("127.0.0.1:0").await;
    // A call of its own, never connected: a send on it goes through the
    // driver behind whatever was asked of it before.
    let (_p, sdp) = Client::new(ip(LO4)).await;
    let probe = calls.offer(offer(sdp, LO4)).await.unwrap().handle;
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let kept = Arc::new(AtomicBool::new(false));
    let mut o = offer(sdp, LO4);
    o.keep = Some(Box::new(Kept(kept.clone())));
    let mut answering = Box::pin(calls.offer(o));
    // Polled while the offer is read and answered on blocking threads, until
    // its call is in the registry, and not once after: the driver's answer,
    // which wakes this task, is checked for first and never taken.
    let handed = std::future::poll_fn(|_| match calls.list().len() {
        2 => Poll::Ready(()),
        _ => Poll::Pending,
    });
    within("the call handed to the driver", async {
        tokio::select! {
            biased;
            () = handed => {}
            _ = &mut answering => panic!("the offer was polled to its answer"),
        }
    })
    .await;
    // Answered after the registration: the driver has taken the call and
    // handed its answer to the offer, which nobody polls any more.
    assert_eq!(probe.control.send("probe".into()).await, Sent::Gone);
    assert_eq!(calls.list().len(), 2, "taken, and its answer heard");
    assert!(!kept.load(Ordering::SeqCst));
    drop(answering);
    until("the dropped offer's call closed", || {
        kept.load(Ordering::SeqCst) && calls.list().len() == 1
    })
    .await;
}

/// The bound is `STOP_WITHIN` itself; the paused clock lets the test pass
/// it without waiting it out.
#[tokio::test(start_paused = true)]
async fn a_groups_close_ends_a_call_whose_owner_never_answers_after_stop_within() {
    let calls = calls_on("127.0.0.1:0").await;
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let mut h = calls.offer(offer(sdp, LO4)).await.unwrap().handle;
    let began = tokio::time::Instant::now();
    calls.close_group(Group::Plain).await;
    assert!(began.elapsed() >= STOP_WITHIN, "{:?}", began.elapsed());
    assert_eq!(calls.group(Group::Plain), GroupState::Down);
    assert_eq!(h.control.ending().await, Ending::ShuttingDown);
    let why = loop {
        match h.events.next().await {
            Some(CallEvent::Ended(why)) => break why,
            Some(_) => {}
            None => panic!("no end"),
        }
    };
    assert_eq!(why, Ended::GroupClosed);
    assert!(calls.list().is_empty());
}

#[tokio::test]
async fn a_groups_close_asks_the_owner_first_and_its_last_word_goes_out() {
    let calls = calls_on("127.0.0.1:0").await;
    let (mut a, mut ha, _) = call(&calls, LO4, LO4).await;
    connect(&mut a, &mut ha).await;
    // The owner says its last word and closes when asked.
    let owner = tokio::spawn(async move {
        let why = ha.control.ending().await;
        let sent = ha.control.send("goodbye".into()).await;
        ha.control.close().await;
        (why, sent, ha)
    });
    // The close returns once the call is gone, the client driven meanwhile.
    let closing = calls.close_group(Group::Plain);
    tokio::pin!(closing);
    within("the group closed", async {
        loop {
            tokio::select! {
                () = &mut closing => break,
                () = a.run_for(Duration::from_millis(5)) => {}
            }
        }
    })
    .await;
    assert_eq!(calls.group(Group::Plain), GroupState::Down);
    let (why, sent, mut ha) = owner.await.unwrap();
    assert_eq!((why, sent), (Ending::ShuttingDown, Sent::Sent));
    let last = a
        .until("the last word on the client", |_, e| match e {
            Event::ChannelData(d) => Some(String::from_utf8(d.data).unwrap()),
            _ => None,
        })
        .await;
    assert_eq!(last, "goodbye");
    assert!(matches!(
        within("the call's end", ha.events.next()).await,
        Some(CallEvent::Ended(Ended::Closed))
    ));
    assert!(calls.list().is_empty());
}

/// A hangup whose owner never reacts (a session stuck behind a client that
/// stopped reading) is bounded by `STOP_WITHIN`, the bound a group's close
/// gives the same owners, and then closes the call itself (§2.6, §8); the
/// paused clock passes it without waiting it out.
#[tokio::test(start_paused = true)]
async fn a_hangup_whose_owner_never_reacts_closes_the_call_after_stop_within() {
    let calls = calls_on("127.0.0.1:0").await;
    let (_c, sdp) = Client::new(ip(LO4)).await;
    let dropped = Arc::new(AtomicBool::new(false));
    let mut o = offer(sdp, LO4);
    o.keep = Some(Box::new(Kept(dropped.clone())));
    let mut h = calls.offer(o).await.unwrap().handle;
    let id = h.id().to_string();
    let began = tokio::time::Instant::now();
    let hung = calls.hangup_within(&id, Some("the dashboard".into()), STOP_WITHIN);
    assert_eq!(hung.await, Some(()));
    assert!(began.elapsed() >= STOP_WITHIN, "{:?}", began.elapsed());
    assert!(calls.get(&id).is_none(), "closed by force");
    assert!(dropped.load(Ordering::SeqCst), "its slot let go");
    // The owner, had it looked, was asked; its call ended under it.
    assert!(matches!(h.control.ending().await, Ending::HungUp { .. }));
    let why = loop {
        match h.events.next().await {
            Some(CallEvent::Ended(why)) => break why,
            Some(_) => {}
            None => panic!("no end"),
        }
    };
    assert_eq!(why, Ended::Closed);
    // A call that is gone is no hangup's.
    assert_eq!(calls.hangup_within(&id, None, STOP_WITHIN).await, None);
}
