//! The playing window as the input side reads it (realtime design §6.4, B3
//! review 2, 8): open-ended while audio of the answer still waits, and
//! ending a margin after the modelled playback.

use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::realtime::lifecycle::Interruption;
use crate::realtime::voice::{SpeakVoice, VoiceOutcome, VoiceVia};

/// A spoken session whose audio leaves just in time (lead 0).
fn speaking_just_in_time(r: &mut Rig) {
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
        .output_lead_ms = Some(0);
}

#[tokio::test]
async fn the_window_stays_open_while_audio_of_the_answer_still_waits() {
    let mut r = Rig::new().await;
    speaking_just_in_time(&mut r);
    r.core.response_create(None, None);
    r.core.on_responder(
        1,
        Msg::Clause {
            text: "A long answer.".into(),
            written: crate::realtime::heard::Written::said("", "A long answer."),
            pcm: bytes::Bytes::from(vec![0u8; 2000 * 48]),
        },
    );
    r.core.flush().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    r.finish(1, Ok(completion("A long answer.")));
    // The call is over (Playing), and what left plays out within 100 ms —
    // but nearly two seconds of the answer still wait in the writer.
    let view = r.core.listen().view.unwrap();
    assert_eq!(view.end, None, "open-ended while audio waits");
    let later = Instant::now() + Duration::from_millis(300);
    assert!(
        matches!(r.core.interruption(later), Interruption::Cuts { .. }),
        "speech now cuts it: the answer is not over"
    );
}

#[tokio::test]
async fn the_window_ends_a_round_trip_and_the_echo_tail_after_the_playback() {
    let mut r = Rig::new().await;
    speaking_just_in_time(&mut r);
    let lmgw = r.core.session.lmgw.get_or_insert_with(Default::default);
    lmgw.echo_tail_ms = Some(100);
    lmgw.output_lead_ms = Some(60_000);
    r.core.response_create(None, None);
    r.core.on_responder(
        1,
        Msg::Clause {
            text: "Hi.".into(),
            written: crate::realtime::heard::Written::said("", "Hi."),
            pcm: bytes::Bytes::from(vec![0u8; 200 * 48]),
        },
    );
    r.core.flush().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    r.finish(1, Ok(completion("Hi.")));
    let first = r.core.listen().view.unwrap().first;
    // No ping answered yet: the tail alone.
    assert_eq!(
        r.core.listen().view.unwrap().end,
        Some(first + Duration::from_millis(300))
    );
    // A measured round trip moves it further.
    let rt = r.core.out.round_trip();
    let t = Instant::now();
    let p = rt.pinged(t);
    rt.ponged(t + Duration::from_millis(40), &p);
    assert_eq!(r.core.echo_margin(), Duration::from_millis(140));
    assert_eq!(
        r.core.listen().view.unwrap().end,
        Some(first + Duration::from_millis(340))
    );
    // The cut still judges by the modelled end: speech in the margin finds
    // an answer that played out, not one to interrupt.
    assert!(matches!(
        r.core.interruption(first + Duration::from_millis(250)),
        Interruption::PlayedOut { .. }
    ));
}
