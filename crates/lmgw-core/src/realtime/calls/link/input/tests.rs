//! The reader's input (calls design §4): real Opus from the audio parts'
//! fixtures, decoded onto the timeline, and the commit window holding the
//! events that cut it.

use super::*;
use crate::realtime::calls::audio::fixtures::{opus_stream, voice};
use crate::realtime::calls::audio::{FRAME_SAMPLES, FRAME_TICKS};

const MS: fn(u64) -> Duration = Duration::from_millis;

fn input() -> Input {
    Input::new(
        "rtc_test".into(),
        InputParams {
            reorder: MS(60),
            commit_window: MS(120),
        },
    )
}

/// The phone's frame `k`: it arrives `30 + 20 (k + 1)` ms after `t0`, its
/// stream steady, so each frame's stamp is its arrival (`audio::stamp`).
fn frame(stream: &[Vec<u8>], k: usize, t0: Instant) -> Frame {
    Frame {
        rtp_time: 1_000 + k as u64 * FRAME_TICKS,
        arrived: arrival(k, t0),
        contiguous: true,
        seq: (10 + k as u64, 10 + k as u64),
        payload: stream[k].clone().into(),
    }
}

fn arrival(k: usize, t0: Instant) -> Instant {
    t0 + MS(30 + 20 * (k as u64 + 1))
}

fn event(kind: &str) -> Incoming {
    Incoming::Event(format!("{{\"type\":\"{kind}\"}}").into())
}

/// What passed, in order: `audio@<ms since t0>` or the event's `type`.
fn labels(passes: &[Pass], t0: Instant) -> Vec<String> {
    passes
        .iter()
        .map(|p| match &p.what {
            Incoming::Audio { pcm, captured } => {
                assert_eq!(pcm.len(), FRAME_SAMPLES);
                format!("audio@{}", captured.duration_since(t0).as_millis())
            }
            Incoming::Event(t) => {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                v["type"].as_str().unwrap().to_string()
            }
            Incoming::Binary => "binary".into(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_commit_waits_for_the_audio_up_to_its_window_and_the_events_behind_it_wait_with_it() {
    let t0 = Instant::now();
    let stream = opus_stream(&voice(20));
    let mut input = input();
    for k in 0..5 {
        let out = input.frame(&frame(&stream, k, t0));
        assert_eq!(labels(&out, t0), [format!("audio@{}", 50 + 20 * k)]);
    }
    // The commit arrives with frame 4, at 130 ms: it cuts at 250 ms.
    let at = t0 + MS(130);
    assert!(input
        .event(event("input_audio_buffer.commit"), at, 10)
        .is_empty());
    assert!(
        input.event(event("response.cancel"), at, 10).is_empty(),
        "behind a waiting commit"
    );
    assert_eq!(input.deadline(), Some(at + MS(120 + 60)));
    // Frames 5 to 10, stamped 150 to 250 ms: before the cut.
    for k in 5..=10 {
        assert_eq!(input.frame(&frame(&stream, k, t0)).len(), 1);
    }
    // Frame 11, stamped past the cut, releases both ahead of itself.
    let out = input.frame(&frame(&stream, 11, t0));
    assert_eq!(
        labels(&out, t0),
        ["input_audio_buffer.commit", "response.cancel", "audio@270"]
    );
    assert_eq!(input.deadline(), None);
    // Anything else goes at once.
    assert_eq!(
        labels(&input.event(event("response.create"), t0, 10), t0),
        ["response.create"]
    );
}

#[test]
fn a_clear_cuts_at_its_own_stamp_and_a_wait_with_no_audio_ends_at_its_deadline() {
    let t0 = Instant::now();
    let stream = opus_stream(&voice(20));
    let mut input = input();
    input.frame(&frame(&stream, 0, t0));
    // A clear at 60 ms waits only for a frame stamped after it.
    let out = input.event(event("input_audio_buffer.clear"), t0 + MS(60), 10);
    assert!(out.is_empty());
    let out = input.frame(&frame(&stream, 1, t0));
    assert_eq!(labels(&out, t0), ["input_audio_buffer.clear", "audio@70"]);
    // A commit, and no frame after it: the deadline (window plus reorder)
    // applies it, with the binary message behind it.
    let at = t0 + MS(80);
    assert!(input
        .event(event("input_audio_buffer.commit"), at, 10)
        .is_empty());
    assert!(input.event(Incoming::Binary, at, 3).is_empty());
    assert!(input.due(at + MS(179)).is_empty());
    assert_eq!(
        labels(&input.due(at + MS(180)), t0),
        ["input_audio_buffer.commit", "binary"]
    );
    // The End takes what waits at once.
    input.event(event("input_audio_buffer.commit"), at + MS(200), 10);
    assert_eq!(labels(&input.flush(), t0), ["input_audio_buffer.commit"]);
}

#[test]
fn a_gap_is_filled_and_what_libopus_cannot_read_is_lost() {
    let t0 = Instant::now();
    let stream = opus_stream(&voice(20));
    let mut input = input();
    input.frame(&frame(&stream, 0, t0));
    // Frame 1 lost: frame 2 brings its frame back from FEC, so 40 ms come
    // out with it, stamped at frame 2's end.
    let mut f = frame(&stream, 2, t0);
    f.contiguous = false;
    let out = input.frame(&f);
    let [Pass {
        what: Incoming::Audio { pcm, captured },
        ..
    }] = out.as_slice()
    else {
        panic!("one audio pass")
    };
    assert_eq!(pcm.len(), 2 * FRAME_SAMPLES);
    assert_eq!(*captured, arrival(2, t0));
    // Not Opus: nothing comes out, and the next frame's gap covers it.
    let mut junk = frame(&stream, 3, t0);
    junk.payload = vec![0xff; 3].into();
    assert!(input.frame(&junk).is_empty());
    // A frame behind the timeline: dropped.
    let mut late = frame(&stream, 1, t0);
    late.arrived = arrival(4, t0);
    assert!(input.frame(&late).is_empty());
}

#[test]
fn only_a_commit_and_a_clear_cut() {
    assert_eq!(
        cut_of(r#"{"type":"input_audio_buffer.commit"}"#),
        Cut::Commit
    );
    assert_eq!(
        cut_of(r#"{"event_id":"e","type":"input_audio_buffer.clear","x":[1,2]}"#),
        Cut::Clear
    );
    assert_eq!(cut_of(r#"{"type":"response.cancel"}"#), Cut::Other);
    assert_eq!(cut_of("not json"), Cut::Other);
    assert_eq!(cut_of(r#"{"type":7}"#), Cut::Other);
}
