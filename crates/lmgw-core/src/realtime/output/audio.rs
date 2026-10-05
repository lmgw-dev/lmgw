//! A speaking response's output (realtime design §2.3, §7.3, §8.2).
//!
//! Each synthesized clause reaches the core whole — its spoken text and its
//! audio, PCM16-LE at 24 kHz — and goes out as one
//! `response.output_audio_transcript.delta` (the clause's text, a space
//! before every clause but the first, so the deltas add up to the
//! transcript) and its audio as `response.output_audio.delta`s of 100 ms.
//! All of it is queued **purgeable**, so the writer paces it and a cancel
//! takes back what has not left; the transcript delta travels with its
//! clause's audio. The audio goes to the writer as PCM slices of the
//! clause, never copied, and is base64-encoded as it leaves (§8.2).
//!
//! The message item opens on the first clause with an `audio` content part
//! and stays open until the audio has played (the drained acknowledgement,
//! `lifecycle::ending`): only then do `output_audio.done`,
//! `output_audio_transcript.done`, `content_part.done` and the item's done
//! events go out. The item's transcript is its heard table's text
//! (`conversation::audio`): what was queued, cut to what was sent when the
//! response is cancelled ([`Output::keep_sent`]).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use bytes::Bytes;

use super::super::conversation::Conversation;
use super::super::heard::Written;
use super::super::ids::Ids;
use super::super::protocol::{Item, ServerEvent, PCM_RATE};
use super::super::writer::Outbox;
use super::Output;

/// How much audio one `response.output_audio.delta` carries (§8.2).
pub(crate) const DELTA_MS: u64 = 100;

/// [`DELTA_MS`] at 24 kHz, in PCM16 bytes.
const CHUNK_BYTES: usize = (PCM_RATE as u64 * DELTA_MS / 1000) as usize * 2;

/// How a response speaks.
#[derive(Debug, Clone)]
pub(crate) struct AudioOut {
    /// The voice `response.audio.output.voice` echoes — always a string,
    /// `{id}` included: `@openai/agents`' schema wants one there.
    pub voice: String,
    /// `output_lead_ms` (§8.2).
    pub lead: Duration,
}

impl Output {
    /// One synthesized clause (module doc). A text response gets none.
    pub fn clause(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        text: &str,
        written: Written,
        pcm: &Bytes,
    ) {
        let Some(lead) = self.audio.as_ref().map(|a| a.lead) else {
            return;
        };
        let at = match self.text_at {
            Some(at) => at,
            None => self.open_message(conv, ids, ob),
        };
        if !self.paced {
            ob.pace(self.gen, lead);
            self.paced = true;
        }
        let item_id = self.items[at as usize].clone();
        let first = conv.heard_push(&item_id, text, written, (pcm.len() / 2) as u64);
        let delta = if first {
            text.to_string()
        } else {
            format!(" {text}")
        };
        ob.send_purgeable(
            self.gen,
            ServerEvent::OutputAudioTranscriptDelta {
                at: self.part_ref(at),
                delta,
            },
        );
        for start in (0..pcm.len()).step_by(CHUNK_BYTES) {
            let end = (start + CHUNK_BYTES).min(pcm.len());
            ob.send_audio(self.gen, self.part_ref(at), pcm.slice(start..end));
        }
    }

    /// What the model wrote after the spoken message's last clause and the
    /// voice left out: the message's history keeps it. With no message open
    /// — nothing of the answer was said — there is no item to keep it in,
    /// and the log says so.
    pub fn unspoken(&self, conv: &mut Conversation, raw: &str) {
        if self.audio.is_none() || raw.is_empty() {
            return;
        }
        match self.text_at {
            // Whitespace too: the space between a preamble and the answer
            // after its tool call is what the model wrote, and a bound
            // session's cut is a prefix match against the stored reply
            // (chat-voice §8.4; it used to read "Moment.Es sind").
            Some(at) => conv.heard_unspoken(&self.items[at as usize], raw),
            None if raw.trim().is_empty() => {}
            None => tracing::info!(
                "realtime response {}: nothing of the answer was said, so its {} bytes of \
                 unspoken text (code) are in no item and not in the model's history (§7.2)",
                self.id,
                raw.len()
            ),
        }
    }

    /// A cancel: every audio item still open keeps what the writer sent of
    /// it — `sent` samples per item, from the purge (§7.3).
    pub fn keep_sent(&self, conv: &mut Conversation, sent: &HashMap<String, u64>) {
        if self.audio.is_none() {
            return;
        }
        for (at, id) in self.items.iter().enumerate() {
            let is_message = matches!(conv.get(id), Some(Item::Message(_)));
            if !self.closed[at] && is_message {
                conv.heard_keep(id, sent.get(id).copied().unwrap_or(0));
            }
        }
    }

    /// Whether `item_id` is an item of this response still being produced.
    pub fn producing(&self, item_id: &str) -> bool {
        self.items
            .iter()
            .zip(&self.closed)
            .any(|(id, closed)| id == item_id && !closed)
    }

    /// `response.audio`: the format and the voice, for a speaking response.
    pub(super) fn audio_object(&self) -> Option<Value> {
        self.audio.as_ref().map(|a| {
            json!({"output": {
                "format": {"type": "audio/pcm", "rate": PCM_RATE},
                "voice": a.voice,
            }})
        })
    }
}
