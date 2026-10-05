//! What a barge-in owes after a response the client never heard (realtime
//! design §4.3, B3 review 1, 4): "heard" is audio or text that left, not an
//! announced item — and a client's `response.create` is held to start
//! again.

use tokio::time::Instant;

use super::*;
use crate::realtime::audio_in::Detected;
use crate::realtime::turn::server_vad::TurnEvent;
use crate::realtime::voice::{SpeakVoice, VoiceOutcome, VoiceVia};

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

fn speech(r: &mut Rig) {
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 4000,
            onset_ms: 4300,
        },
        speech_end: None,
        at: Instant::now(),
        barge_in: false,
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

fn created(events: &[Value]) -> usize {
    events
        .iter()
        .filter(|e| e["type"] == "response.created")
        .count()
}

#[tokio::test]
async fn a_client_response_cut_before_it_was_heard_starts_again_after_the_turn() {
    // `@openai/agents`' tool follow-up, still generating, cut by a cough
    // that turns out to have no words: the tool's result is still spoken.
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(Some("follow".into()), None);
    assert!(!r.core.listen().heard, "nothing of it left yet");
    speech(&mut r);
    let ev = r.events().await;
    assert_eq!(
        ev.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    commit(&mut r, " ");
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.created"]);
    let active = r.core.active.as_ref().unwrap();
    assert_eq!(active.event_id.as_deref(), Some("follow"));
    assert!(r.core.pending.is_none());
}

#[tokio::test]
async fn an_announced_function_call_is_not_heard_and_the_question_is_owed_again() {
    let mut r = Rig::new().await;
    let question = user_turn(&mut r, "Wie spät ist es?");
    r.core.pending_commit(&question, true, false);
    r.core.pending_resolve(&question, false);
    r.core.on_responder(
        1,
        Msg::Delta(StreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "get_time".into(),
        }),
    );
    r.events().await;
    assert!(!r.core.listen().heard);
    speech(&mut r);
    commit(&mut r, "");
    let ev = r.events().await;
    assert_eq!(created(&ev), 1, "{:?}", types(&ev));
    let active = r.core.active.as_ref().unwrap();
    assert_eq!(active.answers.first(), Some(&question));
    assert_eq!(active.event_id, None, "the automatic response");
}

#[tokio::test]
async fn the_create_queued_behind_an_unheard_cut_is_the_one_started_again() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(Some("first".into()), None);
    // A clause the writer never got to send, then the end of generation:
    // Closing, and the client's follow-up is queued.
    r.core.on_responder(
        1,
        Msg::Clause {
            text: "Let me look.".into(),
            written: crate::realtime::heard::Written::said("", "Let me look."),
            pcm: bytes::Bytes::from(vec![0u8; 4800]),
        },
    );
    r.core
        .on_responder(1, Msg::Delta(StreamDelta::Stop(FinishReason::ToolUse)));
    r.core.response_create(Some("follow".into()), None);
    speech(&mut r);
    let ev = r.events().await;
    assert!(
        !ev.iter()
            .any(|e| e["type"] == "response.output_audio.delta"),
        "nothing of it left"
    );
    assert_eq!(ev.last().unwrap()["response"]["status"], "cancelled");
    // The turn ends: one response, the follow-up — not "first" again too.
    commit(&mut r, "noch etwas");
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.created"]);
    assert_eq!(
        r.core.active.as_ref().unwrap().event_id.as_deref(),
        Some("follow")
    );
    assert!(r.core.pending.is_none());
}

#[tokio::test]
async fn a_response_whose_audio_left_is_heard_and_not_started_again() {
    let mut r = Rig::new().await;
    speaking(&mut r);
    r.core.response_create(Some("first".into()), None);
    r.core.on_responder(
        1,
        Msg::Clause {
            text: "Hello there.".into(),
            written: crate::realtime::heard::Written::said("", "Hello there."),
            pcm: bytes::Bytes::from(vec![0u8; 4800]),
        },
    );
    r.core.flush().await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(r.core.listen().heard);
    speech(&mut r);
    commit(&mut r, "");
    let ev = r.events().await;
    assert_eq!(created(&ev), 1, "only the first: {:?}", types(&ev));
    assert!(r.core.active.is_none() && r.core.pending.is_none());
}
