//! The word check's state on synthetic probabilities (realtime design
//! §6.4): 32 ms frames of 768 samples at 24 kHz, the window open from
//! 0 ms with a 500 ms guard, 300 ms of evidence.

use super::super::{Arbiter, Context, Judged, Outcome, WindowFrame};
use super::*;
use crate::realtime::turn::barge_in::BargeInParams;
use crate::realtime::turn::server_vad::ServerVadParams;

/// The knobs these tests' frame counts were written for: 300 ms of
/// evidence, the default before E7 lowered it to 200.
fn at_300() -> BargeInParams {
    BargeInParams {
        min_ms: 300,
        ..BargeInParams::default()
    }
}

const F: u64 = 768;

fn words() -> (Arbiter, ServerVad) {
    let mut a = Arbiter::new(at_300(), false, 24_000);
    a.set_words(true);
    (a, ServerVad::new(&ServerVadParams::default()).unwrap())
}

fn inside() -> Context {
    Context {
        window: Some(WindowFrame {
            window: 1,
            started_ms: 0,
        }),
        ..Context::default()
    }
}

/// Frames `first..` of `pattern`, inside the window; what each said.
fn feed(
    a: &mut Arbiter,
    v: &mut ServerVad,
    first: u64,
    pattern: &[(bool, u64)],
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
                inside(),
            );
            if o != Outcome::default() {
                out.push((k, o));
            }
            k += 1;
        }
    }
    out
}

/// One frame `k` outside the window.
fn outside(a: &mut Arbiter, v: &mut ServerVad, k: u64, voiced: bool) -> Outcome {
    v.push_audio(&[0; F as usize]);
    a.frame(
        v,
        if voiced { 0.9 } else { 0.05 },
        k * F,
        (k + 1) * F,
        Context::default(),
    )
}

fn check(o: &Outcome) -> &CheckRequest {
    o.check.as_ref().expect("a check")
}

#[test]
fn a_gate_turn_waits_for_its_words_and_a_cut_announces_it_back_dated() {
    let (mut a, mut v) = words();
    // Quiet past the guard (16 frames = 512 ms), then speech: the gate's
    // 300 ms come at the tenth voiced frame (k = 25), as without the check —
    // but nothing is announced, and the turn's audio is asked for.
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    assert_eq!(out.len(), 1, "{out:?}");
    let (k, o) = &out[0];
    assert_eq!(*k, 25);
    assert!(o.judged.is_none(), "not announced yet");
    let c = check(o);
    assert!(c.trigger);
    // From 300 ms of pre-roll before the onset (512 ms) to the frame's end.
    assert_eq!(c.samples.len() as u64, 26 * F - 212 * 24);
    assert!(a.checking() && v.in_speech());
    // A second of silence: the turn stays open, kept for its verdict.
    assert!(feed(&mut a, &mut v, 26, &[(false, 32)]).is_empty());
    assert!(v.in_speech());
    // The words cut: the withheld speech_started, back-dated to the onset.
    let done = a.checked(&mut v, c.id, Verdict::Cut);
    match done
        .judged
        .into_iter()
        .next()
        .map(|j| (j.event, j.barge_in))
    {
        Some((
            TurnEvent::SpeechStarted {
                audio_start_ms,
                onset_ms,
            },
            true,
        )) => assert_eq!((audio_start_ms, onset_ms), (212, 512)),
        other => panic!("{other:?}"),
    }
    assert!(!a.checking());
    // Now the post-interrupt window (1500 ms) ends it: the second of
    // silence so far counts.
    let out = feed(&mut a, &mut v, 58, &[(false, 20)]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 26 + 46, "1500 ms after the speech");
}

#[test]
fn a_backchannel_is_checked_once_more_at_its_end_and_then_dropped() {
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let first = check(&out[0].1).clone();
    assert!(a.checked(&mut v, first.id, Verdict::Backchannel) == Checked::default());
    // "Mhm" goes on for 224 ms more — less than another 300 ms of voice —
    // then silence: at the end of its silence window (500 ms) the rest is
    // checked.
    let out = feed(&mut a, &mut v, 26, &[(true, 7), (false, 16)]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 26 + 7 + 15, "the silence window passed");
    let last = check(&out[0].1).clone();
    assert!(!last.trigger && last.samples.len() > first.samples.len());
    // Still nothing but "mhm": dropped, never announced, its audio gone.
    let done = a.checked(&mut v, last.id, Verdict::Backchannel);
    let dropped = done.dropped.expect("dropped");
    assert_eq!(dropped.why, DropReason::Checked);
    assert_eq!((dropped.onset_ms, dropped.evidence_ms), (512, 17 * 32));
    assert!(done.judged.is_empty() && done.check.is_none());
    assert!(!v.in_speech() && !a.checking());
    assert_eq!(v.retained(), 0, "the turn's audio went");
    // The gate listens again in the same window — no guard now.
    let out = feed(&mut a, &mut v, 49, &[(true, 10)]);
    assert!(check(&out[0].1).trigger, "{out:?}");
}

#[test]
fn speech_that_grows_past_its_backchannel_is_checked_again_and_cuts() {
    // "Mhm, aber warte mal": the first 300 ms are only "mhm".
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let first = check(&out[0].1).clone();
    // Its verdict is slow: the speech goes on meanwhile, past another
    // 300 ms of voice — the re-check waits for the verdict.
    assert!(feed(&mut a, &mut v, 26, &[(true, 3), (false, 5), (true, 8)]).is_empty());
    let again = a.checked(&mut v, first.id, Verdict::Backchannel);
    let second = again.check.expect("due, so made at once");
    assert!(second.samples.len() > first.samples.len());
    // A verdict for the first check again changes nothing.
    assert_eq!(
        a.checked(&mut v, first.id, Verdict::Cut),
        Checked::default()
    );
    // "aber warte mal": cut.
    let done = a.checked(&mut v, second.id, Verdict::Cut);
    assert!(matches!(
        done.judged.into_iter().next().map(|j| j.event),
        Some(TurnEvent::SpeechStarted { onset_ms: 512, .. })
    ));
}

#[test]
fn a_new_session_update_keeps_the_turn_being_checked_and_a_reset_drops_it() {
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    a.set_params(
        BargeInParams {
            guard_ms: 100,
            ..at_300()
        },
        false,
    );
    assert!(a.checking(), "the detector still keeps it open");
    assert!(!a.checked(&mut v, c.id, Verdict::Cut).judged.is_empty());
    // A commit or a clear resets both: a late verdict finds nothing.
    let out = feed(&mut a, &mut v, 26, &[(false, 50), (true, 10)]);
    let c = check(&out.last().unwrap().1).clone();
    a.reset();
    v.reset();
    assert_eq!(a.checked(&mut v, c.id, Verdict::Cut), Checked::default());
    assert!(!a.checking());
}

#[test]
fn a_backchannel_after_the_answer_ended_is_a_normal_turn() {
    // E1: "Ja" after "Soll ich das so machen?" — the session finds nothing
    // plays any more, and the turn is announced as a normal one: no
    // barge-in, the plain silence window.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    let done = a.checked(&mut v, c.id, Verdict::Turn);
    assert_eq!(done.judged.len(), 1, "{done:?}");
    assert!(!done.judged[0].barge_in);
    assert!(matches!(
        done.judged[0].event,
        TurnEvent::SpeechStarted { onset_ms: 512, .. }
    ));
    assert!(!a.checking() && v.in_speech());
    // 500 ms of silence end it — not post_interrupt_silence_ms.
    let out = feed(&mut a, &mut v, 26, &[(false, 20)]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, 26 + 15);

    // A reply whose silence window passed while its words were checked ends
    // with its verdict.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    assert!(feed(&mut a, &mut v, 26, &[(false, 20)]).is_empty());
    let done = a.checked(&mut v, c.id, Verdict::Turn);
    let events: Vec<_> = done.judged.iter().map(|j| &j.event).collect();
    assert!(
        matches!(
            events[..],
            [
                TurnEvent::SpeechStarted { .. },
                TurnEvent::SpeechStopped { .. }
            ]
        ),
        "{events:?}"
    );
    assert!(!v.in_speech());
}

#[test]
fn a_turn_still_unconfirmed_past_the_window_s_end_is_promoted() {
    // E1, E2: humming goes on past the answer — at its first frame outside
    // the window it is a normal turn, and no further check is made.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    let o = outside(&mut a, &mut v, 26, true);
    assert_eq!(o.promoted, Some(512));
    let j = o.judged.expect("announced");
    assert!(!j.barge_in);
    assert!(matches!(
        j.event,
        TurnEvent::SpeechStarted { onset_ms: 512, .. }
    ));
    assert!(!a.checking() && !a.awaits(c.id));
    // Its verdict comes late and changes nothing.
    assert_eq!(a.checked(&mut v, c.id, Verdict::Cut), Checked::default());
    // The normal endpointer ends it on the plain window.
    let mut ended = None;
    for k in 27..60 {
        if let Some(j) = outside(&mut a, &mut v, k, false).judged {
            ended = Some((k, j.event));
            break;
        }
    }
    assert!(
        matches!(ended, Some((42, TurnEvent::SpeechStopped { .. }))),
        "{ended:?}"
    );
}

#[test]
fn a_re_check_starts_at_a_pause_near_what_is_new_else_a_pre_roll_before_it() {
    // E2, B4 review M2: the second check would start at the first's end
    // less the 300 ms pre-roll — mid-word, "alles klar" came back as "Les
    // klar." — so it starts at the last pause at or before that. Fix
    // package B6: only a pause within one more pre-roll counts; without
    // one it starts at that point, as E2 had it, so the upload never grows
    // back to the turn's start.
    let (mut a, mut v) = words();
    let pre_roll = v.prefix_samples();
    assert_eq!(pre_roll, 300 * 24);
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let first = check(&out[0].1).clone();
    a.checked(&mut v, first.id, Verdict::Backchannel);
    // Three words more, a pause of three frames after the first (k = 29..31).
    let out = feed(&mut a, &mut v, 26, &[(true, 3), (false, 3), (true, 7)]);
    let second = check(&out[0].1).clone();
    assert_eq!(out[0].0, 38, "another 300 ms of voice");
    // The first check ended at frame 26; the turn had no pause in the
    // 600 ms to 300 ms before that: from 300 ms before it.
    assert_eq!(second.samples.len() as u64, 39 * F - (26 * F - pre_roll));
    a.checked(&mut v, second.id, Verdict::Backchannel);
    let out = feed(&mut a, &mut v, 39, &[(true, 10)]);
    let third = check(&out[0].1).clone();
    // 300 ms before the second's end (39 frames) is frame 29.6: the pause
    // that began at frame 29 is where the third starts — not at 29.6, in
    // the word.
    assert_eq!(third.samples.len() as u64, 49 * F - 29 * F);
    a.checked(&mut v, third.id, Verdict::Backchannel);
    // 300 ms before the third's end is frame 39.6, and the pause's last
    // frame (31) lies within 300 ms more: the fourth starts there, right
    // before the word after it.
    let out = feed(&mut a, &mut v, 49, &[(true, 10)]);
    let fourth = check(&out[0].1).clone();
    assert_eq!(fourth.samples.len() as u64, 59 * F - 31 * F);
    a.checked(&mut v, fourth.id, Verdict::Backchannel);
    // Hum from here on: no pause within reach any more. Each re-check is
    // the new audio and one pre-roll — it no longer reaches back to frame
    // 31, nor to the turn's start.
    for end in [69, 79, 89] {
        let out = feed(&mut a, &mut v, end - 10, &[(true, 10)]);
        let next = check(&out[0].1).clone();
        assert_eq!(next.samples.len() as u64, 10 * F + pre_roll, "to {end}");
        a.checked(&mut v, next.id, Verdict::Backchannel);
    }
}

#[test]
fn a_checked_backchannel_whose_silence_runs_past_the_window_is_dropped() {
    // B4 review M1: "Mhm", judged a backchannel while the answer played;
    // its silence runs on past the window's end. It is no reply: dropped,
    // never announced — it used to be promoted, committed and answered.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    assert!(a.checked(&mut v, c.id, Verdict::Backchannel) == Checked::default());
    assert!(feed(&mut a, &mut v, 26, &[(false, 4)]).is_empty());
    let o = outside(&mut a, &mut v, 30, false);
    assert_eq!(o.judged, None, "{o:?}");
    assert_eq!(o.promoted, None);
    let dropped = o.dropped.expect("dropped");
    assert_eq!((dropped.why, dropped.onset_ms), (DropReason::Checked, 512));
    assert!(!a.checking() && !v.in_speech());
    assert_eq!(v.retained(), 0, "its audio went");
    // What comes next is the detector's, from scratch.
    for k in 31..40 {
        assert_eq!(outside(&mut a, &mut v, k, false), Outcome::default());
    }
}

#[test]
fn past_the_window_a_check_in_flight_decides_and_unchecked_speech_promotes() {
    // A "Ja" whose check is still out when the window ends: silence alone
    // promotes nothing — its verdict does (E1: backchannel words when
    // nothing plays are the user's reply).
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    for k in 26..30 {
        assert_eq!(outside(&mut a, &mut v, k, false), Outcome::default());
    }
    assert!(a.awaits(c.id));
    let done = a.checked(&mut v, c.id, Verdict::Turn);
    assert!(matches!(
        done.judged[..],
        [Judged {
            event: TurnEvent::SpeechStarted { onset_ms: 512, .. },
            barge_in: false
        }]
    ));

    // Speech no check heard, its silence window passed while the last
    // check was out: past the window it is a normal turn — and ends in the
    // same frame, not at the next one.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    assert!(feed(&mut a, &mut v, 26, &[(true, 3), (false, 16)]).is_empty());
    assert!(a.awaits(c.id), "the first check is still out");
    let o = outside(&mut a, &mut v, 45, false);
    assert_eq!(o.promoted, Some(512));
    assert!(matches!(
        o.judged.map(|j| j.event),
        Some(TurnEvent::SpeechStarted { onset_ms: 512, .. })
    ));
    assert!(matches!(
        o.stopped.map(|j| j.event),
        Some(TurnEvent::SpeechStopped { .. })
    ));
    assert!(!v.in_speech() && !a.checking());
}

#[test]
fn a_mute_ends_a_turn_being_checked() {
    // E5: "Mhm", then the client sends nothing for longer than the gate's
    // gap, then speaks again — the new utterance is not glued onto it.
    let (mut a, mut v) = words();
    let out = feed(&mut a, &mut v, 0, &[(false, 16), (true, 10)]);
    let c = check(&out[0].1).clone();
    a.checked(&mut v, c.id, Verdict::Backchannel);
    v.push_audio(&[0; F as usize]);
    let o = a.frame(
        &mut v,
        0.9,
        26 * F,
        27 * F,
        Context {
            wall_gap: true,
            ..inside()
        },
    );
    let dropped = o.dropped.expect("the checked turn went");
    assert_eq!(dropped.why, DropReason::Checked);
    assert!(!a.checking());
    assert!(!v.in_speech(), "nothing glued on: the gate starts afresh");
}
