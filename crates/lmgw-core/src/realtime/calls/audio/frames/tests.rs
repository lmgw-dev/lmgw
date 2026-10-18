use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::realtime::calls::audio::clock::FrameClock;

const F: usize = FRAME_SAMPLES;

/// The paced queue as a test holds it: chunks that are ready, and the
/// response that speaks. With `last` set, taking the final chunk ends the
/// response's audio, as the drained marker behind it does.
#[derive(Default)]
struct Queue {
    chunks: VecDeque<Chunk>,
    speaking: Option<String>,
    last: bool,
}

impl Pull for Queue {
    fn chunk(&mut self) -> Option<Chunk> {
        let c = self.chunks.pop_front();
        if c.is_some() && self.chunks.is_empty() && self.last {
            self.speaking = None;
        }
        c
    }

    fn speaking(&self) -> Option<String> {
        self.speaking.clone()
    }
}

/// `n` samples of `item` of `response`, each sample its index plus `base`,
/// so a frame shows where it was cut.
fn chunk(response: &str, item: &str, n: usize, base: i16) -> Chunk {
    Chunk {
        response_id: response.into(),
        item_id: item.into(),
        pcm: (0..n).map(|i| base.wrapping_add(i as i16)).collect(),
    }
}

fn starved(response: &str, starved: bool) -> Option<OutputAudioStarved> {
    Some(OutputAudioStarved {
        response_id: response.into(),
        starved,
    })
}

#[test]
fn each_tick_pulls_20_ms_across_chunks_however_much_is_queued() {
    // A whole second of audio queued at once leaves at one frame per tick:
    // the clock paces it, not the queue.
    let mut q = Queue {
        speaking: Some("r1".into()),
        ..Queue::default()
    };
    for k in 0..10 {
        q.chunks
            .push_back(chunk("r1", "i1", 2400, (k * 100) as i16));
    }
    let t0 = Instant::now();
    let mut clock = FrameClock::new(t0, 0);
    let mut frames = Frames::default();
    let mut out = Vec::new();
    let now = t0 + Duration::from_millis(100);
    while let Some(slot) = clock.take(now) {
        out.push((slot, frames.frame(&mut q)));
    }
    assert_eq!(out.len(), 6, "slots 0 to 100 ms");
    for (k, (slot, f)) in out.iter().enumerate() {
        assert_eq!(slot.rtp, k as u64 * 960);
        assert_eq!(f.pcm.len(), F);
        assert_eq!((f.padded, f.starved.clone()), (0, None));
        assert_eq!(f.response_id.as_deref(), Some("r1"));
    }
    // Frame 5 is the first of the second chunk; frame 4 the end of the
    // first. One item: its offsets run on across the chunks.
    let (_, f4) = &out[4];
    assert_eq!(f4.pcm[0], 4 * F as i16);
    assert_eq!(f4.spans.len(), 1);
    assert_eq!(
        (f4.spans[0].from, f4.spans[0].to),
        (4 * F as u64, 5 * F as u64)
    );
    let (_, f5) = &out[5];
    assert_eq!(f5.pcm[0], 100);
    assert_eq!((f5.spans[0].from, f5.spans[0].to), (2400, 2400 + F as u64));
    assert_eq!(q.chunks.len(), 8, "only what the ticks took");
    assert_eq!(frames.sent("r1").get("i1"), Some(&(6 * F as u64)));
}

#[test]
fn a_frame_carries_two_items_where_a_clause_ends_inside_it() {
    let mut q = Queue {
        speaking: Some("r1".into()),
        ..Queue::default()
    };
    q.chunks.push_back(chunk("r1", "i1", 300, 0));
    q.chunks.push_back(chunk("r1", "i2", 1000, 1000));
    let mut frames = Frames::default();
    let f = frames.frame(&mut q);
    assert_eq!(f.audio(), F);
    assert_eq!(
        f.spans,
        vec![
            Span {
                item_id: "i1".into(),
                at: 0,
                from: 0,
                to: 300
            },
            Span {
                item_id: "i2".into(),
                at: 300,
                from: 0,
                to: 180
            },
        ]
    );
    assert_eq!((f.pcm[299], f.pcm[300]), (299, 1000));
    let f = frames.frame(&mut q);
    assert_eq!((f.spans[0].from, f.spans[0].to), (180, 180 + F as u64));
}

#[test]
fn an_underrun_pads_and_says_starved_once_and_the_audio_says_it_came_again() {
    let mut q = Queue {
        speaking: Some("r1".into()),
        ..Queue::default()
    };
    q.chunks.push_back(chunk("r1", "i1", 600, 0));
    let mut frames = Frames::default();
    let f = frames.frame(&mut q);
    assert_eq!((f.audio(), f.padded, f.starved), (F, 0, None));
    // The audio runs out inside the frame: the rest is padding, and the
    // frame had audio, so nothing is said yet.
    let f = frames.frame(&mut q);
    assert_eq!((f.audio(), f.padded, f.starved), (120, F - 120, None));
    assert!(f.pcm[120..].iter().all(|&s| s == 0));
    // No audio at all: padding of r1, said once.
    let f = frames.frame(&mut q);
    assert_eq!(f.response_id.as_deref(), Some("r1"));
    assert_eq!(
        (f.audio(), f.padded, f.starved),
        (0, F, starved("r1", true))
    );
    let f = frames.frame(&mut q);
    assert_eq!((f.padded, f.starved), (F, None), "said once");
    // Synthesis catches up.
    q.chunks.push_back(chunk("r1", "i1", 2400, 0));
    let f = frames.frame(&mut q);
    assert_eq!((f.audio(), f.starved), (F, starved("r1", false)));
    // Padding is never the item's audio: 600 + 480 samples were sent.
    assert_eq!(frames.sent("r1").get("i1"), Some(&(600 + F as u64)));
}

#[test]
fn the_last_frame_of_an_answer_ends_in_silence_not_padding() {
    let mut q = Queue {
        speaking: Some("r1".into()),
        last: true,
        ..Queue::default()
    };
    q.chunks.push_back(chunk("r1", "i1", 500, 0));
    let mut frames = Frames::default();
    frames.frame(&mut q);
    let f = frames.frame(&mut q);
    assert_eq!((f.audio(), f.padded, f.starved), (20, 0, None));
    // Between answers: silence, no response, nothing said.
    let f = frames.frame(&mut q);
    assert_eq!(
        (f.response_id, f.spans.len(), f.padded, f.starved),
        (None, 0, 0, None)
    );
    assert!(f.pcm.iter().all(|&s| s == 0));
}

#[test]
fn a_purge_drops_what_is_held_and_answers_what_left_exactly() {
    let mut q = Queue {
        speaking: Some("r1".into()),
        ..Queue::default()
    };
    // The paced queue hands over 100 ms chunks; 30 ms of one have left.
    q.chunks.push_back(chunk("r1", "i1", 2400, 0));
    let mut frames = Frames::default();
    frames.frame(&mut q);
    q.speaking = None;
    assert_eq!(
        frames.purge("r1"),
        HashMap::from([("i1".to_string(), F as u64)])
    );
    let f = frames.frame(&mut q);
    assert_eq!(f.audio(), 0, "nothing of the cut answer goes out after it");
    // The next answer's audio is never mixed into a frame of the last.
    q.speaking = Some("r2".into());
    q.chunks.push_back(chunk("r2", "j1", 100, 0));
    q.chunks.push_back(chunk("r3", "k1", 960, 0));
    let f = frames.frame(&mut q);
    assert_eq!((f.response_id.as_deref(), f.audio()), (Some("r2"), 100));
    let f = frames.frame(&mut q);
    assert_eq!((f.response_id.as_deref(), f.audio()), (Some("r3"), F));
    assert!(
        frames.sent("r2").is_empty(),
        "only the latest answer is kept"
    );
}
