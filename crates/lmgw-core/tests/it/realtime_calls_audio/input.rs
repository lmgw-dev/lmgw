//! Audio in (calls design §4): a VAD turn, loss, and the audio from
//! before the open.

use serde_json::{json, Value};
use str0m::Rtc;

use super::{opus, stream, until_type, Heard, FRAME};
use crate::common::captured_log::capture_log_at;
use crate::common::patience;
use crate::realtime_calls::{calls_world, multipart, post_raw, Phone};
use crate::realtime_calls_net::client::Client;
use crate::realtime_chat_thread::World;
use crate::support::realtime_audio::{fixture, silence, ASR_ALIAS};
use crate::support::realtime_fakes::{events_until, next_event, open, send};

/// A world whose unbound sessions transcribe with the ASR fake.
async fn hearing() -> World {
    calls_world(|s| s.realtime.asr_alias = ASR_ALIAS.into()).await
}

/// Text out, and `turn_detection` as given: what both transports start
/// with here.
fn session(turn_detection: Value) -> Value {
    json!({"type": "realtime", "output_modalities": ["text"],
           "audio": {"input": {"turn_detection": turn_detection}}})
}

/// Server VAD without a response: the turn's events alone.
fn vad() -> Value {
    json!({"type": "server_vad", "create_response": false})
}

/// A call on `?model=chatty` with `session` as the POST's session part,
/// from `c` (its offer `sdp`): answered, not yet connected.
async fn call(w: &World, c: Client, sdp: &str, session: &Value) -> Phone {
    let (ct, body) = multipart(&[
        ("sdp", "application/sdp", sdp),
        ("session", "application/json", &session.to_string()),
    ]);
    let a = post_raw(
        w,
        &w.gw.client(),
        "/v1/realtime/calls?model=chatty",
        &ct,
        body,
    )
    .await;
    Phone::answered(c, a)
}

/// A connected call with `session` applied, past its `session.updated`.
async fn connected(w: &World, session: &Value) -> (Phone, Heard) {
    let (c, sdp) = Phone::offer().await;
    let mut p = call(w, c, &sdp, session).await;
    p.c.connect().await;
    let mut heard = Heard::default();
    until_type(&mut p, &mut heard, "session.created").await;
    until_type(&mut p, &mut heard, "session.updated").await;
    (p, heard)
}

/// `audio_start_ms` and `audio_end_ms` of the first turn in `pcm`, as a
/// WebSocket session on the same gateway places it from the same samples.
async fn on_a_websocket(w: &World, pcm: &[i16]) -> (u64, u64) {
    let mut ws = open(&w.addr(), "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update", "session": session(vad())}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    crate::support::realtime_audio::stream(&mut ws, pcm).await;
    let events = events_until(&mut ws, "input_audio_buffer.speech_stopped").await;
    let started = events
        .iter()
        .find(|e| e["type"] == "input_audio_buffer.speech_started")
        .expect("speech_started");
    let stopped = events.last().unwrap();
    (
        started["audio_start_ms"].as_u64().unwrap(),
        stopped["audio_end_ms"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn speech_in_is_a_committed_turn_on_the_timeline_a_websocket_session_has() {
    let w = hearing().await;
    let pcm = [
        silence(1000),
        fixture("en_complete_short.wav"),
        silence(600),
    ]
    .concat();
    let (mut p, mut heard) = connected(&w, &session(vad())).await;
    stream(&mut p, &mut heard, &opus(&pcm), |_| false).await;
    let started = until_type(&mut p, &mut heard, "input_audio_buffer.speech_started").await;
    let stopped = until_type(&mut p, &mut heard, "input_audio_buffer.speech_stopped").await;
    let committed = until_type(&mut p, &mut heard, "input_audio_buffer.committed").await;
    assert_eq!(committed["item_id"], started["item_id"]);
    let start = started["audio_start_ms"].as_u64().unwrap();
    let end = stopped["audio_end_ms"].as_u64().unwrap();
    // The speech begins 1300 ms into the stream (the clip's 300 ms), less
    // the 300 ms pre-roll.
    let (ws_start, ws_end) = on_a_websocket(&w, &pcm).await;
    assert!(ws_start.abs_diff(1000) <= 100, "{ws_start}");
    // The same samples, decoded from Opus (a few ms of codec delay), on
    // the same timeline: the detector's 32 ms frames apart at most twice.
    assert!(start.abs_diff(ws_start) <= 64, "{start} vs {ws_start}");
    assert!(end.abs_diff(ws_end) <= 64, "{end} vs {ws_end}");
}

#[tokio::test]
async fn a_lost_packet_comes_back_from_fec_and_a_burst_is_concealed() {
    let (log, _guard) = capture_log_at(tracing::Level::DEBUG);
    let w = hearing().await;
    let (mut p, mut heard) = connected(&w, &session(Value::Null)).await;
    // One second of the clip's speech; the network loses frame 10, and 30
    // to 32 in a burst.
    let speech = &fixture("en_complete_short.wav")[300 * 24..][..50 * FRAME];
    stream(&mut p, &mut heard, &opus(speech), |k| {
        k == 10 || (30..33).contains(&k)
    })
    .await;
    p.send(json!({"type": "input_audio_buffer.commit"}));
    until_type(&mut p, &mut heard, "input_audio_buffer.committed").await;
    patience::until("the turn's ASR upload", || w.asr.seen.count() == 1).await;
    // The turn is as long as the RTP span, sample for sample: every lost
    // frame was filled.
    let (rate, channels, samples) = w.asr.seen.wav(0);
    assert_eq!(channels, 1);
    assert_eq!(samples as u64 * 1000, rate as u64 * 1000, "one second");
    let text = log.text();
    assert!(
        text.contains("20 ms of input missing: 0 ms concealed, 20 ms from the next packet's FEC"),
        "{text}"
    );
    assert!(
        text.contains("60 ms of input missing: 40 ms concealed, 20 ms from the next packet's FEC"),
        "{text}"
    );
}

#[tokio::test]
async fn the_audio_from_before_the_open_is_heard_first() {
    let w = hearing().await;
    // A phone whose channel is not oai-events yet: connected, nothing open
    // for the call. Its send buffer takes a message as large as lmgw's.
    let builder = Rtc::builder().set_sctp_max_buffered_amount(512 << 10);
    let ip = "127.0.0.1".parse().unwrap();
    let (c, sdp) = Client::labelled_with(ip, builder, "not-yet").await;
    let mut p = call(&w, c, &sdp, &session(vad())).await;
    p.c.connect().await;
    let mut heard = Heard::default();
    // The whole question before the open: only the audio kept from the
    // first frame can make a turn of it.
    let clip = fixture("en_complete_short.wav");
    stream(&mut p, &mut heard, &opus(&clip), |_| false).await;
    p.c.open_events().await;
    until_type(&mut p, &mut heard, "session.created").await;
    until_type(&mut p, &mut heard, "session.updated").await;
    let started = until_type(&mut p, &mut heard, "input_audio_buffer.speech_started").await;
    let stopped = until_type(&mut p, &mut heard, "input_audio_buffer.speech_stopped").await;
    until_type(&mut p, &mut heard, "input_audio_buffer.committed").await;
    // The timeline begins at the first frame: the word is whole in the
    // first turn, its speech from the clip's 300 ms to 1738 ms.
    let start = started["audio_start_ms"].as_u64().unwrap();
    let end = stopped["audio_end_ms"].as_u64().unwrap();
    assert!(start <= 300, "audio_start_ms {start}");
    assert!(end >= 1738, "audio_end_ms {end}");
}
