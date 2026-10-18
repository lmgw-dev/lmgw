//! `lmgw.response.stop {playout_ntp_ms?}`, a bound call's stop (calls design
//! §6.2, §5.4): while an answer plays it cancels it — the cancelled
//! `response.done` (`client_cancelled`), then `cleared` — and heard is where
//! the phone muted, the sample its playout position names; sent again, or
//! with nothing open, it sends nothing and no error; while the user's turn
//! waits for its transcript the response owed to it is dropped, and nothing
//! speaks after the tap. On an unbound call every `lmgw.*` client event is
//! refused (§6.4).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opusic_c::{Channels, Decoder, SampleRate};
use serde_json::json;
use str0m::Rtc;
use tokio::sync::Notify;

use super::{
    bound, bound_with, loud, manual, openai_types, say, tone, types, unbound, update, Call,
};
use crate::common::captured_log::capture_log_at;
use crate::common::patience::within;
use crate::realtime_calls::calls_world;
use crate::realtime_calls_audio::opus;
use crate::realtime_calls_net::client::Client;
use crate::support::realtime_audio::{fixture, silence, Asr};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_tts::{wav, Tts};

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

/// Run the phone in 20 ms steps until `done` holds.
async fn run_until(call: &mut Call, what: &str, done: impl Fn(&Call) -> bool) {
    within(what, async {
        while !done(call) {
            call.run_for(Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// The index in `call`'s frames of the first that carries the tone, once
/// one arrived, decoding with `dec` from `from` on.
fn first_loud(call: &Call, dec: &mut Decoder, from: &mut usize) -> Option<usize> {
    while *from < call.rec.frames.len() {
        let at = *from;
        *from += 1;
        if loud(dec, &call.rec.frames[at].payload) {
            return Some(at);
        }
    }
    None
}

/// The phone mutes at a position inside the answer — 10 ms into the eleventh
/// frame of it — and says so with the stop: lmgw cuts the heard table at
/// the sample that names, ten frames and a half of the item (each frame lmgw
/// sent of it carried 480 samples), exact whatever the scheduling, and sends
/// the cancelled `response.done`, then `cleared`.
#[tokio::test]
async fn a_stop_while_an_answer_plays_cuts_it_where_the_phone_muted() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = calls_world(|_| {}).await;
    w.tts.push(Tts::Wav(wav(&tone(3_000), 24_000)));
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let mut call = bound(&w).await;
    update(&mut call, manual()).await;
    // A sender report of lmgw's about frames it wrote (str0m's first, made
    // before any frame, names none): the line positions are made on, the
    // phone's and lmgw's alike.
    run_until(&mut call, "lmgw's sender report", |c| {
        c.rec.reports.iter().any(|r| r.sender_packet_count > 0)
    })
    .await;
    let mut dec = Decoder::new(Channels::Mono, SampleRate::Hz24000).unwrap();
    let mut decoded = call.rec.frames.len();
    say(&mut call).await;
    let started = call.next_of("output_audio_buffer.started").await;
    let rid = started["response_id"].clone();
    let mut first = None;
    within("the answer's first frame", async {
        while first.is_none() {
            first = first_loud(&call, &mut dec, &mut decoded);
            if first.is_none() {
                call.run_for(Duration::from_millis(20)).await;
            }
        }
    })
    .await;
    let first = first.unwrap();
    run_until(&mut call, "half a second of the answer", |c| {
        c.rec.frames.len() >= first + 25
    })
    .await;
    // Every frame from the answer's first to the one muted at carries the
    // tone: lmgw sent them whole, each 480 samples of the item (decoded in
    // order, after the first).
    let muted = first + 10;
    for f in &call.rec.frames[decoded..=muted] {
        assert!(
            loud(&mut dec, &f.payload),
            "a frame of the answer is silent"
        );
    }
    let sr = *call
        .rec
        .reports
        .iter()
        .rev()
        .find(|r| r.sender_packet_count > 0)
        .unwrap();
    let ticks = (call.rec.frames[muted].rtp as u32).wrapping_sub(sr.rtp_time.numer() as u32) as i32;
    // 10 ms into the frame: 480 ticks of the 48 kHz clock.
    let position = ntp_ms(sr.ntp_time) + f64::from(ticks + 480) / 48.0;
    call.send(json!({"type": "lmgw.response.stop", "playout_ntp_ms": position}));
    let done = call.next_of("response.done").await;
    assert_eq!(done["response"]["id"], rid);
    assert_eq!(done["response"]["status"], "cancelled", "{done}");
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "client_cancelled"
    );
    let after = call.probe().await;
    assert_eq!(types(&after)[0], "output_audio_buffer.cleared", "{after:?}");
    assert_eq!(after[0]["response_id"], rid);
    // Heard: ten frames and 240 samples of the item.
    let text = log.text();
    let said = format!("cut; heard is the stop's playout position {position:.0}: ");
    let line = text
        .lines()
        .find(|l| l.contains(&said))
        .unwrap_or_else(|| panic!("no heard line for {position} in {text}"));
    let heard = number_after(line, &said).unwrap();
    let sent = number_after(line, " of ").unwrap();
    assert_eq!(heard, 10 * 480 + 240, "{line}");
    assert!(sent >= 25 * 480, "{line}");
}

/// A stop with nothing open sends nothing, and no error; one while an
/// answer plays cancels it (heard is what was sent, no position); the same
/// stop again sends nothing.
#[tokio::test]
async fn a_stop_with_nothing_open_or_sent_twice_sends_nothing() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = calls_world(|_| {}).await;
    w.tts.push(Tts::Wav(wav(&tone(3_000), 24_000)));
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let mut call = bound(&w).await;
    update(&mut call, manual()).await;
    call.send(json!({"type": "lmgw.response.stop"}));
    assert_eq!(openai_types(&call.probe().await), ["session.updated"]);
    say(&mut call).await;
    let started = call.next_of("output_audio_buffer.started").await;
    call.run_for(Duration::from_millis(200)).await;
    call.send(json!({"type": "lmgw.response.stop"}));
    call.send(json!({"type": "lmgw.response.stop"}));
    let done = call.next_of("response.done").await;
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "client_cancelled"
    );
    // The second stop sent nothing: one `cleared`, no other done, no error.
    let after = call.probe().await;
    assert_eq!(
        openai_types(&after),
        ["output_audio_buffer.cleared", "session.updated"],
        "{after:?}"
    );
    assert_eq!(after[0]["response_id"], started["response_id"]);
    let text = log.text();
    assert!(text.contains("cut; heard is what was sent"), "{text}");
    assert!(
        text.contains("lmgw.response.stop with nothing open"),
        "{text}"
    );
}

/// The user's turn commits by server VAD and is owed a response, which waits
/// for its transcript; the stop drops it: once the transcript is in, no
/// response is created and nothing speaks.
#[tokio::test]
async fn a_stop_while_the_turn_waits_for_its_transcript_drops_the_owed_response() {
    let w = calls_world(|_| {}).await;
    let held = Arc::new(Notify::new());
    w.asr
        .push(Asr::HeldText(held.clone(), "when does the train leave"));
    w.chat.push(Turn::text(&["Every hour."]));
    let builder = Rtc::builder();
    let (c, sdp) = Client::with("127.0.0.1".parse().unwrap(), builder).await;
    let mut call = bound_with(&w, c, &sdp).await;
    update(
        &mut call,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                                                   "create_response": true}}}}}),
    )
    .await;
    call.speak(&opus(
        &[fixture("en_complete_short.wav"), silence(700)].concat(),
    ))
    .await;
    call.next_of("input_audio_buffer.committed").await;
    call.send(json!({"type": "lmgw.response.stop"}));
    // The stop is applied before the probe: what it dropped never starts.
    call.probe().await;
    held.notify_one();
    call.next_of("conversation.item.input_audio_transcription.completed")
        .await;
    call.run_for(Duration::from_millis(500)).await;
    let after = call.probe().await;
    assert!(
        !openai_types(&after)
            .iter()
            .any(|t| t.starts_with("response.") || t.starts_with("output_audio_buffer.")),
        "nothing speaks after the tap: {after:?}"
    );
    assert_eq!(w.chat.seen.chat_count(), 0, "no model call");
}

/// On an unbound call — a stock client's — each of a bound call's own client
/// events is answered `invalid_event`, naming what it applies to, and the
/// session goes on.
#[tokio::test]
async fn the_lmgw_client_events_are_refused_on_an_unbound_call() {
    let w = calls_world(|_| {}).await;
    let mut call = unbound(&w).await;
    for (i, event) in [
        json!({"type": "lmgw.response.stop", "playout_ntp_ms": 1.0}),
        json!({"type": "lmgw.output_audio.playout", "jitter_buffer_ms": 40.0}),
        json!({"type": "lmgw.session.take_over"}),
    ]
    .into_iter()
    .enumerate()
    {
        let kind = event["type"].as_str().unwrap().to_string();
        let mut event = event;
        event["event_id"] = json!(format!("evt_{i}"));
        call.send(event);
        let e = call.next_of("error").await;
        assert_eq!(e["error"]["code"], "invalid_event", "{e}");
        assert_eq!(e["error"]["param"], "type");
        assert_eq!(e["error"]["event_id"], format!("evt_{i}"));
        let message = e["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with(&kind) && message.contains("a call bound to a Chat thread"),
            "{message}"
        );
    }
    assert_eq!(types(&call.probe().await), ["session.updated"]);
}
