//! `lmgw.session.closed` for each kind of a running bound session's end a
//! call on the plain listener meets (§8.2's table), the last event before
//! the close, with its kind, `by` and message, and what `lmgw-client` reads
//! of it; beside it a paired device's feed, as §8.2's last column and §8.4's
//! contract say. A dead or stalled verdict's are `liveness`', the phone's own
//! close is `end`'s, a thread out of reach's waits for a device's own call
//! (calls WP7; module doc of `realtime_calls_close`).
//!
//! The call is owner key `phone`'s, which the feed names as it names every
//! owner principal, "the dashboard"; the takeover comes from device
//! `tablet`, so the new binder (`by`, `taken_over_by`) and the call's own
//! binder (`voice.ended.by`) are told apart by name.

use std::time::Duration;

use lmgw_client::realtime::{CloseKind, RevokeKind};
use serde_json::{json, Value};

use super::{closed_last, voice_ended, BoundCall};
use crate::common::captured_log::capture_log_at;
use crate::common::patience::within;
use crate::device_chat::op;

#[tokio::test]
async fn a_takeover_says_who_took_it_and_the_feed_says_voice_ended_taken_over() {
    let mut c = BoundCall::start(|_| {}).await;
    // Device `tablet` binds the thread from a WebSocket on the device
    // listener (takeover=always, the default).
    let mut tablet =
        c.w.connect_as(&c.device, &format!("chat_thread={}", c.tid))
            .await
            .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let created = crate::realtime_chat_thread::next(&mut tablet).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let events = c.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"], &closed["message"]),
        (
            &json!("taken_over"),
            &json!("device 'tablet'"),
            &json!("voice mode moved to device 'tablet'")
        )
    );
    assert_eq!(read.kind, CloseKind::TakenOver);
    assert_eq!(read.by.as_deref(), Some("device 'tablet'"));
    assert!(
        events
            .iter()
            .any(|e| e["error"]["code"] == "chat_thread_taken_over"),
        "{events:#?}"
    );
    let frames = c.feed_after().await;
    assert_eq!(
        voice_ended(&frames),
        [
            json!({"thread_id": c.tid, "by": "the dashboard", "reason": "taken_over",
                "taken_over_by": "device 'tablet'"})
        ],
        "{frames:#?}"
    );
    drop(tablet);
}

/// The hangup comes from the owner's own client, a principal other than
/// the calling key: the closed event's `by` is who hung up, the feed's
/// `voice.ended.by` the call's binder. Both are owner principals, so both
/// read "the dashboard" (module doc); a non-owner cannot bind a thread on
/// the plain listener, so no hangup there names them apart.
#[tokio::test]
async fn a_hangup_is_closed_by_who_hung_up_and_the_feed_says_voice_ended_closed() {
    let mut c = BoundCall::start(|_| {}).await;
    assert_eq!(c.phone.hangup(&c.w, &c.w.gw.client()).await, 200);
    let events = c.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"]),
        (&json!("closed"), &json!("the dashboard"))
    );
    assert_eq!(read.kind, CloseKind::Other { code: None });
    assert_eq!(read.by.as_deref(), Some("the dashboard"));
    let frames = c.feed_after().await;
    assert_eq!(
        voice_ended(&frames),
        [
            json!({"thread_id": c.tid, "by": "the dashboard", "reason": "closed",
                "taken_over_by": null})
        ],
        "{frames:#?}"
    );
}

/// The calling key is disabled: `revoked`, its message led by the
/// `RevokeKind` token a WebSocket's 4003 reason starts with, and the feed
/// says `voice.ended {reason: "revoked"}`.
#[tokio::test]
async fn a_revoked_key_says_how_and_the_feed_says_voice_ended_revoked() {
    let mut c = BoundCall::start(|_| {}).await;
    let (s, v) = op(&c.w, "key_set", json!({ "id": c.key, "enabled": false })).await;
    assert_eq!(s, 200, "{v}");
    let events = c.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"], &closed["message"]),
        (
            &json!("revoked"),
            &Value::Null,
            &json!("revoked: owner key 'phone' was disabled")
        )
    );
    // A non-device key's kind; a device's own call's would be
    // DeviceDisabled (calls WP7).
    assert_eq!(
        read.kind,
        CloseKind::Revoked {
            kind: RevokeKind::Revoked
        }
    );
    let frames = c.feed_after().await;
    assert_eq!(
        voice_ended(&frames),
        [
            json!({"thread_id": c.tid, "by": "the dashboard", "reason": "revoked",
                "taken_over_by": null})
        ],
        "{frames:#?}"
    );
}

/// lmgw stops, both its listeners (the plain one the call's POST came in
/// on, the device one the device's feed reads through): `shutting_down`,
/// and the feed ends with it.
#[tokio::test]
async fn a_stopping_server_says_shutting_down_and_the_feed_ends() {
    let mut c = BoundCall::start(|_| {}).await;
    c.w.state.stops.stop();
    c.w.state.stops.stop_of(lmgw_core::server::Via::Devices);
    let events = c.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"], &closed["message"]),
        (
            &json!("shutting_down"),
            &Value::Null,
            &json!(lmgw_core::server::STOPPING)
        )
    );
    assert_eq!(read.kind, CloseKind::ShuttingDown);
    c.feed.until(10, |_| false).await;
    assert!(c.feed.ended, "the feed ends");
}

/// A frame's payload: not Opus (nothing decodes it before WP3), 1000 bytes.
const PAYLOAD: [u8; 1000] = [7; 1000];

/// The largest datagram that still reaches a phone reading nothing
/// (`Client::lose_over`): SCTP's acknowledgements of the phone's own
/// messages are a few dozen bytes, a packet of lmgw's events hundreds and
/// more.
const LOST_OVER: usize = 200;

/// Small events, each answered: more than the writer's lane holds (64).
const SMALL: usize = 70;

/// What waits for the session passes `realtime.max_message_mb` (1 MiB, its
/// smallest) while the phone reads nothing of lmgw's output: the writer
/// waits at str0m's window, the session behind the writer's full lane in
/// its flush, the reader behind the session's full inbox, and the phone's
/// microphone fills the call's queue (RTP needs no acknowledgement). Past
/// the budget the driver queues nothing more and the call stays up; the
/// session closes as for any other end, so once the phone reads again every
/// answer comes, then `lmgw.session.closed {kind: "closed"}` naming the
/// setting, last, then the close.
#[tokio::test]
async fn too_big_says_closed_naming_the_setting_last() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let mut c = BoundCall::start(|s| {
        s.realtime.ping_interval_s = 0;
        s.realtime.max_message_mb = 1;
    })
    .await;
    c.settled().await;
    let id = c.phone.id.clone();
    c.phone.c.lose_over(Some(LOST_OVER));
    // Each answer quotes its event's type: three of 100 000 bytes are more
    // than str0m's 128 KiB window, so the writer waits at the third.
    let big = || json!({"type": "t".repeat(100_000)});
    for _ in 0..3 {
        c.phone.send(big());
        c.phone.c.until_acknowledged().await;
    }
    // Answers enough to fill the writer's lane (64 events), so the
    // session's flush waits behind it.
    for k in 0..SMALL {
        c.phone.send(json!({"type": format!("x{k}")}));
    }
    c.phone.c.until_acknowledged().await;
    // And three more of 100 000 bytes fill what the reader holds ahead of
    // the session (256 KiB).
    for _ in 0..3 {
        c.phone.send(big());
        c.phone.c.until_acknowledged().await;
    }
    let over = |text: &str| {
        text.lines().any(|l| {
            l.contains(&format!("WebRTC call {id}: ")) && l.contains("over realtime.max_message_mb")
        })
    };
    within("the call's queue past the budget", async {
        while !over(&log.text()) {
            for _ in 0..16 {
                c.phone.c.send_frame_now(&PAYLOAD);
            }
            c.phone.c.run_for(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(c.w.state.calls.get(&id).is_some(), "the call is up");

    c.phone.c.lose_over(None);
    let events = c.phone.closed().await;
    let (closed, read) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"]),
        (&json!("closed"), &Value::Null)
    );
    let message = closed["message"].as_str().unwrap();
    assert!(
        message.contains("over realtime.max_message_mb (1048576 bytes)"),
        "{message}"
    );
    assert_eq!(read.kind, CloseKind::Other { code: None });
    let answered = events.iter().filter(|e| e["type"] == "error").count();
    assert_eq!(answered, 6 + SMALL, "every event answered before the close");
    assert!(
        log.text().contains(&format!("closed — {message}")),
        "{}",
        log.text()
    );
}
