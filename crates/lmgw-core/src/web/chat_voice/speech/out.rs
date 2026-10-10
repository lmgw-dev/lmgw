//! A read-aloud's frames on their way to the page (chat-voice design §6.3,
//! §6.4; WP4 review m6).
//!
//! Synthesis runs far faster than real time and nothing waits for playback
//! (§6.1: no writer, no stall rule), so the frames wait in an unbounded
//! channel for the page's reader — a whole reply's audio when the reader is
//! slow. Bounded by the reply itself, which the no-hidden-caps rule allows;
//! nothing is dropped. Two things keep that honest:
//! - **the audio stays raw until the reader takes it**: a `speech` frame
//!   waits as PCM and is base64-encoded at the SSE edge ([`SpeechRx`]), so
//!   what waits is the audio's size, not four thirds of it;
//! - **a reader that falls behind playback is said** ([`Out::behind`]): a
//!   frame older than the newest still waits for it, and a reader that
//!   plays in real time from the first frame it took would have run out of
//!   what it took by now — or, before it took any, out of the first frame.
//!   The read-aloud logs that once, with how much audio waits, and its end
//!   says the most that ever waited. A reader that keeps up takes each
//!   frame as it comes, and synthesis slower than playback leaves nothing
//!   waiting: neither is said.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine as _;
use bytes::Bytes;
use lmgw_api_types::chat_frames as frames;
use tokio::sync::mpsc;

use crate::realtime::audio::resample::INPUT_RATE;

use super::super::super::chat_turn::TurnFrame;

/// The read-aloud's end, and the reader's.
pub(crate) fn channel() -> (Out, SpeechRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let backlog = Arc::new(Mutex::new(Backlog::default()));
    (
        Out {
            tx,
            backlog: backlog.clone(),
        },
        SpeechRx { rx, backlog },
    )
}

/// A frame as it waits for the reader.
enum Waiting {
    Frame(TurnFrame),
    /// A `speech` frame, its PCM16-LE 24 kHz mono audio still raw.
    Speech {
        seq: u64,
        text: String,
        pcm: Bytes,
    },
}

/// What has been handed to the reader, and what it took.
#[derive(Default)]
struct Backlog {
    /// `speech` frames handed on, and taken by the reader.
    queued: u64,
    taken: u64,
    /// Their audio, in samples.
    queued_samples: u64,
    taken_samples: u64,
    /// When the first was handed on, and its audio.
    first_queued: Option<(Instant, u64)>,
    /// When the reader took its first.
    first_taken: Option<Instant>,
    /// The most audio that waited at once, in samples.
    peak: u64,
}

fn ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(INPUT_RATE)
}

fn lock(b: &Mutex<Backlog>) -> std::sync::MutexGuard<'_, Backlog> {
    b.lock().unwrap_or_else(|e| e.into_inner())
}

/// The read-aloud's end of the channel.
pub(crate) struct Out {
    tx: mpsc::UnboundedSender<Waiting>,
    backlog: Arc<Mutex<Backlog>>,
}

impl Out {
    /// Hand on a frame other than `speech`.
    pub fn frame(&self, f: TurnFrame) {
        let _ = self.tx.send(Waiting::Frame(f));
    }

    /// Hand on a clause's `speech` frame: `pcm` is PCM16-LE.
    pub fn speech(&self, seq: u64, text: String, pcm: Bytes) {
        let samples = (pcm.len() / 2) as u64;
        {
            let mut b = lock(&self.backlog);
            b.queued += 1;
            b.queued_samples += samples;
            b.first_queued.get_or_insert((Instant::now(), samples));
            b.peak = b.peak.max(b.queued_samples - b.taken_samples);
        }
        let _ = self.tx.send(Waiting::Speech { seq, text, pcm });
    }

    /// The reader is gone: the page went away.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Resolves once the reader is gone.
    pub async fn closed(&self) {
        self.tx.closed().await;
    }

    /// The audio waiting for the reader, in milliseconds, when the reader
    /// has fallen behind playback (module doc); `None` while it keeps up.
    pub fn behind(&self) -> Option<u64> {
        let b = lock(&self.backlog);
        if b.queued <= b.taken + 1 {
            return None;
        }
        let (since, budget) = match (b.first_taken, b.first_queued) {
            (Some(at), _) => (at, b.taken_samples),
            (None, Some((at, first))) => (at, first),
            (None, None) => return None,
        };
        let played = since.elapsed().as_millis() as u64;
        (played > ms(budget)).then(|| ms(b.queued_samples - b.taken_samples))
    }

    /// The most audio that waited for the reader at once, and what waits
    /// now, in milliseconds.
    pub fn waited(&self) -> (u64, u64) {
        let b = lock(&self.backlog);
        (ms(b.peak), ms(b.queued_samples - b.taken_samples))
    }
}

/// The reader's end: the page's SSE stream.
pub(crate) struct SpeechRx {
    rx: mpsc::UnboundedReceiver<Waiting>,
    backlog: Arc<Mutex<Backlog>>,
}

impl SpeechRx {
    /// The next frame for the page — a `speech` frame's audio base64 now —
    /// or `None` once the read-aloud is over.
    pub async fn recv(&mut self) -> Option<TurnFrame> {
        Some(match self.rx.recv().await? {
            Waiting::Frame(f) => f,
            Waiting::Speech { seq, text, pcm } => {
                {
                    let mut b = lock(&self.backlog);
                    b.taken += 1;
                    b.taken_samples += (pcm.len() / 2) as u64;
                    b.first_taken.get_or_insert_with(Instant::now);
                }
                let pcm = base64::engine::general_purpose::STANDARD.encode(&pcm);
                TurnFrame::of("speech", &frames::SpeechFrame { seq, text, pcm })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ms` of audio.
    fn audio(ms: u64) -> Bytes {
        Bytes::from(vec![0u8; (ms * 24 * 2) as usize])
    }

    #[tokio::test]
    async fn a_reader_that_keeps_up_is_never_behind() {
        let (out, mut rx) = channel();
        for seq in 0..3 {
            out.speech(seq, "Satz.".into(), audio(1000));
            assert_eq!(out.behind(), None, "only the newest waits");
            let f = rx.recv().await.unwrap();
            assert_eq!(f.event, "speech");
        }
        assert_eq!(out.waited(), (1000, 0));
    }

    #[tokio::test]
    async fn a_reader_that_stopped_taking_is_behind_once_playback_ran_dry() {
        let (out, mut rx) = channel();
        // 40 ms taken, then nothing more.
        out.speech(0, "a".into(), audio(40));
        rx.recv().await.unwrap();
        out.speech(1, "b".into(), audio(1000));
        out.speech(2, "c".into(), audio(1000));
        assert_eq!(out.behind(), None, "it still plays what it took");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(out.behind(), Some(2000), "nothing is dropped");
        // It reads again: what it takes is base64, and it catches up.
        let f = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&f.data).unwrap();
        assert_eq!(v["seq"], 1);
        let pcm = base64::engine::general_purpose::STANDARD
            .decode(v["pcm"].as_str().unwrap())
            .unwrap();
        assert_eq!(pcm.len(), 1000 * 24 * 2);
        rx.recv().await.unwrap();
        assert_eq!(out.waited(), (2000, 0));
    }

    #[tokio::test]
    async fn a_reader_that_never_took_anything_is_behind_after_the_first_frame() {
        let (out, rx) = channel();
        out.speech(0, "a".into(), audio(30));
        out.speech(1, "b".into(), audio(30));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(out.behind(), Some(60));
        drop(rx);
        assert!(out.is_closed());
    }
}
