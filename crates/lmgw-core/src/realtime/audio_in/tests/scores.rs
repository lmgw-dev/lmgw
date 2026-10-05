//! `semantic_vad`'s scores through the input buffer (§6.3): the request
//! leaves with its audio from the 16 kHz ring, and the answer ends the
//! turn between two appends.

use super::*;
use crate::realtime::turn::semantic::{Decision, EndRule, SemanticParams};
use crate::realtime::turn::smart_turn::{SmartTurn, SMART_TURN_ONNX};

fn semantic() -> ServerVadParams {
    ServerVadParams {
        semantic: Some(SemanticParams {
            threshold: 0.5,
            floor: 0.2,
            floor_window_ms: 500,
            max_wait_ms: 4000,
        }),
        ..ServerVadParams::default()
    }
}

/// The complete question plus a second of silence: one request, for the
/// turn up to 200 ms into the pause at 16 kHz; Smart Turn calls it
/// complete, and the answer commits the turn at once.
#[tokio::test]
async fn a_pause_is_scored_from_the_ring_and_the_answer_commits() {
    let mut a = AudioIn::new(Some(&semantic())).unwrap();
    let mut pcm = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    pcm.extend(vec![0; 24_000]);
    let appended = feed(&mut a, &pcm).await;
    assert!(matches!(
        appended.events[..],
        [Detected {
            event: TurnEvent::SpeechStarted { .. },
            ..
        }]
    ));
    let [job] = &appended.scores[..] else {
        panic!("one request: {:?}", appended.scores);
    };
    // The speech ends at 1738 ms (the sidecar); Silero lags it by a frame
    // or two, and the request reaches 200 ms past that.
    assert!(job.pause_ms.abs_diff(1738) <= 96, "{}", job.pause_ms);
    let turn = a.detector.turn_start().unwrap();
    let want = ((job.pause_ms * 24 + 4800 - turn) * 2 / 3) as usize;
    assert!(
        job.samples.len().abs_diff(want) <= 2,
        "{} vs {want}",
        job.samples.len()
    );
    let p = SmartTurn::from_bytes(SMART_TURN_ONNX, 1)
        .unwrap()
        .score(&job.samples)
        .unwrap();
    assert!(p >= 0.9, "{p}");
    // Stale ids decide nothing; the right one ends the turn now.
    assert!(a.turn_scored(job.id + 1, Some(p)).0.is_none());
    let (decision, out) = a.turn_scored(job.id, Some(p));
    assert_eq!(decision, Some(Decision::Now));
    let [Detected {
        event: TurnEvent::SpeechStopped { .. },
        end: Some(end),
        ..
    }] = &out.events[..]
    else {
        panic!("the turn's end: {:?}", out.events.len());
    };
    assert_eq!(end.rule, EndRule::Threshold { p });
    assert!(!a.detector.in_speech());
}

/// A pause that cannot be scored falls back to the plain window: past it
/// already, the turn ends with the failure.
#[tokio::test]
async fn no_score_falls_back_to_the_plain_window() {
    let mut a = AudioIn::new(Some(&semantic())).unwrap();
    let mut pcm = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    pcm.extend(vec![0; 24_000]);
    let appended = feed(&mut a, &pcm).await;
    let id = appended.scores[0].id;
    assert!(a.detector.in_speech(), "a pending score is awaited");
    let (decision, out) = a.turn_scored(id, None);
    assert_eq!(decision, Some(Decision::Fallback));
    assert_eq!(out.events[0].end.map(|e| e.rule), Some(EndRule::Fallback));
}

/// Plain `server_vad` keeps no 16 kHz audio, and a switch to
/// `semantic_vad` mid-session finds the ring in step with the timeline.
#[tokio::test]
async fn the_ring_stays_in_step_across_a_switch() {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    feed(&mut a, &noise(3)).await;
    assert_eq!(a.dsp.as_ref().unwrap().ring.span(0, u64::MAX).len(), 0);
    a.configure(Some(&semantic())).unwrap();
    let mut pcm = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    pcm.extend(vec![0; 24_000]);
    let appended = feed(&mut a, &pcm).await;
    let job = &appended.scores[0];
    assert!(job.pause_ms.abs_diff(3000 + 1738) <= 96, "{}", job.pause_ms);
    let p = SmartTurn::from_bytes(SMART_TURN_ONNX, 1)
        .unwrap()
        .score(&job.samples)
        .unwrap();
    assert!(p >= 0.9, "the same audio scores the same: {p}");
}
