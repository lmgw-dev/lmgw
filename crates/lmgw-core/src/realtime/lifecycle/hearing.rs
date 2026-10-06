//! Voice turns the model hears, the core's half (voice-audio-input design
//! §3.1–§3.3): the commit, the launch and the transcript of an audio turn.
//!
//! - **At the commit** ([`Core::commit_upload`]): a turn committed while
//!   the verdict says `audio` (`Bound.audio_input`, with the session's
//!   memory of a refusal on top) is an audio turn. Its WAV is built once,
//!   on the blocking pool, and shared by its ASR call and the request; the
//!   turn is in `Bound.hearing` until its transcript is in. An owed
//!   response is decided at once (`pending`).
//! - **At the launch** ([`Core::launch_heard`]): the response's new turns
//!   go to the responder as spoken parts — an audio turn as its WAV, one
//!   already transcribed as its text — and to the journal as one deferred
//!   user row, with the pre-save barrier between them. The response starts
//!   held (`held`). Without an audio turn among them, the launch is
//!   today's.
//! - **At the transcript** ([`Core::heard_settled`]): once a response's
//!   last audio turn is transcribed, the journal is told what was heard,
//!   and the hold released or vetoed.
//!
//! **Heard means carried** (WP3 review #3): a turn handed to a response is
//! heard only once the response's attempt carried its audio to the model —
//! the responder says so ([`Core::carried`]): the model answered from it.
//! A skipped or refused attempt, or one cut before the model said anything,
//! heard nothing. Only a heard turn's failed transcription stays the
//! session's own (the reply plays, its row says "not transcribed"), and
//! only while its response is active; any other is said as with audio input
//! off, and the response, with no other words to answer, is vetoed. A
//! failure that is no transcription — no speech-to-text model, the key's
//! policy, the session's stop — is never heard (`input::attempted`,
//! WP3 review #2).
//!
//! What a launch went by is said as `lmgw.chat.input` while audio input is
//! on, and kept for the response's timing (§5).

use tokio::sync::watch;

use super::super::input::has_words;
use super::super::protocol::{ContentPart, Item, ServerEvent};
use super::super::thread::hearing::{Hearing, Launched, Was};
use super::super::thread::journal::In;
use super::super::thread::turn::audio::{Launch, Spoken};
use super::super::transcribe::{Upload, Wav};
use super::held::{Held, NOT_TRANSCRIBED, NO_WORDS};
use super::Core;
use crate::store::InputPath;
use crate::web::chat_voice::bound::UserRow;

/// The journal's end of a heard response's pre-save barrier (§3.3).
pub(crate) type RowTx = watch::Sender<Option<UserRow>>;

impl Core {
    /// The bound session's audio turns still being transcribed.
    pub(in crate::realtime) fn hearing(&self) -> Option<&Hearing> {
        self.bound.as_ref().map(|b| &b.hearing)
    }

    /// Whether `item_id` is an audio turn still being transcribed.
    pub(in crate::realtime) fn hears(&self, item_id: &str) -> bool {
        self.hearing().is_some_and(|h| h.hears(item_id))
    }

    /// Whether a response must still wait before it launches: for a
    /// transcript — a turn the chat model hears goes as audio
    /// (`Transcriber::busy_for_launch`) — or for an MCP label's listing in
    /// flight (realtime-server-tools §1.2).
    pub(in crate::realtime) fn busy_for_launch(&self) -> bool {
        self.transcriber.busy_for_launch(|id| self.hears(id)) || self.mcp.table.listing()
    }

    /// The upload of turn `item_id`, committed now (module doc): its WAV
    /// built once when the chat model hears it — the turn is then in the
    /// hearing — else the segment, as today. `true`: an audio turn.
    pub(in crate::realtime) fn commit_upload(
        &mut self,
        item_id: &str,
        samples: Vec<i16>,
    ) -> (Upload, bool) {
        if !self.hears_next() {
            return (samples.into(), false);
        }
        let wav = Wav::build(samples);
        if let Some(b) = self.bound.as_mut() {
            b.hearing.insert(item_id.to_string(), wav.clone());
        }
        (wav.into(), true)
    }

    /// Response `gen`, launching with the new turns `ids` and answering
    /// `answers` (module doc): its spoken parts and the journal's end of
    /// its barrier, when an audio turn is among `ids`; `None` — today's
    /// launch — when none is. The response starts held.
    pub(super) fn launch_heard(
        &mut self,
        gen: u64,
        ids: &[String],
        answers: &[String],
    ) -> Option<(RowTx, Launch)> {
        let conv = &self.conversation;
        let b = self.bound.as_mut()?;
        if !ids.iter().any(|id| b.hearing.hears(id)) {
            return None;
        }
        let parts = ids
            .iter()
            .map(|id| match b.hearing.wav(id) {
                Some(wav) => Spoken::Wav(wav),
                None => Spoken::Text(super::bound::transcript(conv.get(id))),
            })
            .collect();
        let mut all: Vec<String> = answers.to_vec();
        for id in ids {
            if !all.contains(id) {
                all.push(id.clone());
            }
        }
        let audio = ids
            .iter()
            .filter(|id| b.hearing.hears(id))
            .cloned()
            .collect();
        b.hearing.launch(
            gen,
            Launched {
                new: ids.to_vec(),
                answers: all,
                audio,
                carried: None,
            },
        );
        if let Some(active) = self.active.as_mut() {
            active.held = Some(Held::default());
            active.timing.held_at = Some(std::time::Instant::now());
        }
        let (tx, rx) = watch::channel(None);
        Some((
            tx,
            Launch {
                parts,
                user_row: rx,
            },
        ))
    }

    /// What response `gen`'s turns went by, as the session says it while
    /// audio input is on (module doc): `heard` — it heard an audio turn —
    /// or the verdict's reason for the transcript.
    pub(super) fn launch_input(&mut self, gen: u64, response_id: &str, heard: bool) {
        if !heard && !self.audio_input_on() {
            return;
        }
        let (input, why) = if heard {
            (InputPath::Audio, None)
        } else {
            let verdict = self.audio_now();
            let why = match verdict {
                Some(v) if v.path == InputPath::Audio => {
                    Some("its turns were transcribed before it started".to_string())
                }
                Some(v) => v.why,
                None => None,
            };
            (InputPath::Transcript, why)
        };
        if let Some(served) = self.bound.as_mut().and_then(|b| b.responses.get_mut(&gen)) {
            served.input = Some((input, why.clone()));
        }
        self.ob.send(ServerEvent::LmgwChatInput {
            response_id: response_id.to_string(),
            input,
            why,
        });
    }

    /// `item_id`'s transcript came in: it is no audio turn any more, and
    /// what it was (module doc).
    pub(in crate::realtime) fn hearing_transcribed(&mut self, item_id: &str) -> Was {
        match self.bound.as_mut() {
            Some(b) => b.hearing.transcribed(item_id),
            None => Was::NotHeard,
        }
    }

    /// Response `gen`'s attempt carried its audio to the model, or did not
    /// (`Msg::Carried`, module doc).
    pub(super) fn carried(&mut self, gen: u64, carried: bool) {
        if let Some(b) = self.bound.as_mut() {
            b.hearing.carry(gen, carried);
        }
    }

    /// Whether a turn response `gen` went with as audio is heard now: the
    /// response is active and its attempt carried the audio (module doc).
    /// `last`: the response, when this transcript settles it.
    pub(in crate::realtime) fn heard_now(&self, gen: u64, last: Option<&Launched>) -> bool {
        let carried = match last {
            Some(l) => l.carried,
            None => self.hearing().and_then(|h| h.carried(gen)),
        };
        carried == Some(true) && self.active.as_ref().is_some_and(|a| a.output.gen == gen)
    }

    /// Response `gen`'s last audio turn is transcribed (module doc): the
    /// journal hears what was said, and — while it is the active response
    /// and `live` (the session goes on) — its hold is released or vetoed.
    pub(in crate::realtime) fn heard_settled(&mut self, gen: u64, launched: Launched, live: bool) {
        let carried = launched.carried == Some(true);
        let conv = &self.conversation;
        let hearing = self.hearing();
        let words = launched
            .answers
            .iter()
            .any(|id| conv.get(id).is_some_and(|i| has_words(i, hearing)));
        // A turn it answers whose transcription failed and that a model
        // heard — its own audio turn, when its attempt carried it, or one
        // owed again after a cut whose response's attempt did (only those
        // keep their error, `bound_transcribed`): the reply plays
        // (decision 8). One nobody heard was said as with audio input off.
        let errors = self.bound.as_ref().map(|b| &b.asr_errors);
        let failed = launched.answers.iter().any(|id| {
            let heard = carried || !launched.audio.contains(id);
            heard && errors.is_some_and(|e| e.contains_key(id)) && untranscribed(conv.get(id))
        });
        let veto = !words && !failed;
        let reason = if launched
            .answers
            .iter()
            .any(|id| untranscribed(conv.get(id)))
        {
            NOT_TRANSCRIBED
        } else {
            NO_WORDS
        };
        // Each turn as the row takes it: which went as audio — the journal
        // writes it heard only once the attempt carried it (`In::Began`) —
        // and why a heard one has no transcript. A turn that went as its
        // (empty) text, or that nobody heard, is not "heard and not
        // transcribed".
        let mut turns = self.user_turns(&launched.new);
        for t in &mut turns {
            t.heard = launched.audio.contains(&t.item_id);
            if !(t.heard && carried) {
                t.error = None;
            }
        }
        if let Some(b) = self.bound.as_mut() {
            // A failure nobody heard is no row's: only a heard one stays,
            // for a response that answers it after a cut.
            for id in &launched.new {
                if !(carried && launched.audio.contains(id)) {
                    b.asr_errors.remove(id);
                }
            }
            if let Some(j) = &b.journal {
                j.send(In::Heard { gen, turns, veto });
            }
        }
        let active = self
            .active
            .as_ref()
            .is_some_and(|a| a.output.gen == gen && a.held.as_ref().is_some_and(Held::holding));
        if !live || !active {
            return;
        }
        if veto {
            self.veto(reason);
        } else {
            self.release();
        }
    }
}

/// A committed turn whose ASR call failed: audio with no transcript.
fn untranscribed(item: Option<&Item>) -> bool {
    let Some(Item::Message(m)) = item else {
        return false;
    };
    m.content.iter().any(|p| {
        matches!(
            p,
            ContentPart::InputAudio {
                transcript: None,
                ..
            }
        )
    })
}
