//! Barge-in against the response lifecycle (realtime design §4.3, §6.4):
//! a turn's start, judged by the core, during each phase of a response —
//! the test plays the responder and the detector.

use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::realtime::audio_in::Detected;
use crate::realtime::lifecycle::Interruption;
use crate::realtime::protocol::TurnDetection;
use crate::realtime::turn::server_vad::TurnEvent;
use crate::realtime::voice::{SpeakVoice, VoiceOutcome, VoiceVia};

/// The session speaks, its audio all sent at once (a long lead), so the
/// writer's window is the clause's own length.
fn speaking(r: &mut Rig) {
    r.core.speech.tts.alias = Some("say".into());
    r.core.speech.voice = VoiceOutcome::Resolved(SpeakVoice {
        send: Some("alba".into()),
        name: "alba".into(),
        via: VoiceVia::Model,
        verified: true,
    });
    r.core.session.output_modalities = Some(vec![Modality::Audio]);
    r.core
        .session
        .lmgw
        .get_or_insert_with(Default::default)
        .output_lead_ms = Some(60_000);
}

/// A synthesized clause of `ms` of (silent) audio, handed to the writer.
async fn clause(r: &mut Rig, gen: u64, text: &str, ms: usize) {
    let pcm = bytes::Bytes::from(vec![0u8; ms * 48]);
    r.core.on_responder(
        gen,
        Msg::Clause {
            text: text.into(),
            written: crate::realtime::heard::Written::said("", text),
            pcm,
        },
    );
    r.core.flush().await;
    // The writer releases it: all of it, inside the lead.
    tokio::time::sleep(Duration::from_millis(30)).await;
}

/// The detector started a turn whose deciding frame was captured `at`.
fn speech(r: &mut Rig, at: Instant, gate: bool) {
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 4000,
            onset_ms: 4300,
        },
        speech_end: None,
        at,
        barge_in: gate,
        end: None,
    });
}

/// The open turn committed as user text `text`, its transcript in.
fn commit(r: &mut Rig, text: &str) {
    r.core.turn = None;
    let id = user_turn(r, text);
    r.core.pending_commit(&id, true, false);
    r.core.pending_resolve(&id, false);
}

fn stop(r: &mut Rig, gen: u64, reason: FinishReason) {
    r.core
        .on_responder(gen, Msg::Delta(StreamDelta::Stop(reason)));
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

#[tokio::test]
async fn a_barge_in_while_the_answer_plays_cancels_it_after_speech_started() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(None, None);
    clause(&mut r, 1, "Hello there.", 2000).await;
    let playing = Instant::now();
    r.finish(1, Ok(completion("Hello there.")));
    speech(&mut r, playing, true);
    let ev = r.events().await;
    let t = types(&ev);
    let at = t
        .iter()
        .position(|t| *t == "input_audio_buffer.speech_started")
        .unwrap();
    // speech_started first, then the cancelled item's audio part — so the
    // stock client still has an item to interrupt — and response.done.
    assert_eq!(
        t[at..],
        [
            "input_audio_buffer.speech_started",
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
        ],
        "{t:?}"
    );
    let done = &ev.last().unwrap()["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    // All of it had left: the item keeps it all.
    assert_eq!(done["output"][0]["status"], "incomplete");
    assert_eq!(
        done["output"][0]["content"][0]["transcript"],
        "Hello there."
    );
    assert!(r.core.active.is_none());
    assert!(r.core.turn.is_some(), "the user's turn is open");
    // The stale acknowledgement came at once, not at the window's end.
    assert_eq!(r.acks, [1]);
}

#[tokio::test]
async fn a_text_response_whose_items_are_closed_is_not_cut_and_finishes() {
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.delta(1, "Hal");
    stop(&mut r, 1, FinishReason::Stop);
    r.events().await;
    speech(&mut r, Instant::now(), false);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["input_audio_buffer.speech_started"]);
    assert!(r.core.active.is_some());
    r.finish(1, Ok(completion("Hal")));
    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(ev.last().unwrap()["response"]["status"], "completed");
}

#[tokio::test]
async fn an_owed_response_due_after_the_cut_one_waits_for_the_turn() {
    // WP1c review H5: the cancel ends the active response, after which an
    // owed response that was due would start — while the user talks.
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.delta(1, "Hal");
    let owed = user_turn(&mut r, "und weiter?");
    r.core.pending_commit(&owed, true, false);
    r.core.pending_resolve(&owed, false);
    r.events().await;
    speech(&mut r, Instant::now(), false);
    let ev = r.events().await;
    assert_eq!(
        ev.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    assert_eq!(count(&ev, "response.created"), 0, "{:?}", types(&ev));
    // The turn ends: one response, for both turns.
    commit(&mut r, "noch etwas");
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.created"]);
    let active = r.core.active.as_ref().unwrap();
    assert_eq!(active.answers.len(), 2);
}

#[tokio::test]
async fn a_queued_create_is_carried_past_the_barge_in_and_starts_after_the_turn() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(None, None);
    clause(&mut r, 1, "Let me look.", 2000).await;
    stop(&mut r, 1, FinishReason::Stop);
    // Closing: the client's follow-up is queued.
    r.core.response_create(Some("follow".into()), None);
    speech(&mut r, Instant::now(), true);
    let ev = r.events().await;
    assert_eq!(ev.last().unwrap()["response"]["status"], "cancelled");
    assert_eq!(count(&ev, "response.created"), 1, "only the first");
    // Another while the user still talks: refused, echoing its id.
    r.core.response_create(Some("again".into()), None);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["error"]);
    assert_eq!(
        ev[0]["error"]["code"],
        "conversation_already_has_active_response"
    );
    assert_eq!(ev[0]["error"]["event_id"], "again");
    // The turn ends: the carried create starts, once, answering it too.
    commit(&mut r, "Wie spät ist es?");
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.created"]);
    let active = r.core.active.as_ref().unwrap();
    assert_eq!(active.event_id.as_deref(), Some("follow"));
    assert!(r.core.pending.is_none());
}

#[tokio::test]
async fn a_create_while_the_user_speaks_waits_for_the_turn_even_if_it_is_noise() {
    let mut r = Rig::new().await;
    speech(&mut r, Instant::now(), false);
    r.core.response_create(Some("tool".into()), None);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["input_audio_buffer.speech_started"]);
    // The turn has no words: the client asked, so its response still runs.
    commit(&mut r, " ");
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.created"]);
    assert_eq!(
        r.core.active.as_ref().unwrap().event_id.as_deref(),
        Some("tool")
    );
}

#[tokio::test]
async fn a_cut_response_nobody_heard_is_owed_again_and_a_heard_one_is_not_resumed() {
    // Cut before anything reached the client: the question still gets its
    // answer though the interrupting turn was noise.
    let mut r = Rig::new().await;
    let question = user_turn(&mut r, "Wie spät ist es?");
    r.core.pending_commit(&question, true, false);
    r.core.pending_resolve(&question, false);
    assert!(r.core.active.is_some(), "the automatic response");
    speech(&mut r, Instant::now(), false);
    commit(&mut r, "");
    let ev = r.events().await;
    assert_eq!(count(&ev, "response.created"), 2, "{:?}", types(&ev));
    assert_eq!(
        r.core.active.as_ref().unwrap().answers.first(),
        Some(&question)
    );

    // Cut while it was being heard, by a turn with no words: not resumed
    // (owner's decision Q2).
    let mut r = Rig::new().await;
    let question = user_turn(&mut r, "Wie spät ist es?");
    r.core.pending_commit(&question, true, false);
    r.core.pending_resolve(&question, false);
    r.delta(1, "Es ist");
    speech(&mut r, Instant::now(), false);
    commit(&mut r, "");
    let ev = r.events().await;
    assert_eq!(count(&ev, "response.created"), 1);
    assert!(r.core.active.is_none() && r.core.pending.is_none());
}

#[tokio::test]
async fn a_barge_in_on_a_spoken_preamble_keeps_its_completed_call() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(None, None);
    clause(&mut r, 1, "Let me check.", 2000).await;
    r.core.on_responder(
        1,
        Msg::Delta(StreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "get_time".into(),
        }),
    );
    r.core.on_responder(
        1,
        Msg::Delta(StreamDelta::ToolCallArgsDelta {
            index: 0,
            fragment: "{}".into(),
        }),
    );
    stop(&mut r, 1, FinishReason::ToolUse);
    speech(&mut r, Instant::now(), true);
    let ev = r.events().await;
    let done = &ev.last().unwrap()["response"];
    assert_eq!(done["status_details"]["reason"], "turn_detected");
    assert_eq!(done["output"][0]["status"], "incomplete");
    assert_eq!(done["output"][1]["type"], "function_call");
    assert_eq!(done["output"][1]["status"], "completed", "never cancelled");
    // The call's item closed once, completed, at the end of generation.
    let call_done = ev
        .iter()
        .filter(|e| {
            e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call"
        })
        .count();
    assert_eq!(call_done, 1);
}

#[tokio::test]
async fn with_interrupt_response_off_the_turn_runs_beside_the_answer() {
    let mut r = Rig::new().await;
    speaking(&mut r);
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
    r.core.response_create(None, None);
    clause(&mut r, 1, "Hello there.", 2000).await;
    speech(&mut r, Instant::now(), true);
    let ev = r.events().await;
    assert_eq!(
        types(&ev).last(),
        Some(&"input_audio_buffer.speech_started")
    );
    assert!(r.core.active.is_some(), "not cancelled");
    // The stock client cancels itself on speech_started.
    r.core.response_cancel(None, None);
    let ev = r.events().await;
    assert_eq!(
        ev.last().unwrap()["response"]["status_details"]["reason"],
        "client_cancelled"
    );
}

#[tokio::test]
async fn the_window_is_open_while_the_answer_is_made_and_outlives_its_response() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    assert_eq!(r.core.listen(), Default::default());
    r.core.response_create(None, None);
    clause(&mut r, 1, "Hello there.", 300).await;
    let listen = r.core.listen();
    let view = listen.view.unwrap();
    assert!(listen.heard);
    assert_eq!((view.gen, view.end), (1, None), "still producing");
    r.finish(1, Ok(completion("Hello there.")));
    let view = r.core.listen().view.unwrap();
    // Its modelled end, plus the margin for the transit both ways: no
    // round trip measured here, so `echo_tail_ms` (250) alone.
    assert_eq!(r.core.echo_margin(), Duration::from_millis(250));
    assert_eq!(view.end, Some(view.first + Duration::from_millis(550)));
    r.events().await;
    r.core.on_drained(1);
    r.events().await;
    // Done: no response, and the window is still there to judge input
    // captured inside it.
    let listen = r.core.listen();
    assert!(!listen.heard);
    assert_eq!(listen.view, Some(view));
    // Such input, judged now, is a normal turn: nothing left to cut.
    speech(&mut r, view.first + Duration::from_millis(100), true);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["input_audio_buffer.speech_started"]);
    assert!(r.core.turn.is_some());
    // A turn whose speech began after the audio played out does not cut a
    // response that is only finishing.
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(None, None);
    clause(&mut r, 1, "Hi.", 50).await;
    r.finish(1, Ok(completion("Hi.")));
    let end = r.core.listen().view.unwrap().end.unwrap();
    tokio::time::sleep_until(end + Duration::from_millis(10)).await;
    speech(&mut r, Instant::now(), false);
    assert!(matches!(
        r.core.interruption(Instant::now()),
        Interruption::PlayedOut { .. }
    ));
    assert!(
        r.core.active.is_some(),
        "it finishes at its acknowledgement"
    );
}
