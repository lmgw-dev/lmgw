//! The journal's inputs (`super`'s module doc): what the core and the
//! responders send it, and how each lands in its queue and its slots.

use tokio::sync::oneshot;

use super::super::super::protocol::ErrorObject;
use super::super::super::transcribe::Facts;
use super::super::reply::Heard;
use super::{row, Op, Slot, Task};
use crate::store::{ServedModel, VoiceTiming};

pub(crate) use super::super::super::lifecycle::RowTx;

/// One committed turn a response answers, for its user message.
#[derive(Debug, Clone)]
pub(crate) struct UserTurn {
    pub item_id: String,
    /// Its transcript (empty: no words).
    pub text: String,
    /// How it was transcribed, when the session knows.
    pub asr: Option<Facts>,
    /// Why its transcription failed, for a turn a model heard (its row
    /// says so, voice-audio-input design §3.2).
    pub error: Option<String>,
    /// It went to the chat model as audio — heard once the response's
    /// attempt carried it ([`In::Began`]).
    pub heard: bool,
}

/// What the core knows of a response that ended: its timing, and the TTS
/// that spoke it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ended {
    pub timing: VoiceTiming,
    pub tts: Option<String>,
    pub tts_answered_by: Option<String>,
    pub voice: Option<String>,
}

pub(crate) enum In {
    /// A response was launched: its user entry, then its reply slot
    /// (`super`'s module doc). `reply` gets the user message's id — `None`
    /// when it wrote none — once every earlier entry is done.
    Response {
        gen: u64,
        response_id: String,
        turns: Vec<UserTurn>,
        reply: oneshot::Sender<Result<Option<i64>, ErrorObject>>,
        /// A response that hears the user's audio: its user row is deferred
        /// (`super`'s module doc), and this is its pre-save barrier's
        /// sending end.
        row: Option<RowTx>,
    },
    /// The responder's, for a response that hears audio: its attempt's
    /// fate — the generation its turn began at (`None` when none began: its
    /// audio attempt was skipped or refused before it began), and whether it
    /// carried the audio to the model (WP3 review #3). Its row may be
    /// written now.
    Began {
        gen: u64,
        generation: Option<u64>,
        carried: bool,
    },
    /// The core's, for a response that hears audio: its last audio turn is
    /// transcribed — the new turns' words, ASR facts and errors — and
    /// whether every turn it answers came back without words (`veto`).
    Heard {
        gen: u64,
        turns: Vec<UserTurn>,
        veto: bool,
    },
    /// The responder's: what response `gen`'s turn saved — the reply's id
    /// (`None`: nothing saved) and the generation the turn began at — and
    /// the chat model that answered, for the timing: the core's account of
    /// it misses what a cancelled response said late (WP8 review NIT 6).
    Saved {
        gen: u64,
        message_id: Option<i64>,
        generation: Option<u64>,
        chat: Option<ServedModel>,
    },
    /// The core's: what was heard of response `gen`'s reply. `ended` with
    /// the first (at drain or cancel); a later one is a truncate.
    Cut {
        gen: u64,
        heard: Heard,
        ended: Option<Box<Ended>>,
    },
    /// The core's, as the session ends: committed turns no response
    /// answers, written as a user entry with no reply slot (§8.6).
    User { turns: Vec<UserTurn> },
}

impl Task {
    /// One input, into the queue or its slot.
    pub(super) fn apply(&mut self, input: In) {
        match input {
            In::Response {
                gen,
                response_id,
                turns,
                reply,
                row,
            } => {
                self.ops.push_back(Op::User {
                    turns,
                    reply: Some(reply),
                    deferred: row.is_some(),
                });
                self.ops.push_back(Op::Slot(gen));
                self.slots.insert(
                    gen,
                    Slot {
                        response_id,
                        saved: None,
                        chat: None,
                        heard: None,
                        ended: None,
                        row: row.map(row::Row::new),
                        vetoed: false,
                    },
                );
            }
            In::Began {
                gen,
                generation,
                carried,
            } => {
                if let Some(r) = self.slots.get_mut(&gen).and_then(|s| s.row.as_mut()) {
                    r.began = Some(row::Began {
                        generation,
                        carried,
                    });
                }
            }
            In::Heard { gen, turns, veto } => {
                if let Some(r) = self.slots.get_mut(&gen).and_then(|s| s.row.as_mut()) {
                    r.heard = Some((turns, veto));
                }
            }
            In::Saved {
                gen,
                message_id,
                generation,
                chat,
            } => {
                if let Some(slot) = self.slots.get_mut(&gen) {
                    slot.saved = Some((message_id, generation));
                    slot.chat = chat;
                }
            }
            In::User { turns } => self.ops.push_back(Op::User {
                turns,
                reply: None,
                deferred: false,
            }),
            In::Cut { gen, heard, ended } => match self.slots.get_mut(&gen) {
                Some(slot) => {
                    slot.heard = Some(heard);
                    if ended.is_some() {
                        slot.ended = ended;
                    }
                }
                None if self.done.contains_key(&gen) => {
                    self.ops.push_back(Op::ReCut { gen, heard });
                }
                None => {}
            },
        }
    }
}
