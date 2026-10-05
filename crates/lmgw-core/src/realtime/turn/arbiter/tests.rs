//! The arbiter with the real detector and gate, on synthetic probabilities
//! (realtime design §6.4, §6.5): 32 ms frames of 768 samples at 24 kHz.

use super::*;
use crate::realtime::turn::server_vad::ServerVadParams;

/// The knobs these tests' frame counts were written for: 300 ms of
/// evidence, the default before E7 lowered it to 200.
fn at_300() -> BargeInParams {
    BargeInParams {
        min_ms: 300,
        ..BargeInParams::default()
    }
}

/// One Silero frame on the 24 kHz timeline.
const F: u64 = 768;

fn vad() -> ServerVad {
    ServerVad::new(&ServerVadParams::default()).unwrap()
}

fn arbiter() -> Arbiter {
    Arbiter::new(at_300(), false, 24_000)
}

/// The window from playback start `started_ms` on (the guard is 500 ms).
fn inside(started_ms: u64) -> Context {
    Context {
        window: Some(WindowFrame {
            window: started_ms,
            started_ms,
        }),
        ..Context::default()
    }
}

/// Feed `pattern` (voiced, frames) from frame `first`, each frame's context
/// from `cx(frame index)`; the outcomes that said something, by frame.
fn feed(
    a: &mut Arbiter,
    v: &mut ServerVad,
    first: u64,
    pattern: &[(bool, u64)],
    cx: impl Fn(u64) -> Context,
) -> Vec<(u64, Outcome)> {
    let mut out = Vec::new();
    let mut k = first;
    for &(voiced, n) in pattern {
        for _ in 0..n {
            v.push_audio(&[0; F as usize]);
            let o = a.frame(
                v,
                if voiced { 0.9 } else { 0.05 },
                k * F,
                (k + 1) * F,
                cx(k),
            );
            if o != Outcome::default() {
                out.push((k, o));
            }
            k += 1;
        }
    }
    out
}

fn started(o: &Outcome) -> (u64, u64, bool) {
    match o.judged.as_ref().map(|j| (&j.event, j.barge_in)) {
        Some((
            TurnEvent::SpeechStarted {
                audio_start_ms,
                onset_ms,
            },
            b,
        )) => (*audio_start_ms, *onset_ms, b),
        other => panic!("expected speech_started, got {other:?}"),
    }
}

#[test]
fn a_backchannel_in_the_window_is_no_turn_and_its_audio_goes() {
    let (mut a, mut v) = (arbiter(), vad());
    // Past the guard, four voiced frames (128 ms — a "mhm"), then quiet:
    // the gate's gap resets the evidence, which is reported, once.
    let out = feed(
        &mut a,
        &mut v,
        0,
        &[(false, 20), (true, 4), (false, 40)],
        |_| inside(0),
    );
    assert_eq!(out.len(), 1, "{out:?}");
    let (k, o) = &out[0];
    assert!(o.judged.is_none());
    assert_eq!(
        o.dropped,
        Some(Dropped {
            why: DropReason::Backchannel,
            evidence_ms: 128,
            onset_ms: 640,
        })
    );
    // 800 ms of quiet after the burst: 25 frames.
    assert_eq!(*k, 24 + 25 - 1);
    // Nothing was announced, and the held audio was given back: the ring is
    // the pre-roll again.
    assert!(!v.in_speech());
    assert!(v.retained() as u64 <= 7200 + F, "{}", v.retained());
}

#[test]
fn evidence_after_the_guard_starts_a_back_dated_turn_that_ends_on_the_post_interrupt_window() {
    let (mut a, mut v) = (arbiter(), vad());
    let out = feed(&mut a, &mut v, 0, &[(false, 20), (true, 12)], |_| inside(0));
    assert_eq!(out.len(), 1, "{out:?}");
    // The tenth voiced frame (320 ms ≥ 300) fires, back-dated to the first
    // (640 ms), padded by the 300 ms pre-roll the hold kept.
    assert_eq!(out[0].0, 29);
    assert_eq!(started(&out[0].1), (340, 640, true));
    assert!(v.in_speech());
    assert_eq!(v.turn_start(), Some(340 * 24));
    // The turn is the detector's now, and ends on 1500 ms of silence, not
    // 500 (§6.5): 47 frames, not 16.
    let out = feed(&mut a, &mut v, 32, &[(false, 60)], |_| inside(0));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 32 + 47 - 1);
    assert!(matches!(
        out[0].1.judged.as_ref().map(|j| &j.event),
        Some(TurnEvent::SpeechStopped { .. })
    ));
}

#[test]
fn nothing_counts_in_the_guard_and_the_guard_is_the_window_s() {
    let (mut a, mut v) = (arbiter(), vad());
    // Voice from the window's start (echo of the first syllables): only
    // what comes after 500 ms counts, so the trigger is 10 frames past it.
    let out = feed(&mut a, &mut v, 100, &[(true, 40)], |_| inside(3200));
    assert_eq!(out.len(), 1, "{out:?}");
    // 3200 + 500 = 3700 ms; frame 116 starts at 3712.
    assert_eq!(out[0].0, 125);
    assert_eq!(started(&out[0].1).1, 3712);
}

#[test]
fn a_wall_clock_gap_resets_the_evidence() {
    let (mut a, mut v) = (arbiter(), vad());
    feed(&mut a, &mut v, 0, &[(false, 20), (true, 6)], |_| inside(0));
    // The client muted for a second: the next frames come with a gap.
    let out = feed(&mut a, &mut v, 26, &[(true, 6)], |k| Context {
        wall_gap: k == 26,
        ..inside(0)
    });
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].1.dropped.map(|d| d.evidence_ms), Some(192));
    assert!(out[0].1.judged.is_none(), "6 + 6 frames are no barge-in");
    // Four more make ten since the gap.
    let out = feed(&mut a, &mut v, 32, &[(true, 4)], |_| inside(0));
    assert_eq!(started(&out[0].1).1, 26 * 32);
}

#[test]
fn outside_the_window_the_normal_onset_decides() {
    let (mut a, mut v) = (arbiter(), vad());
    let out = feed(&mut a, &mut v, 0, &[(false, 20), (true, 3)], |_| {
        Context::default()
    });
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 22, "96 ms of voice");
    assert_eq!(started(&out[0].1), (340, 640, false));
    // Not responding: the plain silence window ends it.
    let out = feed(&mut a, &mut v, 23, &[(false, 20)], |_| Context::default());
    assert_eq!(out[0].0, 23 + 16 - 1);
    // A turn that starts while a response the client has heard is in
    // progress interrupts it, and gets the longer window.
    let (mut a, mut v) = (arbiter(), vad());
    let busy = |_| Context {
        heard: true,
        ..Context::default()
    };
    feed(&mut a, &mut v, 0, &[(false, 20), (true, 3)], busy);
    let out = feed(&mut a, &mut v, 23, &[(false, 60)], busy);
    assert_eq!(out[0].0, 23 + 47 - 1);
}

#[test]
fn evidence_that_leaves_the_window_carries_its_onset_to_the_next_turn() {
    let (mut a, mut v) = (arbiter(), vad());
    // The answer ends at frame 25 while the user is 5 frames in.
    let window = |k: u64| {
        if k < 25 {
            inside(0)
        } else {
            Context::default()
        }
    };
    let out = feed(&mut a, &mut v, 0, &[(false, 20), (true, 10)], window);
    assert_eq!(out.len(), 1, "{out:?}");
    // The voice before the window's end counts towards the normal onset
    // (E1): the first voiced frame after it starts the turn — at the gate's
    // onset, not at its own.
    assert_eq!(out[0].0, 25);
    assert_eq!(started(&out[0].1), (340, 640, false));
    // The answer had played out: a reply, on the plain silence window
    // (B3 review, E1), not the post-interrupt one.
    let out = feed(&mut a, &mut v, 30, &[(false, 60)], window);
    assert_eq!(out[0].0, 30 + 16 - 1);

    // A short reply straddling the end — 64 ms of voice before it, 32 after,
    // never a normal onset of its own: a turn (E1).
    let (mut a, mut v) = (arbiter(), vad());
    let out = feed(
        &mut a,
        &mut v,
        0,
        &[(false, 23), (true, 3), (false, 20)],
        window,
    );
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].0, 25);
    assert_eq!(started(&out[0].1).1, 23 * 32, "back-dated to the onset");
    assert!(matches!(
        out[1].1.judged.as_ref().map(|j| &j.event),
        Some(TurnEvent::SpeechStopped { .. })
    ));

    // With no onset within the gap (800 ms) the carry is dropped, and said.
    let (mut a, mut v) = (arbiter(), vad());
    let out = feed(
        &mut a,
        &mut v,
        0,
        &[(false, 20), (true, 5), (false, 40)],
        window,
    );
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(
        out[0].1.dropped.map(|d| (d.evidence_ms, d.onset_ms)),
        Some((160, 640))
    );
    assert!(v.retained() as u64 <= 7200 + F);
}

#[test]
fn an_open_turn_is_the_detector_s_and_the_gate_listens_again_after_it() {
    let (mut a, mut v) = (arbiter(), vad());
    // The user was talking before the window opened (interrupt_response
    // off, say): the turn goes on through the window and ends normally.
    let window = |k: u64| {
        if k < 10 {
            Context::default()
        } else {
            inside(320)
        }
    };
    let out = feed(&mut a, &mut v, 0, &[(true, 30), (false, 16)], window);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(!out[0].1.judged.as_ref().unwrap().barge_in);
    // A second utterance in the same window: the gate's, with no lock-out
    // from the first and no guard (the window began long before).
    let out = feed(&mut a, &mut v, 46, &[(true, 12)], window);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 46 + 9);
    assert!(started(&out[0].1).2, "the gate's");
}

#[test]
fn a_barge_in_turn_that_ends_in_the_window_leaves_the_gate_live_for_the_next() {
    let (mut a, mut v) = (arbiter(), vad());
    let out = feed(
        &mut a,
        &mut v,
        0,
        &[(false, 20), (true, 10), (false, 48), (true, 10)],
        |_| inside(0),
    );
    let kinds: Vec<(u64, bool)> = out
        .iter()
        .map(|(k, o)| {
            let j = o.judged.as_ref().unwrap();
            (*k, matches!(j.event, TurnEvent::SpeechStarted { .. }))
        })
        .collect();
    // Started (gate), stopped after 1500 ms, started (gate) again.
    assert_eq!(kinds, [(29, true), (29 + 47, false), (78 + 9, true)]);
}

#[test]
fn half_duplex_hears_nothing_in_the_window() {
    let mut a = Arbiter::new(at_300(), true, 24_000);
    let mut v = vad();
    // Long, loud speech inside the window: no turn, said once.
    let out = feed(&mut a, &mut v, 0, &[(false, 20), (true, 40)], |_| inside(0));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(
        out[0].1.dropped.map(|d| d.why),
        Some(DropReason::HalfDuplex)
    );
    assert!(!v.in_speech());
    // After the window, speech is a turn again.
    let out = feed(&mut a, &mut v, 60, &[(true, 3)], |_| Context::default());
    assert_eq!(out.len(), 1);
    assert!(v.in_speech());
    // A turn open when the window begins ends at the frame before it (B3
    // review 5): what the microphone hears now would be the answer's echo.
    let out = feed(&mut a, &mut v, 63, &[(true, 20)], |_| inside(2000));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 63);
    assert_eq!(
        out[0].1.dropped.map(|d| d.why),
        Some(DropReason::HalfDuplex)
    );
    match out[0].1.judged.as_ref().map(|j| &j.event) {
        Some(TurnEvent::SpeechStopped {
            audio_end_ms,
            segment,
        }) => {
            assert_eq!(*audio_end_ms, 63 * 32, "the last frame before it");
            assert_eq!(segment.start_sample + segment.samples.len() as u64, 63 * F);
        }
        other => panic!("{other:?}"),
    }
    assert!(!v.in_speech());
}
