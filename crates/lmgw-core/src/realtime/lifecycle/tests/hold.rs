//! The hold (voice-audio-input design §3.2, `lifecycle::held`), driven
//! directly on a response the test marks held: what passes and what waits,
//! the replay in arrival order at the release, the quiet veto with the
//! usage known so far, the cut of a held response whatever
//! `interrupt_response` says, and the arrival and release marks the timing
//! counts.

use std::time::{Duration, Instant};

use super::super::held::Held;
use super::*;
use crate::realtime::protocol::ResponseStatus;
use crate::realtime::responder::Mark;

/// A created, launched response, held as one that heard an audio turn is.
async fn held() -> Rig {
    let mut r = Rig::new().await;
    r.core.response_create(Some("c1".into()), None);
    let active = r.core.active.as_mut().unwrap();
    active.held = Some(Held::default());
    active.timing.held_at = Some(Instant::now());
    r.events().await;
    r
}

#[tokio::test]
async fn marks_pass_and_output_waits_for_the_release_then_replays_in_order() {
    let mut r = held().await;
    r.core.on_responder(1, Msg::Mark(Mark::FirstToken));
    r.delta(1, "Es ");
    r.delta(1, "ist ");
    assert_eq!(
        r.events().await,
        Vec::<Value>::new(),
        "nothing reaches the client"
    );
    let t = &r.core.active.as_ref().unwrap().timing;
    assert!(
        t.stages(Instant::now()).first_token_ms.is_some(),
        "a mark passes"
    );

    r.core.release();
    r.delta(1, "sonnig.");
    r.finish(1, Ok(completion("Es ist sonnig.")));
    let ev = r.events().await;
    let deltas: Vec<&str> = ev
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(
        deltas,
        ["Es ", "ist ", "sonnig."],
        "replayed in order, then live"
    );
    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done"]);
    assert_eq!(ev[0]["response"]["status"], "completed");
}

#[tokio::test]
async fn the_end_of_a_held_response_waits_too() {
    let mut r = held().await;
    r.delta(1, "Ja.");
    r.finish(1, Ok(completion("Ja.")));
    assert_eq!(r.events().await, Vec::<Value>::new());
    assert!(
        r.acks.is_empty(),
        "not finished: the drained marker is not queued"
    );
    r.core.release();
    r.events().await;
    assert_eq!(r.acks, [1]);
}

#[tokio::test]
async fn a_veto_drops_the_queue_and_ends_quietly_as_a_cancel() {
    let mut r = held().await;
    // The usage passes the hold: the veto's `response.done` carries it
    // (WP3 review #5).
    r.core.on_responder(
        1,
        Msg::Delta(StreamDelta::Usage(Usage {
            prompt_tokens: Some(221),
            completion_tokens: Some(3),
            ..Default::default()
        })),
    );
    r.delta(1, "Hallo!");
    r.finish(1, Ok(completion("Hallo!")));
    r.core.veto(super::super::held::NO_WORDS);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done"], "no error, no output");
    let done = &ev[0]["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "no_words");
    assert_eq!(done["usage"]["input_tokens"], 221, "{done}");
    assert!(r.core.active.is_none());
    // What it still says is a cancelled response's: dropped.
    r.delta(1, "noch");
    assert_eq!(r.events().await, Vec::<Value>::new());
}

#[tokio::test]
async fn the_wait_counts_once_from_the_first_held_output_to_the_release() {
    let mut r = held().await;
    r.core.on_responder(1, Msg::Mark(Mark::FirstToken));
    r.delta(1, "Ja.");
    tokio::time::sleep(Duration::from_millis(30)).await;
    r.core.release();
    let t = &r.core.active.as_ref().unwrap().timing;
    let st = t.stages(Instant::now());
    let wait = st.transcript_wait_ms.unwrap();
    assert!((30..1000).contains(&wait), "{wait}");
    // The first token is the arrival's, not inflated by the hold; what
    // reached the client is the release's.
    assert!(st.first_token_ms.unwrap() < 30, "{st:?}");
    let (what, at) = st.first.unwrap();
    assert_eq!(what, "text");
    // Reached the client at the release, at least the 30 ms wait after the
    // launch — not at its arrival, before it (WP3 review #10).
    assert!(at >= 30 && at > st.first_token_ms.unwrap(), "{at} {st:?}");
    assert!(t
        .line(ResponseStatus::Completed, Instant::now())
        .contains("input=audio held="));

    // Output that came after the release waited for nothing.
    let mut r = held().await;
    r.core.release();
    r.delta(1, "Ja.");
    let st = r
        .core
        .active
        .as_ref()
        .unwrap()
        .timing
        .stages(Instant::now());
    assert_eq!(st.transcript_wait_ms, Some(0));

    // Never released: nothing reached the client, and no wait is said.
    let r = held().await;
    let st = r
        .core
        .active
        .as_ref()
        .unwrap()
        .timing
        .stages(Instant::now());
    assert_eq!((st.first, st.transcript_wait_ms), (None, None));
}

/// WP3 review #6: with `interrupt_response: false` a turn runs beside an
/// answer the client hears — but a held response nobody heard would answer
/// half a sentence: it is cut all the same.
#[tokio::test]
async fn a_held_response_is_cut_by_a_new_turn_whatever_interrupt_response_says() {
    use crate::realtime::audio_in::Detected;
    use crate::realtime::protocol::TurnDetection;
    use crate::realtime::turn::server_vad::TurnEvent;
    let mut r = held().await;
    let td = r
        .core
        .session
        .audio
        .as_mut()
        .and_then(|a| a.input.as_mut())
        .and_then(|i| i.turn_detection.as_mut());
    let Some(TurnDetection::ServerVad {
        interrupt_response, ..
    }) = td
    else {
        panic!("the session starts on server_vad");
    };
    *interrupt_response = Some(false);
    assert_eq!(r.core.listen().cuts, Some(1), "the barge-in gate knows");
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 4000,
            onset_ms: 4300,
        },
        speech_end: None,
        at: tokio::time::Instant::now(),
        barge_in: false,
        end: None,
    });
    assert!(r.core.active.is_none(), "cut");
    let ev = r.events().await;
    let done = ev.iter().find(|e| e["type"] == "response.done").unwrap();
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "turn_detected"
    );
}
