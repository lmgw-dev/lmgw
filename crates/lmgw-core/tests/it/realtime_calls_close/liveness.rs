//! A call's liveness (§8.1): the phone's traffic says it is there, its
//! acknowledgements that it reads, and either missing for one whole
//! `realtime.ping_interval_s` ends the call; the never-connected and
//! never-opened bounds are the same interval. On the paused clock (module
//! doc of `realtime_calls_close`).

use std::time::Duration;

use lmgw_core::realtime::calls::CallState;
use serde_json::json;
use str0m::Event;
use tokio::time::Instant;

use super::{at_the_bound, closed_last, let_go_at, voice_ended, BoundCall, SILENCE};
use crate::common::captured_log::capture_log_at;
use crate::common::patience::until;
use crate::realtime_calls::{calls_world, post_sdp, Phone};
use crate::realtime_calls_net::client::Client;

/// A session update the session answers with `session.updated`: output the
/// phone has to acknowledge.
fn update() -> serde_json::Value {
    json!({"type": "session.update", "session": {"type": "realtime"}})
}

/// Two and a half intervals of a phone that streams its microphone (a
/// frame every 20 ms), asks something every half second and reads the
/// answers: the call is up all along, no ping having been sent, and every
/// answer came.
#[tokio::test]
async fn a_call_stays_up_past_two_intervals_while_the_phone_sends_and_reads() {
    let mut d = BoundCall::start(|s| s.realtime.ping_interval_s = 2).await;
    d.settled().await;
    tokio::time::pause();
    let began = Instant::now();
    let mut asked = 0;
    let mut frames = 0u64;
    while began.elapsed() < Duration::from_secs(5) {
        d.phone.c.send_frame_now(SILENCE);
        if frames.is_multiple_of(25) {
            d.phone.send(update());
            asked += 1;
        }
        frames += 1;
        d.phone.c.run_for(Duration::from_millis(20)).await;
    }
    let info = d.w.state.calls.get(&d.phone.id).expect("the call is up");
    assert_eq!(info.state, CallState::Running);
    let mut answered = 0;
    let mut closed = false;
    while d.phone.c.met(|e| match e {
        Event::ChannelData(c) => {
            let v: serde_json::Value = serde_json::from_slice(&c.data).unwrap();
            assert_ne!(v["type"], "lmgw.session.closed", "{v}");
            v["type"] == "session.updated"
        }
        Event::Closed => {
            closed = true;
            true
        }
        _ => false,
    }) {
        answered += 1;
    }
    assert!(!closed, "the call was not closed");
    assert_eq!(answered, asked, "every update answered");
}

/// A phone that goes silent — no consent check, no RTP, no SCTP — is ended
/// one interval after the last thing lmgw heard from it, its session's log
/// naming the setting; the call lets go then. Its `lmgw.session.closed` is
/// handed to the writer, which has room, so it is sent before the close
/// (`say_closed_now`): the phone, frozen rather than gone, finds it ahead
/// of the close when it reads again. That holds whether the close waited
/// for its delivery or not — the frozen phone loses nothing — so the
/// delivery itself is `end`'s. The device's feed says the call ended
/// `closed`.
#[tokio::test]
async fn a_phone_that_goes_silent_is_ended_one_interval_after_it_was_last_heard() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let every = Duration::from_secs(4);
    let mut d = BoundCall::start(|s| s.realtime.ping_interval_s = 4).await;
    d.settled().await;
    tokio::time::pause();
    // Frozen from here: nothing drives the phone. What it sent last is
    // taken first.
    tokio::time::sleep(Duration::from_millis(1)).await;
    let heard =
        d.w.state
            .calls
            .get(&d.phone.id)
            .unwrap()
            .last_heard
            .unwrap();
    let gone = let_go_at(&d.w, &d.phone.id).await;
    at_the_bound("the silent call's end", gone - heard, every);
    let said = "closed — no traffic from the client within 4 s, setting realtime.ping_interval_s";
    assert!(log.text().contains(said), "{}", log.text());

    // What lmgw sent waits in the phone's socket: the event, then the
    // close.
    tokio::time::resume();
    let events = d.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"], &closed["message"]),
        (
            &json!("closed"),
            &serde_json::Value::Null,
            &json!("no traffic from the client within 4 s, setting realtime.ping_interval_s")
        )
    );
    assert_eq!(
        read.kind,
        lmgw_client::realtime::CloseKind::Other { code: None }
    );
    let frames = d.feed_after().await;
    let ended = voice_ended(&frames);
    assert_eq!(
        ended,
        [
            json!({"thread_id": d.tid, "by": "the dashboard", "reason": "closed",
                "taken_over_by": null})
        ],
        "{frames:#?}"
    );
}

/// A phone that keeps its microphone streaming but stopped reading — the
/// answer to its last event never acknowledged — is ended stalled one
/// interval after lmgw's output began to wait, though its traffic never
/// stopped. Its `lmgw.session.closed` is handed to the writer only because
/// the writer takes it at once (`say_closed_now`: one answer in flight is
/// within str0m's window), and str0m takes it behind that answer before
/// the close, so the phone finds both ahead of the close when it reads
/// again — as it would after a close at once; the delivery itself is
/// `end`'s.
#[tokio::test]
async fn a_phone_that_sends_but_stops_reading_is_ended_stalled_after_one_interval() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let every = Duration::from_secs(2);
    let mut d = BoundCall::start(|s| s.realtime.ping_interval_s = 2).await;
    d.settled().await;
    tokio::time::pause();
    // Its last event; from here it only sends frames, and reads nothing.
    d.phone.send(update());
    let calls = d.w.state.calls.clone();
    let mut waiting = None;
    let gone = loop {
        d.phone.c.send_frame_now(SILENCE);
        tokio::time::sleep(Duration::from_millis(20)).await;
        match calls.get(&d.phone.id) {
            Some(info) if info.state != CallState::Closing => {
                waiting = waiting.or(info.unacked_since);
                let heard = info.last_heard.unwrap();
                assert!(
                    Instant::now() - heard < every,
                    "the phone was heard all along"
                );
            }
            // Let go: closing, or gone.
            _ => break Instant::now(),
        }
    };
    let waiting = waiting.expect("the answer waited for its acknowledgement");
    at_the_bound("the stalled call's end", gone - waiting, every);
    let text = log.text();
    assert!(
        text.contains(
            "closed — the client acknowledged none of the session's output within 2 s, setting \
             realtime.ping_interval_s"
        ),
        "{text}"
    );
    assert!(!text.contains("no traffic from the client"), "{text}");

    tokio::time::resume();
    let events = d.phone.closed().await;
    assert!(
        events.iter().any(|e| e["type"] == "session.updated"),
        "{events:#?}"
    );
    let (closed, _) = closed_last(&events);
    assert_eq!(closed["kind"], "closed");
    assert!(
        closed["message"]
            .as_str()
            .unwrap()
            .starts_with("the client acknowledged none"),
        "{closed}"
    );
}

/// The never-connected and never-opened bounds (§8.1): a call whose ICE
/// never connects ends one interval after its answer, one whose
/// `oai-events` never opens one interval after it connected, each with a
/// log line saying which and naming the setting; neither ever had a
/// session.
#[tokio::test]
async fn a_call_that_never_connects_or_never_opens_ends_one_interval_on() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let every = Duration::from_secs(3);
    let w = calls_world(|s| s.realtime.ping_interval_s = 3).await;
    let owner = w.gw.client();

    let quiet = Phone::call(&w, &owner, "model=chatty").await;
    let (c, sdp) = Client::labelled("127.0.0.1".parse().unwrap(), "not-oai-events").await;
    let a = post_sdp(&w, &owner, "model=chatty", &sdp).await;
    let mut shut = Phone::answered(c, a);
    shut.c
        .until("the channel-less client connected", |_, e| {
            matches!(e, Event::Connected).then_some(())
        })
        .await;
    until("the call connected", || {
        w.state
            .calls
            .get(&shut.id)
            .is_some_and(|i| i.connected.is_some())
    })
    .await;
    tokio::time::pause();
    let answered = w.state.calls.get(&quiet.id).unwrap().created;
    let connected = w.state.calls.get(&shut.id).unwrap().connected.unwrap();
    // Frozen from here: neither phone is driven.
    let first = let_go_at(&w, &quiet.id).await;
    at_the_bound("the never-connected call's end", first - answered, every);
    let second = let_go_at(&w, &shut.id).await;
    at_the_bound("the never-opened call's end", second - connected, every);
    let text = log.text();
    for said in [
        format!(
            "WebRTC call {}: ICE and DTLS did not connect within 3 s of the answer \
             (realtime.ping_interval_s); ended",
            quiet.id
        ),
        format!(
            "WebRTC call {}: the `oai-events` channel did not open within 3 s of connecting \
             (realtime.ping_interval_s); ended",
            shut.id
        ),
    ] {
        assert!(text.contains(&said), "{said}\n{text}");
    }
    assert!(
        !text.contains("on WebRTC call"),
        "no session started: {text}"
    );
}
