//! Each tick's 20 ms of output (calls design §5.1).
//!
//! **The paced queue is pulled, not timed**, on a call: each tick of the
//! frame clock (`clock`) takes the next 480 samples of released audio
//! across chunk and clause boundaries, so the queue's audio leaves exactly
//! as fast as the phone plays it. The writer's pacer is not used and the
//! lead is 0; the transcript deltas queued with the audio leave with the
//! tick that reaches them. One clock, not two: a writer releasing on its own
//! timer beside a link sending on another would beat against it and put
//! silence into the middle of a word whenever their phases slipped.
//!
//! **An underrun.** A tick that finds a response speaking but no audio
//! ready — synthesis is late, or a tool runs after the spoken preamble —
//! pads with silence, and so does the rest of a frame its audio ran out in.
//! Padding is never counted as an item's audio ([`OutFrame::spans`] carry
//! only the TTS's samples, so the heard table and `audio_end_ms` count only
//! them); the playback record counts it, since the phone plays it. The
//! first tick that finds no audio at all says the response starved
//! (`lmgw.output_audio.starved {starved: true}`, C22), and the next tick
//! with its audio says it came again (`false`); a bound call sends both, in
//! order with the transcript deltas. A response that ends or is cut while
//! starved owes no `false`: its `response.done` ends it for the phone.
//!
//! **The seam.** [`Pull`] is the writer's paced queue as the frame clock
//! pulls it; the wiring implements it over `writer::paced::Shared`.
//! A chunk is taken whole and cut here, so what left is counted by the
//! frame, exactly ([`Frames::purge`]): the paced queue counts a chunk as
//! sent when it is taken, up to 80 ms more than the phone was sent.

use std::collections::HashMap;

use lmgw_api_types::realtime::OutputAudioStarved;

use super::FRAME_SAMPLES;

/// One chunk of a response's released audio, as the paced queue holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub response_id: String,
    pub item_id: String,
    /// PCM16 at 24 kHz.
    pub pcm: Vec<i16>,
}

/// The writer's paced queue as the frame clock pulls it (module doc).
pub(crate) trait Pull {
    /// The next chunk of released audio, if one is ready now. The
    /// transcript deltas queued ahead of it leave with this pull, onto the
    /// data channel, so they go out with the audio they belong to.
    fn chunk(&mut self) -> Option<Chunk>;

    /// The response whose audio may still come — it paces, and its last
    /// chunk has not been taken (its drained marker is not reached) — if
    /// any. A tick that finds no audio pads for it; with none, it sends
    /// silence.
    fn speaking(&self) -> Option<String>;
}

/// A run of one item's audio in a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub item_id: String,
    /// Where in the frame it starts, in samples.
    pub at: usize,
    /// The item's samples it carried, `from..to`, counted from the item's
    /// first: what the heard table cuts by.
    pub from: u64,
    pub to: u64,
}

/// One tick's 20 ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutFrame {
    /// [`FRAME_SAMPLES`] of PCM16 at 24 kHz, for the encoder.
    pub pcm: Vec<i16>,
    /// The response it belongs to: whose audio it carried, or which it
    /// padded; `None` for silence between responses.
    pub response_id: Option<String>,
    /// The items' audio it carried, in order.
    pub spans: Vec<Span>,
    /// Samples of padding at its end: an underrun of a speaking response.
    pub padded: usize,
    /// `lmgw.output_audio.starved`, on the tick where the response ran dry
    /// or came again.
    pub starved: Option<OutputAudioStarved>,
    /// The response whose first audio this frame carries: its
    /// `output_audio_buffer.started` goes with it (§6.2).
    pub started: Option<String>,
}

impl OutFrame {
    /// The samples of items' audio it carried.
    #[cfg(test)]
    pub fn audio(&self) -> usize {
        self.spans.iter().map(|s| (s.to - s.from) as usize).sum()
    }
}

/// The frame clock's side of the paced queue (module doc).
#[derive(Debug, Default)]
pub(crate) struct Frames {
    /// The chunk being cut, and how far it is cut.
    held: Option<(Chunk, usize)>,
    /// The latest response that sent audio, and per item of it the samples
    /// sent so far.
    sent: Option<(String, Vec<(String, u64)>)>,
    /// The response that starved and has not had audio since.
    starved: Option<String>,
}

impl Frames {
    /// The next tick's 20 ms, pulled from `pull` (module doc).
    pub fn frame(&mut self, pull: &mut impl Pull) -> OutFrame {
        // The latest response that sent audio before this frame: a frame of
        // another one's audio is that one's first.
        let before = self.sent.as_ref().map(|(r, _)| r.clone());
        let mut pcm = Vec::with_capacity(FRAME_SAMPLES);
        let mut spans: Vec<Span> = Vec::new();
        let mut response: Option<String> = None;
        while pcm.len() < FRAME_SAMPLES {
            let Some((chunk, off)) = self.held.take().or_else(|| pull.chunk().map(|c| (c, 0)))
            else {
                break;
            };
            // A frame carries one response's audio: the next one's waits
            // for the next tick.
            if response.as_ref().is_some_and(|r| *r != chunk.response_id) {
                self.held = Some((chunk, off));
                break;
            }
            let take = (FRAME_SAMPLES - pcm.len()).min(chunk.pcm.len() - off);
            if take > 0 {
                let from = self.note(&chunk.response_id, &chunk.item_id, take as u64);
                match spans.last_mut() {
                    Some(s) if s.item_id == chunk.item_id && s.to == from => s.to += take as u64,
                    _ => spans.push(Span {
                        item_id: chunk.item_id.clone(),
                        at: pcm.len(),
                        from,
                        to: from + take as u64,
                    }),
                }
                pcm.extend_from_slice(&chunk.pcm[off..off + take]);
                response.get_or_insert_with(|| chunk.response_id.clone());
            }
            if off + take < chunk.pcm.len() {
                self.held = Some((chunk, off + take));
            }
        }
        let speaking = pull.speaking();
        let audio = pcm.len();
        let padded = if speaking.is_some() {
            FRAME_SAMPLES - audio
        } else {
            0
        };
        pcm.resize(FRAME_SAMPLES, 0);
        let starved = match (&response, &speaking) {
            // Audio: a response that starved has it again.
            (Some(r), _) => {
                let again = self.starved.take().filter(|s| s == r);
                again.map(|r| OutputAudioStarved {
                    response_id: r,
                    starved: false,
                })
            }
            // None, while a response speaks: it starved, said once.
            (None, Some(s)) if self.starved.as_ref() != Some(s) => {
                self.starved = Some(s.clone());
                Some(OutputAudioStarved {
                    response_id: s.clone(),
                    starved: true,
                })
            }
            (None, Some(_)) => None,
            // Nothing speaks: whatever starved is over.
            (None, None) => {
                self.starved = None;
                None
            }
        };
        let started = response.clone().filter(|r| before.as_ref() != Some(r));
        OutFrame {
            pcm,
            response_id: response.or(if padded > 0 { speaking } else { None }),
            spans,
            padded,
            starved,
            started,
        }
    }

    /// `response_id` was cut (a cancel, a barge-in, a stop, a `clear`):
    /// what of its chunk is still held is dropped, and the samples sent of
    /// each of its items are answered, exact to the frame — what heard is
    /// without a position (§5.4).
    pub fn purge(&mut self, response_id: &str) -> HashMap<String, u64> {
        if self
            .held
            .as_ref()
            .is_some_and(|(c, _)| c.response_id == response_id)
        {
            self.held = None;
        }
        if self.starved.as_deref() == Some(response_id) {
            self.starved = None;
        }
        self.sent(response_id)
    }

    /// Whether a chunk of `response_id` is held, cut partway: not all of
    /// its audio has left.
    pub fn holds(&self, response_id: &str) -> bool {
        self.held
            .as_ref()
            .is_some_and(|(c, _)| c.response_id == response_id)
    }

    /// The session is closing: what is held goes, and a starved response
    /// with it.
    pub fn clear(&mut self) {
        self.held = None;
        self.starved = None;
    }

    /// The samples sent of each of `response_id`'s items so far; empty when
    /// it is not the latest response that sent audio.
    pub fn sent(&self, response_id: &str) -> HashMap<String, u64> {
        match &self.sent {
            Some((r, items)) if r == response_id => items.iter().cloned().collect(),
            _ => HashMap::new(),
        }
    }

    /// `n` more samples of `item_id` of `response_id` are sent: where in the
    /// item they start.
    fn note(&mut self, response_id: &str, item_id: &str, n: u64) -> u64 {
        if self.sent.as_ref().is_none_or(|(r, _)| r != response_id) {
            self.sent = Some((response_id.to_string(), Vec::new()));
        }
        let items = &mut self.sent.as_mut().expect("set above").1;
        match items.iter_mut().find(|(i, _)| i == item_id) {
            Some((_, sent)) => {
                let from = *sent;
                *sent += n;
                from
            }
            None => {
                items.push((item_id.to_string(), n));
                0
            }
        }
    }
}

#[cfg(test)]
mod tests;
