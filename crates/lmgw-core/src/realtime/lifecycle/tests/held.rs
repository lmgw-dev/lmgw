//! A client's `response.create` held while the user speaks (realtime design
//! §4.3, owner's decision Q3): what ends the wait, and what a second one
//! meets meanwhile (B3 review 6, 7).

use tokio::time::Instant;

use super::*;
use crate::realtime::audio_in::Detected;
use crate::realtime::transcribe::Done;
use crate::realtime::turn::server_vad::{Segment, TurnEvent};

/// The detector started a turn.
fn speech(r: &mut Rig) {
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 0,
            onset_ms: 300,
        },
        speech_end: None,
        at: Instant::now(),
        barge_in: false,
        end: None,
    });
}

#[tokio::test]
async fn a_second_create_while_one_is_held_is_refused_until_the_transcript_is_in() {
    let mut r = Rig::new().await;
    speech(&mut r);
    r.core.response_create(Some("tool".into()), None);
    r.events().await;
    // The turn commits; its transcript is still being made.
    r.core.turn = None;
    let id = user_turn(&mut r, "Wie spät ist es?");
    r.core
        .transcriber
        .push(id.clone(), "asr".into(), Vec::new(), None, None);
    r.core.pending_commit(&id, true, false);
    // Nobody speaks and nothing is active — but the held create answers
    // this turn, so another one now would be a second response.
    r.core.response_create(Some("second".into()), None);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["error"]);
    assert_eq!(
        ev[0]["error"]["code"],
        "conversation_already_has_active_response"
    );
    assert_eq!(ev[0]["error"]["event_id"], "second");
    assert!(r.core.active.is_none());
    // The transcript: the held create starts, once.
    r.core.on_transcript(Done {
        item_id: id,
        seconds: 1.0,
        result: Ok("Wie spät ist es?".into()),
        again: None,
        facts: Default::default(),
    });
    let ev = r.events().await;
    assert_eq!(
        ev.iter()
            .filter(|e| e["type"] == "response.created")
            .count(),
        1,
        "{:?}",
        types(&ev)
    );
    assert_eq!(
        r.core.active.as_ref().unwrap().event_id.as_deref(),
        Some("tool")
    );
}

#[tokio::test]
async fn a_turn_that_cannot_be_transcribed_still_starts_the_held_create() {
    // B3 review 7: no ASR alias — the turn ends with asr_not_configured,
    // and the create held for it must not wait for a commit that never
    // comes.
    let mut r = Rig::new().await;
    speech(&mut r);
    r.core.response_create(Some("tool".into()), None);
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStopped {
            audio_end_ms: 2000,
            segment: Segment {
                start_sample: 0,
                samples: vec![0; 480],
            },
        },
        speech_end: None,
        at: Instant::now(),
        barge_in: false,
        end: None,
    });
    let ev = r.events().await;
    assert_eq!(
        types(&ev),
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "error",
            "response.created",
        ]
    );
    assert_eq!(ev[2]["error"]["code"], "asr_not_configured");
    assert!(r.core.pending.is_none());
}
