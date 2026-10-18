//! The close's order (§8.1's End, §8.2): lmgw's last event reaches the
//! phone before `Rtc::close`, its first transmission lost or not, and a
//! phone that never acknowledges it has the call closed without it one
//! `realtime.ping_interval_s` after the close began; with the interval at
//! 0 the call stays closing, shown as such, until a hangup closes it at
//! once. The phone's own close applies what it sent before it — a stop and
//! a push-to-talk commit among it (§13.1's End) — ends the call as a hangup
//! does, and nothing is said to the phone after it.

use std::time::Duration;

use lmgw_core::realtime::calls::CallState;
use lmgw_core::server::STOP_WITHIN;
use serde_json::json;
use str0m::Event;
use tokio::time::Instant;

use super::{closed_last, removed_at, voice_ended, BoundCall};
use crate::common::captured_log::capture_log_at;
use crate::common::patience::{until, within};
use crate::realtime_calls::hangup;

/// The phone loses everything lmgw sends from the hangup until the
/// session's writer closes the call — which it does once str0m took every
/// event before the close, the hangup's `lmgw.session.closed` last — so the
/// event's first sending is lost whichever of the 200 and the writer's
/// close comes first. The call let go when the 200 came (closing from
/// then), but keeps its link until SCTP delivered the event: the
/// retransmission reaches the phone once it reads again, and only then the
/// close. A close at once would have taken the event with it.
#[tokio::test]
async fn the_last_event_is_delivered_before_the_close_though_its_first_sending_was_lost() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let mut c = BoundCall::start(|_| {}).await;
    c.settled().await;
    let owner = c.w.gw.client();
    let id = c.phone.id.clone();
    let closing = format!("WebRTC call {id}: closing (");
    let w = &c.w;
    let status = c
        .phone
        .c
        .discard_while(async {
            let status = hangup(w, &owner, &id).await;
            until("the writer's close", || log.text().contains(&closing)).await;
            status
        })
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        c.w.state.calls.get(&id).map(|i| i.state),
        Some(CallState::Closing),
        "let go at the 200, its last event not delivered"
    );
    let events = c.phone.closed().await;
    let (closed, _) = closed_last(&events);
    assert_eq!(
        (&closed["kind"], &closed["by"]),
        (&json!("closed"), &json!("the dashboard"))
    );
}

/// The close's wait for delivery is one `realtime.ping_interval_s` at most
/// (§8.2): a phone frozen as the hangup comes acknowledges nothing, and the
/// call is closed without its last event exactly one interval after its
/// close began, said at INFO naming the setting. On the paused clock, the
/// hangup asked of lmgw directly.
#[tokio::test]
async fn a_phone_that_never_acknowledges_the_last_event_is_closed_one_interval_on() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let every = Duration::from_secs(5);
    let mut c = BoundCall::start(|s| s.realtime.ping_interval_s = 5).await;
    c.settled().await;
    let id = c.phone.id.clone();
    tokio::time::pause();
    // Frozen from here: nothing drives the phone.
    let began = Instant::now();
    let by = Some("the dashboard".to_string());
    let calls = &c.w.state.calls;
    within("the hangup", calls.hangup_within(&id, by, STOP_WITHIN))
        .await
        .expect("the call is there");
    assert_eq!(calls.get(&id).map(|i| i.state), Some(CallState::Closing));
    let gone = removed_at(&c.w, &id).await;
    // Met at the bound: the driver wakes for it (the closing call's
    // deadline), not at str0m's next timeout; give or take one poll.
    let got = gone - began;
    assert!(
        got >= every && got <= every + Duration::from_millis(20),
        "closed after {got:?}, the bound being {every:?}"
    );
    let said = format!(
        "WebRTC call {id}: the client did not acknowledge the last events within 5 s \
         (realtime.ping_interval_s); closed without them"
    );
    assert!(log.text().contains(&said), "{}", log.text());
}

/// With `realtime.ping_interval_s` at 0 the close's wait for delivery has
/// no bound (§8.2), so a call hung up while its phone is frozen stays up,
/// closing, its slot back. Nothing hides it: the registry shows it
/// `closing`, and a hangup of it closes it at once and answers 200.
#[tokio::test]
async fn a_closing_call_is_shown_closing_and_a_hangup_closes_it_at_once() {
    let (log, _guard) = capture_log_at(tracing::Level::INFO);
    let mut c = BoundCall::start(|s| s.realtime.ping_interval_s = 0).await;
    c.settled().await;
    let owner = c.w.gw.client();
    let id = c.phone.id.clone();
    // Frozen from here: nothing drives the phone, which acknowledges
    // nothing.
    assert_eq!(c.phone.hangup(&c.w, &owner).await, 200);
    let closing = format!("WebRTC call {id}: closing (");
    until("the writer's close", || log.text().contains(&closing)).await;
    let calls = &c.w.state.calls;
    assert_eq!(calls.get(&id).map(|i| i.state), Some(CallState::Closing));
    let listed: Vec<_> = calls
        .list()
        .into_iter()
        .map(|i| (i.id.to_string(), i.state))
        .collect();
    assert_eq!(listed, [(id.clone(), CallState::Closing)]);

    assert_eq!(c.phone.hangup(&c.w, &owner).await, 200);
    assert!(calls.get(&id).is_none(), "closed at once");
    assert!(calls.list().is_empty());
    let said = format!("WebRTC call {id}: hung up while it closed");
    assert!(log.text().contains(&said), "{}", log.text());
    assert_eq!(c.phone.hangup(&c.w, &owner).await, 404, "gone");
}

/// The phone sends an event and closes `oai-events` right behind it: the
/// session handles the event first (here a `lmgw.session.take_over` it
/// ignores, said at DEBUG), then ends as after a hangup — the feed says
/// `voice.ended {reason: "closed"}` — and lmgw sends nothing more: no
/// `lmgw.session.closed`, no close of lmgw's own, only the end of the call.
#[tokio::test]
async fn the_phones_own_close_applies_what_came_before_it_and_hears_nothing_after() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let mut c = BoundCall::start(|_| {}).await;
    c.settled().await;
    let sid = c.w.state.calls.get(&c.phone.id).unwrap().session.unwrap();
    c.phone.send(json!({"type": "lmgw.session.take_over"}));
    c.phone.c.close_channel();
    let mut after = Vec::new();
    c.phone
        .c
        .until("lmgw ended the call", |_, e| match e {
            Event::ChannelData(d) => {
                // The connect warm's states come when they come, the close
                // racing them (`Phone::closed`'s rule).
                let v: serde_json::Value = serde_json::from_slice(&d.data).unwrap();
                if v["type"] != "lmgw.model.state" {
                    after.push(v);
                }
                None
            }
            Event::Closed => Some(()),
            _ => None,
        })
        .await;
    assert!(
        after.is_empty(),
        "nothing after the phone's close: {after:?}"
    );
    let text = log.text();
    let handled = text
        .find(&format!(
            "realtime {sid}: lmgw.session.take_over outside a pending bind"
        ))
        .unwrap_or_else(|| panic!("the event before the close was handled: {text}"));
    let ended = text
        .find(&format!("realtime {sid}: session ended"))
        .unwrap_or_else(|| panic!("the session ended: {text}"));
    assert!(handled < ended, "{text}");
    assert!(
        !text.contains(&format!("WebRTC call {}: closing (", c.phone.id)),
        "no close of lmgw's own: {text}"
    );
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

/// §13.1's End: while an answer plays the user presses and speaks, then the
/// phone sends `lmgw.response.stop`, the push-to-talk commit and closes
/// `oai-events` right behind them. Both are applied before the call ends:
/// the answer cancelled (its reply stored, cut) and the turn transcribed and
/// stored after it; then the call ends as after a hangup (`voice.ended
/// {reason: "closed"}`), and nothing reaches the phone after its close.
#[tokio::test]
async fn a_stop_and_a_commit_right_before_the_phones_close_are_both_applied() {
    use crate::realtime_calls_audio::opus;
    use crate::realtime_chat_thread::manual_update;
    use crate::support::realtime_audio::{fixture, silence};
    use crate::support::realtime_fakes::Turn;
    use crate::support::realtime_tts::{speech, wav, Tts};

    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let mut c = BoundCall::start(|_| {}).await;
    // A short first clause, heard whole before the stop, and a long second.
    c.w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    // The second clause is far longer than the test's pacing needs (about
    // 2 s from its start to the stop), so no lag of the machine ends it first.
    let long = fixture("en_two_sentences_pause.wav").repeat(6);
    c.w.tts.push(Tts::Wav(wav(&long, 24_000)));
    c.w.chat.push(Turn::text(&[
        "Trains leave hourly. ",
        "The next one is at noon.",
    ]));
    let sid = c.w.state.calls.get(&c.phone.id).unwrap().session.unwrap();
    c.phone.send(manual_update(0));
    assert_eq!(c.phone.next_but_states().await["type"], "session.updated");
    let turn = opus(&silence(200));
    for f in &turn {
        c.phone.c.send_frame_now(f);
        c.phone.c.run_for(Duration::from_millis(20)).await;
    }
    c.phone.send(json!({"type": "input_audio_buffer.commit"}));
    c.phone.send(json!({"type": "response.create"}));
    loop {
        if c.phone.next_but_states().await["type"] == "output_audio_buffer.started" {
            break;
        }
    }
    c.phone.c.run_for(Duration::from_millis(1_000)).await;
    // The press: the user's words, then the stop, the release's commit and
    // the close, back to back.
    for f in &turn {
        c.phone.c.send_frame_now(f);
        c.phone.c.run_for(Duration::from_millis(20)).await;
    }
    // What came before the close is read; nothing is read until it is sent.
    c.phone.c.met(|_| false);
    c.phone.send(json!({"type": "lmgw.response.stop"}));
    c.phone.send(json!({"type": "input_audio_buffer.commit"}));
    c.phone.c.close_channel();
    let mut after = Vec::new();
    c.phone
        .c
        .until("lmgw ended the call", |_, e| match e {
            Event::ChannelData(d) => {
                after.push(String::from_utf8(d.data).unwrap());
                None
            }
            Event::Closed => Some(()),
            _ => None,
        })
        .await;
    assert!(
        after.is_empty(),
        "nothing after the phone's close: {after:?}"
    );
    let frames = c.feed_after().await;
    assert_eq!(
        voice_ended(&frames),
        [
            json!({"thread_id": c.tid, "by": "the dashboard", "reason": "closed",
                "taken_over_by": null})
        ],
        "{frames:#?}"
    );
    let text = log.text();
    let stopped = text
        .find(&format!(
            "realtime {sid}: lmgw.response.stop stopped response"
        ))
        .unwrap_or_else(|| panic!("the stop was applied: {text}"));
    let ended = text
        .find(&format!("realtime {sid}: session ended"))
        .unwrap_or_else(|| panic!("the session ended: {text}"));
    assert!(stopped < ended, "{text}");
    // The thread holds the first turn, the answer as heard — its first
    // clause whole, the second cut where it was stopped — then the turn
    // committed after the stop.
    let messages = c.w.messages(c.tid).await;
    let roles: Vec<&str> = messages.iter().map(|(role, _, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{messages:?}");
    let reply = &messages[1].1;
    assert!(
        reply.starts_with("Trains leave hourly.") && !reply.ends_with("noon."),
        "{reply}"
    );
}
