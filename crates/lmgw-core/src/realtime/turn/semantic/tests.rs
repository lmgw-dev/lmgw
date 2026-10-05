//! The `semantic_vad` rule through the detector (§6.3): frames with fixed
//! probabilities over silent audio, the scores injected; and once on a
//! committed clip with Silero and Smart Turn for real.

use super::*;
use crate::realtime::audio::pcm::f32_to_pcm16;
use crate::realtime::audio::resample::tests::stream;
use crate::realtime::audio::resample::StreamResampler;
use crate::realtime::audio::vad::{Vad, FRAME, SILERO_VAD_ONNX};
use crate::realtime::test_fixtures::wav_24k;
use crate::realtime::turn::server_vad::{ServerVad, ServerVadParams, TurnEvent};
use crate::realtime::turn::smart_turn::{SmartTurn, SMART_TURN_ONNX};

/// One Silero frame on the 24 kHz timeline (32 ms).
const F: u64 = 768;
const V: f32 = 0.9;
const U: f32 = 0.1;

/// The settings' defaults (§6.3): medium/auto, high and low.
fn medium() -> SemanticParams {
    SemanticParams {
        threshold: 0.5,
        floor: 0.2,
        floor_window_ms: 500,
        max_wait_ms: 4000,
    }
}

fn high() -> SemanticParams {
    SemanticParams {
        floor: 0.1,
        max_wait_ms: 2000,
        ..medium()
    }
}

fn low() -> SemanticParams {
    SemanticParams {
        threshold: 0.95,
        floor: 0.95,
        max_wait_ms: 3000,
        ..medium()
    }
}

/// A detector on `rule`, its plain window 500 ms (the no-score fallback).
fn vad(rule: SemanticParams) -> ServerVad {
    ServerVad::new(&ServerVadParams {
        semantic: Some(rule),
        ..ServerVadParams::default()
    })
    .unwrap()
}

/// What one frame did: its turn event and its score request.
type Step = (u64, Option<TurnEvent>, Option<ScoreRequest>);

/// Drives frames `first..` over silent audio; what each frame that did
/// something did, by frame index.
fn drive(sv: &mut ServerVad, first: u64, probs: &[f32]) -> Vec<Step> {
    let mut out = Vec::new();
    for (i, &p) in probs.iter().enumerate() {
        let k = first + i as u64;
        sv.push_audio(&[0; F as usize]);
        let event = sv.push_frame(p, k * F, (k + 1) * F);
        let request = sv.take_score();
        if event.is_some() || request.is_some() {
            out.push((k, event, request));
        }
    }
    out
}

/// Five voiced frames (a turn from frame 0), then `n` unvoiced ones.
fn turn_then_pause(sv: &mut ServerVad, n: usize) -> Vec<Step> {
    let started = drive(sv, 0, &[V; 5]);
    assert!(
        matches!(
            started[..],
            [(2, Some(TurnEvent::SpeechStarted { .. }), None)]
        ),
        "{started:?}"
    );
    drive(sv, 5, &vec![U; n])
}

/// The one request of a step list.
fn request(steps: &[Step]) -> ScoreRequest {
    match steps {
        [(_, None, Some(r))] => *r,
        other => panic!("expected one score request, got {other:?}"),
    }
}

/// `audio_end_ms` of a commit.
fn stop_ms(event: &Option<TurnEvent>) -> u64 {
    match event {
        Some(TurnEvent::SpeechStopped { audio_end_ms, .. }) => *audio_end_ms,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// The only step's commit.
fn commit_ms(steps: &[Step]) -> u64 {
    match steps {
        [(_, event, None)] => stop_ms(event),
        other => panic!("expected one commit, got {other:?}"),
    }
}

/// The 7th unvoiced frame (224 ms) asks for the turn up to 200 ms into the
/// pause — variant B, the silence included; a score at the threshold
/// commits right there, long before the 500 ms window, and says so.
#[test]
fn a_high_score_commits_at_the_probe() {
    let mut sv = vad(medium());
    let steps = turn_then_pause(&mut sv, 7);
    assert_eq!(steps[0].0, 11);
    let r = request(&steps);
    assert_eq!((r.start, r.end), (0, 5 * F + 4800));
    let (decision, event) = sv.scored(r.id, Some(0.5));
    assert_eq!(decision, Some(Decision::Now));
    assert_eq!(stop_ms(&event), 12 * 32);
    assert!(!sv.in_speech());
    assert_eq!(
        sv.take_end(),
        Some(TurnEnd {
            rule: EndRule::Threshold { p: 0.5 },
            held: false
        })
    );
    assert_eq!(sv.take_end(), None, "taken once");
}

/// Between the floor and the threshold the plain 500 ms window commits
/// (16 frames, 512 ms); the answer is in at 224 ms.
#[test]
fn an_unsure_score_commits_at_the_floor_window() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(
        sv.scored(r.id, Some(0.2)),
        (Some(Decision::FloorWindow(500)), None)
    );
    assert!(drive(&mut sv, 12, &[U; 8]).is_empty());
    let steps = drive(&mut sv, 20, &[U; 1]);
    assert_eq!(commit_ms(&steps), 21 * 32);
    assert_eq!(
        sv.take_end().map(|e| e.rule),
        Some(EndRule::Floor { p: 0.2 })
    );
}

/// An answer that comes after the floor window has passed commits at once.
#[test]
fn a_late_floor_answer_commits_when_it_comes() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert!(
        drive(&mut sv, 12, &[U; 20]).is_empty(),
        "pending is awaited"
    );
    let (_, event) = sv.scored(r.id, Some(0.3));
    assert_eq!(stop_ms(&event), 32 * 32);
}

/// Below the floor the 500 ms window does not cut it short: the turn
/// commits at the 4 s max wait (125 frames of silence).
#[test]
fn a_low_score_waits_for_the_max_wait() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(
        sv.scored(r.id, Some(0.19)),
        (Some(Decision::MaxWait(4000)), None)
    );
    assert!(drive(&mut sv, 12, &[U; 117]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 129, &[U; 1])), 130 * 32);
    assert_eq!(
        sv.take_end().map(|e| e.rule),
        Some(EndRule::MaxWait { p: Some(0.19) })
    );
}

/// Low eagerness has no middle band: 0.94 waits the 3 s (94 frames).
#[test]
fn low_eagerness_has_no_floor() {
    let mut sv = vad(low());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(
        sv.scored(r.id, Some(0.94)),
        (Some(Decision::MaxWait(3000)), None)
    );
    assert!(drive(&mut sv, 12, &[U; 86]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 98, &[U; 1])), 99 * 32);
}

/// Resumed voice continues the turn; its next pause gets a fresh request
/// (up to 200 ms into it), and the old pause's late answer is ignored.
#[test]
fn resumed_voice_continues_and_rescores() {
    let mut sv = vad(medium());
    let first = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(
        sv.scored(first.id, Some(0.1)).0,
        Some(Decision::MaxWait(4000))
    );
    assert!(drive(&mut sv, 12, &[V; 3]).is_empty());
    let second = request(&drive(&mut sv, 15, &[U; 7]));
    assert_ne!(second.id, first.id);
    assert_eq!(second.end, 15 * F + 4800);
    assert_eq!(
        sv.scored(first.id, Some(0.99)),
        (None, None),
        "stale answer"
    );
    assert!(sv.scored(second.id, Some(0.99)).1.is_some());
}

/// A blip shorter than `resume_ms` keeps the pause: no new request, and
/// the pending answer still counts.
#[test]
fn blips_keep_the_pause() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert!(drive(&mut sv, 12, &[V, V, U, U]).is_empty());
    assert!(sv.scored(r.id, Some(0.8)).1.is_some());
}

/// WP6 review M2: an answer that comes while the voice is back, but for
/// less than `resume_ms`, commits nothing yet — ended there, the commit
/// would split a word. The next unvoiced frame commits; enough voice
/// clears the pause and its answer.
#[test]
fn an_answer_during_resumed_voice_waits_for_the_next_unvoiced_frame() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert!(drive(&mut sv, 12, &[V, V]).is_empty());
    assert_eq!(sv.scored(r.id, Some(0.8)), (Some(Decision::Now), None));
    assert!(sv.in_speech(), "no commit inside the word");
    assert_eq!(commit_ms(&drive(&mut sv, 14, &[U])), 15 * 32);
    assert_eq!(
        sv.take_end().map(|e| e.rule),
        Some(EndRule::Threshold { p: 0.8 })
    );

    // The voice goes on for `resume_ms` (three frames): the pause is over,
    // its answer with it, and the turn goes on to a pause of its own.
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert!(drive(&mut sv, 12, &[V, V]).is_empty());
    assert_eq!(sv.scored(r.id, Some(0.8)), (Some(Decision::Now), None));
    assert!(drive(&mut sv, 14, &[V]).is_empty());
    assert!(drive(&mut sv, 15, &[U; 6]).is_empty(), "no commit");
    assert!(sv.in_speech());
    let next = request(&drive(&mut sv, 21, &[U]));
    assert_ne!(next.id, r.id);
}

/// WP6 review: the request ids are the detector's, so a switch to
/// `server_vad` and back does not start them over — the old pause's late
/// answer matches nothing of the new rule's.
#[test]
fn request_ids_go_on_across_a_switch_to_server_vad_and_back() {
    let mut sv = vad(medium());
    let old = request(&turn_then_pause(&mut sv, 7));
    let plain = ServerVadParams {
        silence_duration_ms: 2000,
        ..ServerVadParams::default()
    };
    sv.set_params(&plain).unwrap();
    sv.set_params(&ServerVadParams {
        semantic: Some(medium()),
        ..plain
    })
    .unwrap();
    let new = request(&drive(&mut sv, 12, &[U]));
    assert_eq!(new.id, old.id + 1);
    assert_eq!(sv.scored(old.id, Some(0.99)), (None, None), "the old pause");
    assert_eq!(sv.scored(new.id, Some(0.99)).0, Some(Decision::Now));
}

/// No score: the pause falls back to the plain window (500 ms = 16
/// frames), at once if the failure arrives after it. A non-finite score
/// is a failure.
#[test]
fn no_score_falls_back_to_the_silence_window() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(sv.scored(r.id, None), (Some(Decision::Fallback), None));
    assert!(drive(&mut sv, 12, &[U; 8]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 20, &[U; 1])), 21 * 32);
    assert_eq!(sv.take_end().map(|e| e.rule), Some(EndRule::Fallback));

    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    assert!(drive(&mut sv, 12, &[U; 20]).is_empty());
    let (decision, event) = sv.scored(r.id, Some(f32::NAN));
    assert_eq!(decision, Some(Decision::Fallback));
    assert_eq!(stop_ms(&event), 32 * 32, "a late failure commits now");
}

/// A pending score is awaited past the plain window; none at all still
/// ends at the max wait (2 s = 62.5 frames: the 63rd unvoiced frame).
#[test]
fn a_pending_score_is_awaited_up_to_the_max_wait() {
    let mut sv = vad(high());
    turn_then_pause(&mut sv, 7);
    assert!(drive(&mut sv, 12, &[U; 55]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 67, &[U; 1])), 68 * 32);
    assert_eq!(
        sv.take_end().map(|e| e.rule),
        Some(EndRule::MaxWait { p: None })
    );
}

/// After a barge-in nothing commits before 1500 ms (47 frames), whatever
/// the score, and the timing line says the window held it; the turn after
/// is back to normal.
#[test]
fn the_post_interrupt_window_holds_every_rule() {
    let mut sv = vad(medium());
    sv.arm_post_interrupt();
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(sv.scored(r.id, Some(0.95)), (Some(Decision::Now), None));
    assert!(drive(&mut sv, 12, &[U; 39]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 51, &[U; 1])), 52 * 32);
    assert_eq!(
        sv.take_end(),
        Some(TurnEnd {
            rule: EndRule::Threshold { p: 0.95 },
            held: true
        })
    );
    drive(&mut sv, 52, &[V; 5]);
    let r = request(&drive(&mut sv, 57, &[U; 7]));
    assert!(sv.scored(r.id, Some(0.95)).1.is_some());

    // A wait longer than the post-interrupt window is not shortened by it.
    let mut sv = vad(medium());
    sv.arm_post_interrupt();
    let r = request(&turn_then_pause(&mut sv, 7));
    sv.scored(r.id, Some(0.0));
    assert!(drive(&mut sv, 12, &[U; 117]).is_empty());
    assert_eq!(commit_ms(&drive(&mut sv, 129, &[U; 1])), 130 * 32);
}

/// While the word check keeps a turn open (§6.4) no score is asked and the
/// plain window is the one that counts; once it is a normal turn, its
/// pause is scored up to 200 ms into it.
#[test]
fn a_turn_kept_open_is_judged_without_smart_turn() {
    let mut sv = vad(medium());
    drive(&mut sv, 0, &[V; 5]);
    sv.keep_open(true);
    assert!(drive(&mut sv, 5, &[U; 20]).is_empty(), "no request");
    assert!(sv.stop_due(), "the plain 500 ms window passed");
    sv.keep_open(false);
    assert!(!sv.stop_due(), "now the rule's: unscored, the max wait");
    let r = request(&drive(&mut sv, 25, &[U; 1]));
    assert_eq!(r.end, 5 * F + 4800);
    assert!(sv.scored(r.id, Some(0.7)).1.is_some());
}

/// The request covers the last 8 s up to 200 ms into the pause, and the
/// detector still holds that audio.
#[test]
fn the_request_is_the_last_8_s() {
    let mut sv = vad(medium());
    drive(&mut sv, 0, &[V; 400]);
    let r = request(&drive(&mut sv, 400, &[U; 7]));
    assert_eq!(r.end, 400 * F + 4800);
    assert_eq!(r.end - r.start, 8 * 24_000);
    assert_eq!(sv.audio(r.start, r.end).len(), 8 * 24_000, "retained");
}

#[test]
fn stale_and_unknown_answers_change_nothing() {
    let mut sv = vad(medium());
    assert_eq!(sv.scored(0, Some(1.0)), (None, None), "nothing asked");
    let r = request(&turn_then_pause(&mut sv, 7));
    assert_eq!(sv.scored(r.id + 1, Some(1.0)), (None, None), "unknown id");
    assert_eq!(sv.scored(r.id, Some(0.1)).0, Some(Decision::MaxWait(4000)));
    assert_eq!(sv.scored(r.id, Some(1.0)), (None, None), "answered already");
    sv.reset();
    assert!(!sv.in_speech());
    assert_eq!(sv.scored(r.id, None), (None, None), "cleared");
    // Plain server_vad has no rule to answer.
    let mut plain = ServerVad::new(&ServerVadParams::default()).unwrap();
    assert_eq!(plain.scored(0, Some(1.0)), (None, None));
    assert!(!plain.semantic());
}

/// A retune keeps the pause and its score, and applies the new knobs to it.
#[test]
fn a_retune_keeps_the_pause() {
    let mut sv = vad(medium());
    let r = request(&turn_then_pause(&mut sv, 7));
    sv.scored(r.id, Some(0.3));
    sv.set_params(&ServerVadParams {
        semantic: Some(SemanticParams {
            threshold: 0.3,
            ..medium()
        }),
        ..ServerVadParams::default()
    })
    .unwrap();
    assert!(sv.stop_due(), "0.3 now reaches the threshold");
    // Plain server_vad from here: the 500 ms window.
    sv.set_params(&ServerVadParams::default()).unwrap();
    assert!(!sv.semantic());
}

#[test]
fn rejects_bad_parameters() {
    for (t, f) in [(-0.1, 0.0), (1.5, 0.2), (f32::NAN, 0.2), (0.5, 0.6)] {
        let p = SemanticParams {
            threshold: t,
            floor: f,
            ..medium()
        };
        assert!(p.validate().is_err(), "{t} / {f}");
        let err = ServerVad::new(&ServerVadParams {
            semantic: Some(p),
            ..ServerVadParams::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("realtime.semantic_vad"), "{err}");
    }
    assert!(low().validate().is_ok(), "floor == threshold");
    // Fix package B6: the floor window past the maximum wait.
    let p = SemanticParams {
        floor_window_ms: 2500,
        ..high()
    };
    assert_eq!(
        p.validate(),
        Err(SemanticParamError::FloorWindowAboveMaxWait {
            floor_window_ms: 2500,
            max_wait_ms: 2000
        })
    );
    assert!(SemanticParams {
        floor_window_ms: 2000,
        ..high()
    }
    .validate()
    .is_ok());
}

#[test]
fn the_timing_line_names_the_rule() {
    let end = |rule, held| TurnEnd { rule, held }.to_string();
    assert_eq!(
        end(EndRule::Threshold { p: 0.974 }, false),
        "semantic_vad threshold, p 0.97"
    );
    assert_eq!(
        end(EndRule::MaxWait { p: None }, true),
        "semantic_vad max wait, no score yet; held by post_interrupt_silence_ms"
    );
    assert_eq!(
        end(EndRule::Fallback, false),
        "semantic_vad silence fallback, no score"
    );
}

/// The real chain on the mid-sentence clip at high eagerness: Silero
/// frames, the requests scored by Smart Turn on our own 16 kHz stream (the
/// ring the session keeps). The 400 ms pause scores low and holds (the
/// plain 300 ms window alone would split there); the sentence end scores
/// high and commits at the 224 ms probe.
#[test]
fn smart_turn_holds_a_mid_sentence_pause() {
    let mut x = wav_24k("en_midsentence_pause.wav");
    x.extend(std::iter::repeat_n(0.0, 24 * 1000));
    let pcm = f32_to_pcm16(&x);
    let y = stream(&mut StreamResampler::new().unwrap(), &x);
    let mut silero = Vad::from_bytes(SILERO_VAD_ONNX).unwrap();
    let mut st = SmartTurn::from_bytes(SMART_TURN_ONNX, 1).unwrap();
    let mut sv = vad(high());
    let (mut scores, mut commits) = (Vec::new(), Vec::new());
    for (k, frame) in y.as_chunks::<FRAME>().0.iter().enumerate() {
        let (s, e) = (k as u64 * F, (k as u64 + 1) * F);
        sv.push_audio(&pcm[(s as usize).min(pcm.len())..(e as usize).min(pcm.len())]);
        let mut event = sv.push_frame(silero.probability(frame).unwrap(), s, e);
        if let Some(r) = sv.take_score() {
            // 24 kHz timeline → the aligned 16 kHz stream: two thirds.
            let audio = &y[(r.start * 2 / 3) as usize..(r.end * 2 / 3) as usize];
            let p = st.score(audio).unwrap();
            scores.push((r.end / 24, p));
            event = sv.scored(r.id, Some(p)).1;
        }
        if let Some(TurnEvent::SpeechStopped { audio_end_ms, .. }) = event {
            commits.push(audio_end_ms);
        }
    }
    eprintln!("pause scores (score end ms, p): {scores:?}; commits at {commits:?} ms");
    assert_eq!(scores.len(), 2, "{scores:?}");
    assert!(scores[0].1 < 0.1 && scores[1].1 >= 0.5, "{scores:?}");
    // One turn, committed at the second probe: 4399 ms speech end,
    // Silero's tail of up to two frames, then 7 frames.
    assert_eq!(commits.len(), 1);
    assert!(commits[0].abs_diff(4399 + 224) <= 96, "{commits:?}");
}
