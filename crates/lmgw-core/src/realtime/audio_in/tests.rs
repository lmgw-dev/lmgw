//! The input buffer's work and memory (realtime design §4.1, §10.4; WP2
//! review R2, R3): a large append is processed in slices with the task
//! yielding between them, the buffers on its way stay the size of a slice,
//! and a detector that is down keeps no audio.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::realtime::audio::pcm::f32_to_pcm16;
use crate::realtime::input::Barge;
use crate::realtime::test_fixtures::wav_24k;
use crate::realtime::turn::barge_in::BargeInParams;

/// The knobs these tests' frame counts were written for: 300 ms of
/// evidence, the default before E7 lowered it to 200.
fn at_300() -> BargeInParams {
    BargeInParams {
        min_ms: 300,
        ..BargeInParams::default()
    }
}

/// The gate's knobs, duration-only (the word check has its own tests).
fn barge(params: BargeInParams) -> Barge {
    Barge {
        params,
        half_duplex: false,
        words: false,
    }
}

/// `seconds` of low noise — something Silero scores, never a turn.
fn noise(seconds: usize) -> Vec<i16> {
    let mut x: u32 = 12345;
    (0..seconds * INPUT_RATE as usize)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((x >> 16) % 64) as i16 - 32
        })
        .collect()
}

/// One append, arriving now, with nothing playing.
async fn feed(a: &mut AudioIn, pcm: &[i16]) -> Appended {
    a.append(pcm, tokio::time::Instant::now(), &Listen::default())
        .await
}

/// Everything the detector half has allocated, in samples.
fn footprint(a: &AudioIn) -> usize {
    let dsp = a.dsp.as_ref().map_or(0, |d| d.scratch.capacity());
    a.detector.capacity() + dsp + a.manual.capacity()
}

#[tokio::test]
async fn a_large_append_is_taken_in_slices_and_the_task_yields_between_them() {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    // Load the model first, so the append below awaits nothing but its own
    // yields.
    feed(&mut a, &noise(1)).await;
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = {
        let ticks = ticks.clone();
        tokio::spawn(async move {
            loop {
                ticks.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        })
    };
    // The test runtime has one thread: the ticker runs only when the append
    // gives the thread up.
    let big = noise(20);
    let slices = big.len().div_ceil(SLICE);
    ticks.store(0, Ordering::SeqCst);
    let appended = feed(&mut a, &big).await;
    let ran = ticks.load(Ordering::SeqCst);
    ticker.abort();
    assert!(appended.down.is_none());
    assert!(appended.events.is_empty(), "noise is no turn");
    assert!(
        ran >= slices / 2,
        "the ticker ran {ran} times during {slices} slices: the append did not yield"
    );
    assert_eq!(a.received, (INPUT_RATE as u64) * 21);
}

#[tokio::test]
async fn buffers_track_the_slice_not_the_append_and_a_long_turn_is_given_back() {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    // 60 s in one message: nothing on the way keeps its size.
    feed(&mut a, &noise(60)).await;
    assert!(
        footprint(&a) <= 4 * SLICE,
        "{} samples allocated after a 60 s append",
        footprint(&a)
    );

    // A turn holds its audio until it ends — legitimately — and gives the
    // room back once committed.
    let speech = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    let mut long = Vec::new();
    for _ in 0..8 {
        long.extend_from_slice(&speech[..speech.len() * 3 / 4]);
    }
    let started = feed(&mut a, &long).await;
    assert!(a.detector.in_speech() || !started.events.is_empty());
    assert!(a.speech_seen.is_some(), "the turn's speech was heard");
    let committed = a.commit();
    assert!(committed.is_some_and(|c| c.len() > 10 * SLICE));
    assert!(
        footprint(&a) <= 4 * SLICE,
        "{} samples kept after the commit",
        footprint(&a)
    );

    // Manual mode: the uncommitted buffer is what the client sent, and a
    // clear gives it back.
    a.configure(None).unwrap();
    feed(&mut a, &noise(30)).await;
    assert_eq!(a.manual.len(), 30 * INPUT_RATE as usize);
    assert!(a.detector.capacity() <= 4 * SLICE);
    a.clear();
    assert_eq!(a.manual.capacity(), 0);
}

#[tokio::test]
async fn a_detector_that_is_down_keeps_no_audio_and_a_switch_starts_it_again() {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    feed(&mut a, &noise(1)).await;
    // As if Silero had failed on the last frame.
    a.down = true;
    let appended = feed(&mut a, &noise(30)).await;
    assert!(appended.events.is_empty() && appended.down.is_none());
    assert_eq!(a.detector.retained(), 0, "the timeline only");
    assert!(a.detector.capacity() <= 4 * SLICE);
    assert_eq!(a.received, 31 * INPUT_RATE as u64);

    // Manual and back: detection starts afresh.
    a.configure(None).unwrap();
    a.configure(Some(&ServerVadParams::default())).unwrap();
    assert!(!a.down);
}

#[tokio::test]
async fn a_detector_that_goes_down_mid_turn_drops_the_turn_at_once_and_says_where() {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    let speech = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    feed(&mut a, &speech[..speech.len() / 2]).await;
    assert!(a.detector.in_speech(), "a turn is open");
    // As if Silero had failed on the append's last frame (package A review
    // #5): the turn goes now, not with the next append.
    let down = a.fail("Silero failed".into());
    assert_eq!(down.at_ms, a.received * 1000 / INPUT_RATE as u64);
    assert!(!a.detector.in_speech());
    assert_eq!(a.detector.retained(), 0);
    assert!(a.speech_seen.is_none());
    assert!(a.commit().is_none(), "nothing left to commit");
}

/// Stream `pcm` as a real-time client would — 100 ms appends, each
/// arriving as its last sample was recorded, from `t0` — with `listen` for
/// every append; the turn events, with the index of the append that
/// completed each.
async fn real_time(
    a: &mut AudioIn,
    pcm: &[i16],
    t0: tokio::time::Instant,
    listen: &Listen,
) -> Vec<(usize, Detected)> {
    let mut out = Vec::new();
    for (k, chunk) in pcm.chunks(2400).enumerate() {
        let arrived = t0 + std::time::Duration::from_millis(100 * (k as u64 + 1));
        let appended = a.append(chunk, arrived, listen).await;
        out.extend(appended.events.into_iter().map(|d| (k, d)));
    }
    out
}

fn start_of(d: &Detected) -> (u64, u64) {
    match d.event {
        TurnEvent::SpeechStarted {
            audio_start_ms,
            onset_ms,
        } => (audio_start_ms, onset_ms),
        _ => panic!("not a start"),
    }
}

#[tokio::test]
async fn speech_while_the_client_plays_must_earn_its_turn_and_starts_where_it_began() {
    let mut speech = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    speech.extend(vec![0i16; 24 * 1500]);
    let t0 = tokio::time::Instant::now();
    // Outside any window: the normal onset.
    let mut plain = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    let normal = real_time(&mut plain, &speech, t0, &Listen::default()).await;
    let (k_normal, first) = &normal[0];
    assert!(!first.barge_in);
    // The client plays from t0 on, the answer still being produced: the gate
    // decides (no guard here, 300 ms of evidence).
    let mut gated = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    gated.set_barge_in(
        &barge(BargeInParams {
            guard_ms: 0,
            ..at_300()
        }),
        true,
    );
    let playing = Listen {
        view: Some(PlayView {
            gen: 1,
            first: t0,
            end: None,
        }),
        heard: true,
        cuts: None,
    };
    let barge = real_time(&mut gated, &speech, t0, &playing).await;
    let (k_barge, started) = &barge[0];
    assert!(started.barge_in);
    // Later — 300 ms of voice rather than 96 — and back-dated to the same
    // onset, give or take a frame: the hold kept the pre-roll.
    assert!(k_barge > k_normal, "{k_barge} vs {k_normal}");
    let (a_norm, o_norm) = start_of(first);
    let (a_barge, o_barge) = start_of(started);
    assert!(o_barge.abs_diff(o_norm) <= 32, "{o_barge} vs {o_norm}");
    assert!(a_barge.abs_diff(a_norm) <= 32, "{a_barge} vs {a_norm}");
    assert_eq!(a_barge, o_barge.saturating_sub(300));
    // Its capture instant is on the wall clock of the stream: the deciding
    // frame was recorded ~300 ms of voice after the onset.
    let at_ms = started.at.duration_since(t0).as_millis() as u64;
    assert!(
        (o_barge + 280..=o_barge + 400).contains(&at_ms),
        "{at_ms} vs onset {o_barge}"
    );
    // Both turns end — the barge-in's on the post-interrupt window (§6.5):
    // 1500 ms of silence after the speech (which ends at 1738 ms), not 500.
    let end_of = |d: &Detected| match d.event {
        TurnEvent::SpeechStopped { audio_end_ms, .. } => audio_end_ms,
        _ => panic!("not an end"),
    };
    assert_eq!((normal.len(), barge.len()), (2, 2));
    let (plain_end, barge_end) = (end_of(&normal[1].1), end_of(&barge[1].1));
    assert!((2200..2400).contains(&plain_end), "{plain_end}");
    assert!((3200..3400).contains(&barge_end), "{barge_end}");
}

#[tokio::test]
async fn a_backchannel_during_playback_is_no_turn_and_after_the_window_it_would_be() {
    let speech = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    // 150 ms of the question's start: a "mhm" while the answer plays.
    let mut pcm = vec![0i16; 24 * 600];
    pcm.extend_from_slice(&speech[300 * 24..450 * 24]);
    pcm.extend(vec![0i16; 24 * 1500]);
    let t0 = tokio::time::Instant::now();
    let playing = Listen {
        view: Some(PlayView {
            gen: 1,
            first: t0,
            end: Some(t0 + std::time::Duration::from_secs(10)),
        }),
        heard: true,
        cuts: None,
    };
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    a.set_barge_in(&barge(at_300()), true);
    assert!(real_time(&mut a, &pcm, t0, &playing).await.is_empty());
    assert!(
        a.detector.retained() <= 24 * 300 + 768,
        "the held audio went"
    );
    // The same burst once the window has ended is a normal turn (96 ms).
    let ended = Listen {
        view: Some(PlayView {
            gen: 1,
            first: t0,
            end: Some(t0),
        }),
        heard: false,
        cuts: None,
    };
    let mut b = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    let events = real_time(&mut b, &pcm, t0, &ended).await;
    assert!(
        matches!(events.first(), Some((_, d)) if !d.barge_in),
        "a turn"
    );
}

/// A half-duplex detector, its model loaded by a second of silence that
/// arrived at `at`.
async fn half_duplex(at: tokio::time::Instant) -> AudioIn {
    let mut a = AudioIn::new(Some(&ServerVadParams::default())).unwrap();
    a.set_barge_in(
        &Barge {
            params: BargeInParams::default(),
            half_duplex: true,
            words: false,
        },
        true,
    );
    let warm = a.append(&[0; 24_000], at, &Listen::default()).await;
    assert!(warm.events.is_empty());
    a
}

fn end_ms(d: &Detected) -> u64 {
    match d.event {
        TurnEvent::SpeechStopped { audio_end_ms, .. } => audio_end_ms,
        _ => panic!("not an end"),
    }
}

#[tokio::test]
async fn a_turn_that_cuts_the_answer_is_not_ended_by_that_answer_s_window() {
    // Live run 2, H1: the user's sentence began just before the answer's
    // first audio, and one append held both. The turn started on the frames
    // before the window; the next frame, inside it, ended it under half
    // duplex — before the core, which acts after the whole append, had even
    // cut the answer — and "I've" was committed and answered.
    let clip = f32_to_pcm16(&wav_24k("en_complete_short.wav"));
    let (head, tail) = clip.split_at(24 * 1100);
    let t = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let ms = std::time::Duration::from_millis;
    // The 1.1 s append arrives at `t`: the answer's first audio left at
    // 700 ms of it, after the onset (speech from 300 ms, 96 ms to confirm).
    let playing = |cuts| Listen {
        view: Some(PlayView {
            gen: 3,
            first: t - ms(400),
            end: None,
        }),
        heard: true,
        cuts,
    };

    // A turn that does not cut the answer (interrupt_response off) is the
    // B3 review 5 case: it ends at the frame before the window, so the
    // answer's echo is not transcribed with it.
    let mut kept = half_duplex(t - ms(3000)).await;
    let events = kept.append(head, t, &playing(None)).await.events;
    assert_eq!(events.len(), 2, "started, and ended at the window");
    assert!(matches!(events[0].event, TurnEvent::SpeechStarted { .. }));
    let stop = end_ms(&events[1]);
    assert!((1664..=1700).contains(&stop), "{stop}");

    // The turn that cuts it goes on through that answer's window — before
    // the cut, and after it up to the window's margin.
    let mut a = half_duplex(t - ms(3000)).await;
    let events = a.append(head, t, &playing(Some(3))).await.events;
    assert_eq!(events.len(), 1, "started, not ended");
    assert!(a.detector.in_speech());
    let cut = Listen {
        view: Some(PlayView {
            gen: 3,
            first: t - ms(400),
            end: Some(t + ms(300)),
        }),
        heard: false,
        cuts: None,
    };
    let mut rest = tail.to_vec();
    rest.extend(vec![0i16; 24 * 2000]);
    let len = ms(rest.len() as u64 / 24);
    let events = a.append(&rest, t + len, &cut).await.events;
    assert_eq!(events.len(), 1, "one end, the sentence's");
    // The speech ends at 1738 ms of the clip (1 s of warm-up before it),
    // then the post-interrupt window: the user was talking over an answer.
    let stop = end_ms(&events[0]);
    assert!(stop >= 1000 + 1738 + 1500, "{stop}");
}

mod scores;
