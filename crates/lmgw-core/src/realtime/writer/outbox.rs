//! The session core's outbox: what it decided to say, until it flushes to
//! the writer (realtime design §4.1, §4.3, §8.2).

use std::time::Duration;

use bytes::Bytes;

use super::super::protocol::{PartRef, ServerEvent};
use super::{Data, WriterHandle};

/// The session core's outgoing events for the event it is handling.
///
/// The core is the only writer of session state, and its handlers stay
/// synchronous — a whole response's closing sequence is decided in one go —
/// so they queue here, and the core [`flush`](Self::flush)es before it
/// takes the next frame or responder message. That flush is where the data
/// lane's flow control reaches the reader (the writer's module doc).
#[derive(Default)]
pub(crate) struct Outbox(Vec<Data>);

impl Outbox {
    pub fn send(&mut self, event: ServerEvent) {
        self.0.push(Data::Event { purge: None, event });
    }

    /// Output a cancel of `generation` takes back if it has not left — a
    /// speaking response's transcript deltas — and which is paced with its
    /// audio (the writer's module doc).
    pub fn send_purgeable(&mut self, generation: u64, event: ServerEvent) {
        self.0.push(Data::Event {
            purge: Some(generation),
            event,
        });
    }

    /// Audio of `generation`'s item `at`: PCM16-LE, paced, and taken back by
    /// a cancel if it has not left (the writer's module doc).
    pub fn send_audio(&mut self, generation: u64, at: PartRef, pcm: Bytes) {
        self.0.push(Data::Audio {
            gen: generation,
            at,
            pcm,
        });
    }

    /// `generation`'s audio runs `lead` ahead of real time (§8.2): queued
    /// before its first audio.
    pub fn pace(&mut self, generation: u64, lead: Duration) {
        self.0.push(Data::Pace {
            gen: generation,
            lead,
        });
    }

    /// Ask for `generation`'s drained acknowledgement once everything queued
    /// so far has left (the writer's module doc).
    pub fn drained(&mut self, generation: u64) {
        self.0.push(Data::Drained(generation));
    }

    /// Hand everything queued to the writer, in order, waiting for room.
    pub async fn flush(&mut self, out: &WriterHandle) {
        for d in std::mem::take(&mut self.0) {
            let _ = out.data.send(d).await;
        }
    }
}
