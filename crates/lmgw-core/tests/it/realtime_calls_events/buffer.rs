//! A call's data channel carries no audio, and its output buffer is lmgw's
//! (calls design §6.1, §6.2): `started` with an answer's first frame,
//! `stopped` right after the `response.done` of one that played out, and a
//! barge-in's verdict `cleared` right after its cancelled `response.done`.

use serde_json::json;

use super::{bound, manual, say, types, unbound, update};
use crate::realtime_calls::calls_world;
use crate::realtime_calls_audio::opus;
use crate::support::realtime_audio::{fixture, silence};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_tts::{speech, wav, Tts};

/// A stock call (unbound) speaks an answer: its buffer starts with the first
/// frame, after the answer's `response.content_part.added`, and stops right
/// after its `response.done`, once. No audio travels on the data channel:
/// no `response.output_audio.delta`, `input_audio_buffer.append` refused as
/// the call's audio is on RTP, and `session.lmgw.output_lead_ms` echoes 0,
/// whatever an update asks: the frame clock paces, not a lead.
#[tokio::test]
async fn a_stock_calls_answer_starts_its_buffer_with_its_first_frame_and_stops_it_after_its_done() {
    let w = calls_world(|s| s.realtime.tts_alias = "speak".into()).await;
    w.tts.push(Tts::Wav(wav(&speech(600), 24_000)));
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let mut call = unbound(&w).await;
    let created: serde_json::Value = serde_json::from_str(&call.rec.messages[0]).unwrap();
    assert_eq!(created["type"], "session.created");
    assert_eq!(created["session"]["lmgw"]["output_lead_ms"], 0);
    let mut asked = manual();
    asked["session"]["lmgw"] = json!({"output_lead_ms": 300});
    let updated = update(&mut call, asked).await;
    assert_eq!(
        updated["session"]["lmgw"]["output_lead_ms"], 0,
        "no lead on a call"
    );

    call.send(json!({"type": "input_audio_buffer.append", "audio": "AAAA",
                     "event_id": "evt_append"}));
    let e = call.next_of("error").await;
    assert_eq!(e["error"]["code"], "invalid_event", "{e}");
    assert_eq!(e["error"]["param"], "type");
    assert_eq!(e["error"]["event_id"], "evt_append");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("arrives on its RTP track"),
        "{e}"
    );

    say(&mut call).await;
    let events = call.until_type("response.done").await;
    let done = events.last().unwrap();
    assert_eq!(done["response"]["status"], "completed", "{done}");
    let rid = done["response"]["id"].clone();
    let t = types(&events);
    let at = |name: &str| {
        t.iter()
            .position(|x| *x == name)
            .unwrap_or_else(|| panic!("no {name} in {t:?}"))
    };
    assert!(at("response.content_part.added") < at("output_audio_buffer.started"));
    let started = &events[at("output_audio_buffer.started")];
    assert_eq!(started["response_id"], rid);
    assert!(started["event_id"].as_str().unwrap().starts_with("event_"));
    assert_eq!(
        t.iter()
            .filter(|x| x.starts_with("output_audio_buffer."))
            .count(),
        1
    );
    // Right after the done: `stopped`, the buffer played out.
    let after = call.probe().await;
    assert_eq!(
        types(&after),
        ["output_audio_buffer.stopped", "session.updated"],
        "{after:?}"
    );
    assert_eq!(after[0]["response_id"], rid);
    assert!(
        !call
            .rec
            .messages
            .iter()
            .any(|m| m.contains("response.output_audio.delta")),
        "no audio on the data channel"
    );
    assert!(!call.rec.frames.is_empty(), "the audio went on RTP");
}

/// A bound call's answer is cut by the user's speech: the barge-in's
/// cancelled `response.done` (`turn_detected`) is followed right away by
/// `output_audio_buffer.cleared`, the verdict the phone mutes on, after the
/// `started` of the same answer.
#[tokio::test]
async fn a_barge_ins_verdict_is_cleared_right_after_its_cancelled_done() {
    let w = calls_world(|_| {}).await;
    // Far longer than the test's pacing needs (the user speaks over it a
    // second in), so no lag of the machine ends the answer first.
    let long = fixture("en_two_sentences_pause.wav").repeat(6);
    w.tts.push(Tts::Wav(wav(&long, 24_000)));
    w.chat.push(Turn::text(&[
        "Trains leave from the main station every hour.",
    ]));
    let mut call = bound(&w).await;
    update(
        &mut call,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                                                   "interrupt_response": true}}},
            "lmgw": {"barge_in_guard_ms": 300, "post_interrupt_silence_ms": 600,
                     "barge_in_check": "duration"}}}),
    )
    .await;
    let question = opus(&[fixture("en_complete_short.wav"), silence(600)].concat());
    call.speak(&question).await;
    let started = call.next_of("output_audio_buffer.started").await;
    // A second of the answer, then the user speaks over it.
    let n = call.rec.frames.len();
    while call.rec.frames.len() < n + 50 {
        call.run_for(std::time::Duration::from_millis(20)).await;
    }
    call.speak(&question).await;
    let cut = call.until_type("response.done").await;
    assert!(
        types(&cut).contains(&"input_audio_buffer.speech_started"),
        "{cut:?}"
    );
    let done = cut.last().unwrap();
    assert_eq!(done["response"]["status"], "cancelled", "{done}");
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "turn_detected"
    );
    assert_eq!(done["response"]["id"], started["response_id"]);
    assert!(!types(&cut).contains(&"output_audio_buffer.cleared"));
    let next = call
        .until("the event after the done", |v| {
            v["type"] != "lmgw.output_audio.starved"
        })
        .await;
    let cleared = next.last().unwrap();
    assert_eq!(cleared["type"], "output_audio_buffer.cleared", "{next:?}");
    assert_eq!(cleared["response_id"], started["response_id"]);
}
