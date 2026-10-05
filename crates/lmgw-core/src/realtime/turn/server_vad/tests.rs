use super::*;
use crate::realtime::audio::pcm::f32_to_pcm16;
use crate::realtime::audio::resample::tests::stream;
use crate::realtime::audio::resample::StreamResampler;
use crate::realtime::audio::vad::{Vad, FRAME, SILERO_VAD_ONNX};
use crate::realtime::test_fixtures::wav_24k;

/// One Silero frame on the 24 kHz timeline.
const F: u64 = 768;
const V: f32 = 0.9;
const U: f32 = 0.1;

/// Drives frames `first..` with the given probabilities over silent audio.
fn drive(sv: &mut ServerVad, first: u64, probs: &[f32]) -> Vec<(u64, TurnEvent)> {
    let mut out = Vec::new();
    for (i, &p) in probs.iter().enumerate() {
        let k = first + i as u64;
        sv.push_audio(&[0; F as usize]);
        out.extend(sv.push_frame(p, k * F, (k + 1) * F).map(|e| (k, e)));
    }
    out
}

fn frames(pattern: &[(f32, usize)]) -> Vec<f32> {
    pattern
        .iter()
        .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
        .collect()
}

/// The real pipeline on a committed clip plus `pad_ms` of synthesized
/// silence: 24 kHz PCM and one Silero probability per frame.
fn fixture(name: &str, pad_ms: usize) -> (Vec<i16>, Vec<f32>) {
    let mut x = wav_24k(name);
    x.extend(std::iter::repeat_n(0.0, pad_ms * 24));
    let y = stream(&mut StreamResampler::new().unwrap(), &x);
    let mut vad = Vad::from_bytes(SILERO_VAD_ONNX).unwrap();
    let probs = y
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(|f| vad.probability(f).unwrap())
        .collect();
    (f32_to_pcm16(&x), probs)
}

fn run(params: &ServerVadParams, audio: &[i16], probs: &[f32]) -> Vec<TurnEvent> {
    let mut sv = ServerVad::new(params).unwrap();
    let mut events = Vec::new();
    for (k, &p) in probs.iter().enumerate() {
        let (s, e) = (k as u64 * F, (k as u64 + 1) * F);
        let clip = |i: u64| (i as usize).min(audio.len());
        sv.push_audio(&audio[clip(s)..clip(e)]);
        events.extend(sv.push_frame(p, s, e));
    }
    events
}

fn params(silence_ms: u32) -> ServerVadParams {
    ServerVadParams {
        silence_duration_ms: silence_ms,
        ..ServerVadParams::default()
    }
}

/// A silence window as the detector sees it: whole 32 ms frames.
fn window_ms(ms: u64) -> u64 {
    ms.div_ceil(32) * 32
}

/// Checks a turn against ground truth: onset within one frame; the last
/// voiced frame within two (Silero's own reference holds the first
/// sentence's last voiced frame 54 ms past the generated end); the
/// segment covers the speech and starts at the padded onset.
fn assert_turn(started: &TurnEvent, stopped: &TurnEvent, gt: (u64, u64), silence: u64) {
    let TurnEvent::SpeechStarted {
        audio_start_ms,
        onset_ms,
    } = *started
    else {
        panic!("expected a start, got {started:?}")
    };
    let TurnEvent::SpeechStopped {
        audio_end_ms,
        ref segment,
    } = *stopped
    else {
        panic!("expected a stop, got {stopped:?}")
    };
    assert!(
        onset_ms.abs_diff(gt.0) <= 32,
        "onset {onset_ms} vs {}",
        gt.0
    );
    let speech_end = audio_end_ms - window_ms(silence);
    assert!(
        speech_end.abs_diff(gt.1) <= 64,
        "end {speech_end} vs {}",
        gt.1
    );
    assert_eq!(segment.start_sample, audio_start_ms * 24);
    assert_eq!(
        segment.samples.len() as u64,
        (audio_end_ms - audio_start_ms) * 24
    );
    assert!(audio_start_ms <= gt.0 && audio_end_ms >= gt.1);
}

#[test]
fn two_sentences_split_at_500_ms() {
    let (audio, probs) = fixture("en_two_sentences_pause.wav", 1000);
    let ev = run(&params(500), &audio, &probs);
    assert_eq!(ev.len(), 4, "{ev:?}");
    assert_turn(&ev[0], &ev[1], (300, 1098), 500);
    assert_turn(&ev[2], &ev[3], (1798, 2849), 500);
    // The second turn's pre-roll stops at the first commit.
    let (
        TurnEvent::SpeechStopped { audio_end_ms, .. },
        TurnEvent::SpeechStarted { audio_start_ms, .. },
    ) = (&ev[1], &ev[2])
    else {
        unreachable!()
    };
    assert!(audio_start_ms >= audio_end_ms);
    // First turn: onset 320 ms, padded to 20 ms; speech_stopped 512 ms
    // after the last voiced frame (1152 ms).
    assert_eq!(
        ev[0],
        TurnEvent::SpeechStarted {
            audio_start_ms: 20,
            onset_ms: 320
        }
    );
}

#[test]
fn two_sentences_stay_one_turn_at_800_ms() {
    let (audio, probs) = fixture("en_two_sentences_pause.wav", 1000);
    let ev = run(&params(800), &audio, &probs);
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_turn(&ev[0], &ev[1], (300, 2849), 800);
}

#[test]
fn noise_and_silence_make_no_turns() {
    let (audio, probs) = fixture("noise_only.wav", 1000);
    assert!(probs.iter().all(|&p| p < 0.2), "{probs:?}");
    assert!(run(&params(500), &audio, &probs).is_empty());
}

#[test]
fn blips_do_not_hold_a_turn_open() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    // Five voiced frames: the onset confirms on the third (96 ms).
    let ev = drive(&mut sv, 0, &[V; 5]);
    assert_eq!(ev[0].0, 2);
    // A one-frame blip every fifth frame: 16 unvoiced frames still end
    // the turn, blips neither reset nor add (frame 23, end 768 ms).
    let pattern: Vec<f32> = (0..30).map(|j| if j % 5 == 4 { V } else { U }).collect();
    let ev = drive(&mut sv, 5, &pattern);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, 23);
    assert!(matches!(
        ev[0].1,
        TurnEvent::SpeechStopped {
            audio_end_ms: 768,
            ..
        }
    ));
}

#[test]
fn sustained_voice_resets_the_silence_count() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[V; 5]);
    // 64 ms of voice is a blip: 10 + 6 unvoiced frames still end it.
    let ev = drive(&mut sv, 5, &frames(&[(U, 10), (V, 2), (U, 6)]));
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, 5 + 17);
    // 96 ms of voice resets: 10 + 15 unvoiced do not end it, 16 do.
    drive(&mut sv, 30, &[V; 5]);
    let ev = drive(&mut sv, 35, &frames(&[(U, 10), (V, 3), (U, 16)]));
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, 35 + 28);
}

#[test]
fn post_interrupt_window_applies_to_one_turn() {
    let p = params(500);
    // Armed while idle: the next turn needs 1500 ms (47 frames), the one
    // after is back to 500 ms (16 frames).
    let mut sv = ServerVad::new(&p).unwrap();
    sv.arm_post_interrupt();
    drive(&mut sv, 0, &[V; 3]);
    assert!(drive(&mut sv, 3, &[U; 46]).is_empty());
    assert_eq!(drive(&mut sv, 49, &[U; 1]).len(), 1);
    drive(&mut sv, 50, &[V; 3]);
    let ev = drive(&mut sv, 53, &[U; 16]);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, 68);
    // Armed mid-turn: that turn uses it.
    drive(&mut sv, 70, &[V; 3]);
    sv.arm_post_interrupt();
    assert!(drive(&mut sv, 73, &[U; 46]).is_empty());
    assert_eq!(drive(&mut sv, 119, &[U; 1]).len(), 1);
}

#[test]
fn idle_retention_is_the_pre_roll() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[U; 100]);
    assert_eq!(sv.retained(), 7200, "300 ms at 24 kHz");
    // An absurd padding retains what arrived, nothing more.
    let mut wide = ServerVad::new(&ServerVadParams {
        prefix_padding_ms: u32::MAX,
        ..params(500)
    })
    .unwrap();
    drive(&mut wide, 0, &[U; 10]);
    assert_eq!(wide.retained(), 10 * F as usize);
}

#[test]
fn begin_at_back_dates_into_held_audio() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[U; 50]);
    // The barge-in gate has a pending onset at frame 50 from here on.
    sv.hold_from(Some(50 * F));
    drive(&mut sv, 50, &frames(&[(V, 1), (U, 2), (V, 1), (U, 26)]));
    assert!(!sv.in_speech(), "scattered voice never confirmed an onset");
    let started = sv.begin_at(50 * F);
    assert_eq!(
        started,
        TurnEvent::SpeechStarted {
            audio_start_ms: 1300,
            onset_ms: 1600
        }
    );
    sv.hold_from(None);
    let ev = drive(&mut sv, 80, &[U; 16]);
    let TurnEvent::SpeechStopped { segment, .. } = &ev[0].1 else {
        panic!("{ev:?}")
    };
    assert_eq!(segment.start_sample, 50 * F - 7200);

    // Without the hold the start clamps to what was retained.
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[U; 80]);
    let TurnEvent::SpeechStarted { audio_start_ms, .. } = sv.begin_at(50 * F) else {
        unreachable!()
    };
    assert_eq!(audio_start_ms, (80 * F - 7200) / 24);
    // In a turn a later onset never moves the start later.
    assert_eq!(
        sv.begin_at(79 * F),
        TurnEvent::SpeechStarted {
            audio_start_ms,
            onset_ms: 79 * 32
        }
    );
}

#[test]
fn reset_drops_audio_and_keeps_the_timeline() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[V; 10]);
    assert!(sv.in_speech());
    sv.reset();
    assert!(!sv.in_speech());
    assert_eq!(sv.retained(), 0);
    let ev = drive(&mut sv, 10, &frames(&[(V, 3), (U, 16)]));
    let TurnEvent::SpeechStopped { segment, .. } = &ev[1].1 else {
        panic!("{ev:?}")
    };
    assert_eq!(segment.start_sample, 10 * F, "not before the clear");
    assert_eq!(segment.samples.len() as u64, 19 * F);
}

#[test]
fn threshold_is_inclusive_and_validated() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    assert_eq!(drive(&mut sv, 0, &[0.5, 0.5, 0.5]).len(), 1);
    assert_eq!(
        drive(&mut sv, 3, &[f32::NAN; 16]).len(),
        1,
        "NaN is unvoiced"
    );
    for t in [f32::NAN, 1.5, -0.1] {
        let bad = ServerVadParams {
            threshold: t,
            ..params(500)
        };
        assert!(matches!(
            ServerVad::new(&bad),
            Err(ParamError::Threshold(_))
        ));
    }
    let zero = ServerVadParams {
        sample_rate: 0,
        ..params(500)
    };
    assert_eq!(ServerVad::new(&zero).unwrap_err(), ParamError::ZeroRate);
    // Frames without audio, absurd spans and a reversed frame: no panic.
    let mut bare = ServerVad::new(&params(500)).unwrap();
    bare.push_frame(V, 0, u64::MAX / 2);
    assert!(bare.in_speech());
    let ev = bare.push_frame(U, u64::MAX / 2, u64::MAX - 1);
    assert!(
        matches!(&ev, Some(TurnEvent::SpeechStopped { segment, .. }) if segment.samples.is_empty())
    );
    // A reversed frame is ignored, not a panic.
    assert!(sv.push_frame(V, 10, 5).is_none());
}

#[test]
fn new_params_apply_in_place_and_keep_the_turn() {
    let mut sv = ServerVad::new(&params(500)).unwrap();
    drive(&mut sv, 0, &[V; 5]);
    drive(&mut sv, 5, &[U; 10]);
    // Mid-turn the window grows to 800 ms (25 frames): the 10 silent frames
    // so far still count, so 15 more end it — and not 6.
    sv.set_params(&params(800)).unwrap();
    assert!(sv.in_speech());
    assert!(drive(&mut sv, 15, &[U; 14]).is_empty());
    let ev = drive(&mut sv, 29, &[U; 1]);
    let TurnEvent::SpeechStopped { segment, .. } = &ev[0].1 else {
        panic!("{ev:?}")
    };
    // The turn's start survived the change.
    assert_eq!(segment.start_sample, 0);
    assert!(matches!(
        sv.set_params(&ServerVadParams {
            threshold: 2.0,
            ..params(500)
        }),
        Err(ParamError::Threshold(_))
    ));
}
