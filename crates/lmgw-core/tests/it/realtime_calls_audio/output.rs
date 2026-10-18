//! Audio out (calls design §5): the frame clock, padding and `starved`,
//! the round trip, playout reports, heard on a barge-in. Every call here is
//! bound to a Chat thread: `starved` and the playout reports are a bound
//! call's (§6.4), and so is heard less the allowance (§5.4).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lmgw_api_types::realtime::{PlayoutSource, WebRtcCallState, WebRtcGroup};
use serde_json::{json, Value};
use str0m::Rtc;
use tokio::sync::Notify;

use super::{opus, stream, until, until_type, Heard};
use crate::common::captured_log::{capture_log_at, CapturedLog};
use crate::common::patience::within;
use crate::realtime_calls::{calls_world, post_sdp, Phone};
use crate::realtime_calls_net::client::{self, Client};
use crate::realtime_chat_thread::{manual_update, World};
use crate::support::realtime_audio::{fixture, silence};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_tts::{speech, wav, Tts};

/// A call bound to a new thread on `chatty`, from a phone whose own
/// receiver reports go every 200 ms (a quick round trip), connected and
/// past its `session.created`.
async fn bound(w: &World) -> (Phone, Heard) {
    let tid = w.thread("chatty", json!({})).await;
    let builder = Rtc::builder().set_rtcp_report_interval_audio(Duration::from_millis(200));
    let (c, sdp) = Client::with("127.0.0.1".parse().unwrap(), builder).await;
    let a = post_sdp(w, &w.gw.client(), &format!("chat_thread={tid}"), &sdp).await;
    let mut p = Phone::answered(c, a);
    p.c.connect().await;
    let mut heard = Heard::default();
    until_type(&mut p, &mut heard, "session.created").await;
    (p, heard)
}

/// Apply `update` (a `session.update`), past its `session.updated`.
async fn update(p: &mut Phone, heard: &mut Heard, update: Value) {
    p.send(update);
    let ev = until(p, heard, "session.updated", |v| {
        v["type"] == "session.updated" || v["type"] == "error"
    })
    .await;
    assert_eq!(ev.last().unwrap()["type"], "session.updated", "{ev:?}");
}

/// Drive the phone in 20 ms steps, noting what it hears, until `done`
/// holds.
async fn run_until(p: &mut Phone, heard: &mut Heard, what: &str, done: impl Fn(&Heard) -> bool) {
    within(what, async {
        while !done(heard) {
            p.c.run_for(Duration::from_millis(20)).await;
            p.c.met(|e| {
                heard.note(e);
                false
            });
        }
    })
    .await;
}

/// A push-to-talk turn: 200 ms of silence, the commit, `response.create`.
async fn say(p: &mut Phone, heard: &mut Heard) {
    stream(p, heard, &opus(&silence(200)), |_| false).await;
    p.send(json!({"type": "input_audio_buffer.commit"}));
    p.send(json!({"type": "response.create"}));
}

/// The transcript deltas among `events`.
fn deltas(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e["type"] == "response.output_audio_transcript.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect()
}

/// `t` in NTP milliseconds, as `playout_ntp_ms` carries it.
fn ntp_ms(t: SystemTime) -> f64 {
    2_208_988_800_000.0 + t.duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0
}

/// The number right after the last `after` in `text`.
fn number_after(text: &str, after: &str) -> Option<u64> {
    let rest = &text[text.rfind(after)? + after.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[tokio::test]
async fn a_reply_leaves_every_20_ms_and_pads_while_its_synthesis_is_late() {
    let w = calls_world(|_| {}).await;
    let held = Arc::new(Notify::new());
    w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    w.tts
        .push(Tts::Held(held.clone(), wav(&speech(300), 24_000)));
    w.chat.push(Turn::text(&["First.\n", "Second."]));
    // Before the POST: before the call's frame clock started.
    let before_call = client::now();
    let (mut p, mut heard) = bound(&w).await;
    update(&mut p, &mut heard, manual_update(0)).await;
    say(&mut p, &mut heard).await;
    // The first clause plays out, and the second is still with the TTS: the
    // clock pads, and says so once.
    let before = until(&mut p, &mut heard, "starved", |v| {
        v["type"] == "lmgw.output_audio.starved"
    })
    .await;
    let created = before
        .iter()
        .find(|e| e["type"] == "response.created")
        .expect("response.created");
    let rid = created["response"]["id"].clone();
    let starved = before.last().unwrap();
    assert_eq!(
        (&starved["response_id"], &starved["starved"]),
        (&rid, &json!(true))
    );
    assert_eq!(deltas(&before), ["First."]);
    // Its audio comes again: `false`, with the clause it carries.
    held.notify_one();
    let again = until(&mut p, &mut heard, "starved again", |v| {
        v["type"] == "lmgw.output_audio.starved"
    })
    .await;
    assert_eq!(again.last().unwrap()["starved"], false, "{again:?}");
    assert_eq!(deltas(&again), [" Second."]);
    let rest = until_type(&mut p, &mut heard, "response.done").await;
    assert_eq!(rest["response"]["status"], "completed", "{rest}");
    // Every frame lmgw sent, through the silence before the answer, the
    // padding and the wait for drained: on the RTP clock's 20 ms slots, in
    // order (a slot a late tick skipped leaves a gap of whole slots, §5.5).
    let frames = &heard.frames;
    assert!(frames.len() > 50, "{} frames", frames.len());
    for pair in frames.windows(2) {
        let step = pair[1].rtp - pair[0].rtp;
        assert!(step > 0 && step % 960 == 0, "{pair:?}");
    }
    // Paced, never ahead of the clock: a frame leaves no earlier than its
    // slot is due, the first slot at the clock's start, after the POST. So
    // the frame `k` slots after the first one received arrived no earlier
    // than `k` × 20 ms after the POST, however the scheduler ran.
    for f in frames {
        let k = (f.rtp - frames[0].rtp) / 960;
        assert!(
            f.arrived >= before_call + Duration::from_millis(20 * k),
            "slot {k} arrived {:?} after the POST",
            f.arrived.duration_since(before_call)
        );
    }
}

#[tokio::test]
async fn the_round_trip_comes_from_receiver_reports_and_a_playout_report_moves_the_delay() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = calls_world(|_| {}).await;
    // A 3.6 s answer: long enough to report on while it plays.
    w.tts.push(Tts::Wav(wav(
        &fixture("en_two_sentences_pause.wav"),
        24_000,
    )));
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let (mut p, mut heard) = bound(&w).await;
    update(&mut p, &mut heard, manual_update(0)).await;
    // The round trip, from the phone's receiver reports: the allowance's
    // DEBUG line names it once it is measured.
    let measured = |log: &CapturedLog| {
        let text = log.text();
        text.split("; round trip ")
            .skip(1)
            .any(|t| t.starts_with(|c: char| c.is_ascii_digit()))
    };
    run_until(&mut p, &mut heard, "a round trip measured", |_| {
        measured(&log)
    })
    .await;
    // A sender report of lmgw's about frames it wrote (str0m's first, made
    // before any frame, names none): the line positions are made on.
    run_until(&mut p, &mut heard, "lmgw's sender report", |h| {
        h.reports.iter().any(|r| r.sender_packet_count > 0)
    })
    .await;
    say(&mut p, &mut heard).await;
    until_type(&mut p, &mut heard, "response.output_audio_transcript.delta").await;
    let n = heard.frames.len();
    run_until(&mut p, &mut heard, "600 ms of the answer", |h| {
        h.frames.len() >= n + 30
    })
    .await;
    // The phone plays the frame it received 20 frames (400 ms) ago: its
    // position, made through lmgw's latest sender report as libwebrtc makes
    // `estimatedPlayoutTimestamp`.
    let sr = *heard
        .reports
        .iter()
        .rev()
        .find(|r| r.sender_packet_count > 0)
        .unwrap();
    let played = heard.frames[heard.frames.len() - 21];
    let ticks = (played.rtp as u32).wrapping_sub(sr.rtp_time.numer() as u32) as i32;
    let position = ntp_ms(sr.ntp_time) + f64::from(ticks) / 48.0;
    // How long the phone held that frame before it reported it.
    let sent = client::now();
    let held = sent.duration_since(played.arrived).as_millis() as u64;
    p.send(
        json!({"type": "lmgw.output_audio.playout", "playout_ntp_ms": position,
                  "jitter_buffer_ms": 60.0}),
    );
    let reported = "from the phone's playout report (delay ";
    run_until(&mut p, &mut heard, "the allowance from the report", |_| {
        log.text().contains(reported)
    })
    .await;
    // The delay measured: from that frame's send to the report's arrival,
    // less half the round trip. The frame left before the phone received
    // it, and the report arrived after the phone sent it, so the first part
    // is at least as long as the phone held the frame (a millisecond for
    // rounding), whatever the scheduling.
    let text = log.text();
    let line = &text[text.rfind(reported).unwrap()..];
    let line = line.lines().next().unwrap();
    let number = |after| number_after(line, after).unwrap_or_else(|| panic!("{line}"));
    let delay = number("(delay ");
    let since = number("its sample left ");
    let rtt = number("the round trip then, ");
    assert!(since + 1 >= held, "held {held} ms: {line}");
    assert!(
        delay.abs_diff(since.saturating_sub(rtt / 2)) <= 1,
        "the delay is its send to the report less half the round trip: {line}"
    );
    assert!(
        log.text().contains("the phone's jitter buffer 60 ms"),
        "{}",
        log.text()
    );
    // The status block lists the call with the same playout (calls design
    // §9): the frame clock hands it over on its next tick.
    let id = p.id.clone();
    let listed = |w: &World| {
        w.state
            .calls
            .status()
            .calls
            .into_iter()
            .find(|c| c.id == id)
    };
    run_until(&mut p, &mut heard, "the status block's report", |_| {
        listed(&w).is_some_and(|c| c.allowance_source == Some(PlayoutSource::Report))
    })
    .await;
    let c = listed(&w).unwrap();
    assert_eq!(c.state, WebRtcCallState::Running, "{c:?}");
    assert_eq!(c.group, WebRtcGroup::Plain, "{c:?}");
    assert!(c.thread.is_some() && c.session.is_some(), "{c:?}");
    assert!(
        c.key.as_deref().is_some_and(|k| k.starts_with("owner")),
        "{c:?}"
    );
    assert!(c.remote.is_some() && c.connected_at.is_some(), "{c:?}");
    // The session's round trip, from the test client's receiver reports,
    // with the latest one's age.
    assert!(c.round_trip_ms.is_some(), "{c:?}");
    assert!(c.receiver_report_age_ms.is_some(), "{c:?}");
    assert!(c.client_jitter_ms.is_some(), "{c:?}");
    assert_eq!(c.jitter_buffer_ms, Some(60.0), "{c:?}");
    assert!(c.playout_report_age_ms.is_some(), "{c:?}");
    // Whole milliseconds both ways: one apart at most.
    assert!(
        c.allowance_ms
            .is_some_and(|a| a.abs_diff(delay.saturating_sub(rtt / 2)) <= 1),
        "the allowance is the delay less half the round trip: {c:?}, {line}"
    );
    let done = until_type(&mut p, &mut heard, "response.done").await;
    assert_eq!(done["response"]["status"], "completed", "{done}");
}

#[tokio::test]
async fn heard_on_a_bound_calls_barge_in_is_what_left_by_the_allowance_before_the_cut() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = calls_world(|_| {}).await;
    w.tts.push(Tts::Wav(wav(
        &fixture("en_two_sentences_pause.wav"),
        24_000,
    )));
    w.chat.push(Turn::text(&[
        "Trains leave from the main station every hour.",
    ]));
    let (mut p, mut heard) = bound(&w).await;
    // Server VAD that interrupts, the barge-in suites' knobs, audio out.
    update(
        &mut p,
        &mut heard,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                                                   "interrupt_response": true}}},
            "lmgw": {"barge_in_guard_ms": 300, "post_interrupt_silence_ms": 600,
                     "barge_in_check": "duration"}}}),
    )
    .await;
    let question = [fixture("en_complete_short.wav"), silence(600)].concat();
    stream(&mut p, &mut heard, &opus(&question), |_| false).await;
    until_type(&mut p, &mut heard, "response.output_audio_transcript.delta").await;
    let n = heard.frames.len();
    run_until(&mut p, &mut heard, "a second of the answer", |h| {
        h.frames.len() >= n + 50
    })
    .await;
    // The user speaks over the answer.
    stream(&mut p, &mut heard, &opus(&question), |_| false).await;
    let cut = until(&mut p, &mut heard, "the cancelled response.done", |v| {
        v["type"] == "response.done"
    })
    .await;
    assert!(
        cut.iter()
            .any(|e| e["type"] == "input_audio_buffer.speech_started"),
        "{cut:?}"
    );
    let done = cut.last().unwrap();
    assert_eq!(done["response"]["status"], "cancelled", "{done}");
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "turn_detected"
    );
    // What the phone heard: what left by the playout allowance before the
    // cut — it mutes when `cleared` reaches it (§5.4); the arithmetic is
    // `writer::paced::pulled`'s unit tests', on the paused clock.
    let text = log.text();
    let line = text
        .lines()
        .find(|l| l.contains("cut; heard is what left by the playout allowance's "))
        .unwrap_or_else(|| panic!("no heard line in {text}"));
    // "… allowance's {less} samples before the cut: {heard} of {sent}
    // samples sent (…)".
    let number = |after| number_after(line, after).unwrap_or_else(|| panic!("{line}"));
    let less = number("allowance's ");
    let heard_samples = number("before the cut: ");
    let sent = number(" of ");
    assert!(less >= 480, "at least one frame: {line}");
    assert!(sent > 24_000, "a second of it was sent: {line}");
    assert!(heard_samples <= sent, "never more than was sent: {line}");
    // Whatever the scheduling: the clock sends no frame more than the
    // allowance late (older slots are skipped) or, the last of a late tick,
    // a frame late, so what left by the cut runs at most that far ahead of
    // it, and heard is at most twice the allowance and two frames short of
    // what was sent (a third frame for the allowance moving meanwhile).
    assert!(
        heard_samples + 2 * less + 3 * 480 >= sent,
        "heard too little: {line}"
    );
}
