//! A heard response's turn (voice-audio-input design §3.5): the attempt
//! with the user's audio, and the one retry with the transcript.
//!
//! **The attempt.** The core hands a heard response its spoken parts and
//! the journal's answer about its user row ([`Heard`]). The responder
//! re-reads the thread as it always does; when a thread-level row of the
//! verdict fails by now (the setting is off, knowledge bases in auto mode),
//! it skips the audio and runs the transcript turn once the row is settled
//! — or, when the row ends the response (a failed transcription, a veto, the
//! stop), relays how it ended as a retry would. Otherwise the turn goes with
//! the spoken parts (`TurnOpts::spoken`) and the barrier
//! (`TurnOpts::user_row`); route and capability are left to `fit_route` and
//! the model's own refusal.
//!
//! **Retried once with the transcript** ([`refusal`]): a refusal of the
//! attempt before its first `delta`, `reasoning` or `tool` frame — lmgw's
//! own about the audio (`audio_input_unsupported`: the model that answers
//! cannot take it; a context guard's) or the model's server's (an audio
//! part it cannot take, a body too large, a context exceeded). The attempt's frames from its first `error` frame
//! with no gateway error behind it (an MCP server that did not answer,
//! before any model call) are held back with it, and only a frame carrying
//! a gateway error decides: a refusal drops them (the retry says them
//! again), anything else relays them. The responder waits for the user row
//! and starts a second turn, `Fresh {user_message_id: None}` with no spoken
//! parts, over the history now holding the row. A failed transcription has
//! no words to send: the response fails with `transcription_failed`. A
//! veto, or the response's stop, ends it quietly. A failed retry is the
//! response's error: there is no third attempt. Neither attempt sends a
//! `turn` frame.
//!
//! **The journal's row** waits for the attempt's fate ([`In::Began`]): the
//! generation it began at (`start_turn` returned: the history is read by
//! then, so the row written under that generation is never in its own
//! request beside its audio), and whether it **carried the audio** to the
//! model (WP3 review #3) — said, to the core too ([`Msg::Carried`]), at the
//! first frame the model answered with (`true`), or once its frames ended,
//! it was refused, or the response was stopped first (`false`). A skipped
//! attempt says no turn began, and nothing was carried, before it waits for
//! the row, which is then a plain history write. Only a carried turn's row
//! says the model heard it.
//!
//! **The note** ([`Msg::Input`]) says the response goes as its transcript,
//! and why. **The memory** ([`Msg::Refused`], `memory`) is kept by the
//! model that refused, as the turn names who answered: a server's refusal
//! of the audio once its message names the audio, else only once the
//! transcript retry answered — a refusal the transcript meets as well says
//! nothing about the audio; and a llama-server that went away under the
//! audio (a crash, a dropped connection: not retried, the response's error)
//! at once, with a note, so later turns do not pay its restarts again
//! (`refusal::crashed`). Elsewhere a dropped connection says nothing about
//! the audio: that turn goes again as its transcript, and nothing is kept
//! (`refusal::dropped`, review V4). `audio_input_unsupported` and the
//! context guard concern the
//! route the gate took this time, which the next verdict judges itself, and
//! a skipped attempt the thread: never kept.

use tokio::sync::mpsc;

use super::super::super::responder::{self, Msg};
use super::super::journal::{self, In};
use crate::error::GatewayError;
use crate::proxy::StopSignal;
use crate::state::SharedState;
use crate::store::{ChatThread, InputPath};
use crate::web::chat_voice::bound::{self, RowWatch, TurnFrame, TurnLanguage, TurnOpts, VoiceTurn};

mod launch;
mod memory;
mod refusal;
mod seam;
mod settled;
#[cfg(test)]
mod tests;

pub(crate) use launch::{Heard, Launch, Spoken};
pub(crate) use memory::Refused;
use refusal::names_audio;
pub(crate) use refusal::{crashed, dropped, refusal, Refusal};
pub use seam::{heard_response_for_tests, HeardForTests};
use settled::{said, settled};

/// How a bound response's turns start: the thread, its voice and
/// language, and the response's stop for the turn — the same for the
/// attempt and the retry.
pub(super) struct Starter<'a> {
    pub state: &'a SharedState,
    pub thread: &'a ChatThread,
    pub voice: Option<VoiceTurn>,
    pub language: Option<TurnLanguage>,
    pub stop: StopSignal,
    /// The response's turns go as their transcripts because the model lacks
    /// audio input (the verdict's): what the first turn's request rows say.
    pub degraded: Option<String>,
    /// Who the session's turns run as: its principal — a device's turns are
    /// checked against its key and charged to it (client-apps design §1.3).
    pub caller: bound::Caller,
}

/// A started turn: its frames, and the generation it began at.
pub(super) type Started = (mpsc::Receiver<TurnFrame>, Option<u64>);

impl Starter<'_> {
    /// Start a turn answering `user_message_id` (`None`: the history as it
    /// stands, no `turn` frame), with `heard`'s spoken parts and barrier.
    /// `degraded`: what the turn lost before it began (a heard turn going
    /// again as its transcript), for its request rows.
    pub async fn start(
        &self,
        user_message_id: Option<i64>,
        heard: Option<Heard>,
        degraded: Option<String>,
    ) -> Result<Started, GatewayError> {
        let (frames_tx, frames) = mpsc::channel::<TurnFrame>(64);
        let (began_tx, mut began) = tokio::sync::oneshot::channel();
        let (spoken, user_row) = heard.map(|h| (h.spoken, h.user_row)).unzip();
        let opts = TurnOpts {
            stop: Some(self.stop.clone()),
            voice: self.voice.clone(),
            language: self.language.clone(),
            // The plan is made: a refused one refused the turn before.
            heard: None,
            began: Some(began_tx),
            spoken,
            user_row,
            degraded,
            caller: self.caller.clone(),
            // The session's own slot holds its place (realtime §10.3).
            slot: None,
        };
        bound::start_turn(self.state, self.thread, user_message_id, frames_tx, opts)
            .await
            .map_err(GatewayError::Internal)?;
        // Said before `start_turn` returned, and after the history was read.
        Ok((frames, began.try_recv().ok()))
    }
}

/// What the turn does once its frames ended ([`Attempt::next`]).
pub(super) enum Next {
    /// The response's turn is over: relay these frames (none, or how it
    /// ended), then end.
    End(Vec<TurnFrame>),
    /// The transcript turn, started.
    Again(Started),
}

/// A heard response's attempt, and its one retry (module doc).
pub(super) struct Attempt {
    gen: u64,
    tx: responder::Tx,
    /// The journal, told when the attempt began (or that none did).
    journal: Option<journal::Tx>,
    launch: Option<Launch>,
    /// The journal's answer about the user row, once the attempt started.
    row: Option<RowWatch>,
    /// The audio attempt runs and has said nothing yet: a refusal now is
    /// held back ([`Self::screen`]).
    armed: bool,
    /// The thread's model when the attempt started: whom a refusal is kept
    /// for when nothing answered in its place.
    model: String,
    /// Frames held back from an `error` frame with no gateway error behind
    /// it on, while armed ([`Self::screen`]).
    held_back: Vec<TurnFrame>,
    /// The refusal held back.
    refused: Option<HeldRefusal>,
    /// A server's refusal kept only once the transcript retry answered
    /// ([`Self::ended`]).
    pending: Option<Refused>,
    /// The generation the audio attempt began at, for its row.
    generation: Option<u64>,
    /// Whether it carried the audio was said ([`Self::carried`]).
    told: bool,
}

/// A refusal of the audio attempt, held back: what refused, its error, and
/// the model it is kept for.
struct HeldRefusal {
    kind: Refusal,
    error: GatewayError,
    model: String,
}

impl Attempt {
    pub fn new(
        launch: Option<Launch>,
        gen: u64,
        tx: &responder::Tx,
        journal: Option<journal::Tx>,
    ) -> Self {
        Self {
            gen,
            tx: tx.clone(),
            journal,
            launch,
            row: None,
            armed: false,
            model: String::new(),
            held_back: Vec::new(),
            refused: None,
            pending: None,
            generation: None,
            told: false,
        }
    }

    /// Start the response's first turn: today's, for a response nothing
    /// was heard for; the transcript turn once the row is settled, when a
    /// thread-level row of the verdict fails by now (or the turn's audio
    /// could not be prepared); else the audio attempt.
    pub async fn first(
        &mut self,
        starter: &Starter<'_>,
        user_message_id: Option<i64>,
        stop: &StopSignal,
    ) -> Result<Started, GatewayError> {
        let Some(launch) = self.launch.take() else {
            return starter
                .start(user_message_id, None, starter.degraded.clone())
                .await;
        };
        let (heard, unprepared) = launch.prepare().await;
        self.row = Some(heard.user_row.clone());
        self.model = starter.thread.model_alias.clone();
        let snap = starter.state.snapshot();
        let skip = bound::audio_input_rows(&snap, starter.thread).or(unprepared);
        let Some(why) = skip else {
            self.armed = true;
            let started = starter.start(user_message_id, Some(heard), None).await;
            match &started {
                // Whether it carries the audio is said once it is known
                // (module doc).
                Ok((_, generation)) => self.generation = *generation,
                // Refused before it began: the row is a plain history write.
                Err(_) => self.carried(false),
            }
            return started;
        };
        drop(heard);
        // No turn began: the row is a plain history write, and nothing waits
        // for a turn that never comes (module doc).
        self.carried(false);
        self.note(why);
        // How the row ended the response is relayed as a retry relays it.
        match settled(self.row.clone(), stop).await.frames() {
            None => starter.start(None, None, None).await,
            Some(frames) => Ok(said(frames)),
        }
    }

    /// The response heard the user's audio: its save waits for its row.
    pub fn heard(&self) -> bool {
        self.row.is_some()
    }

    /// A frame of the turn as the relay may see it, with any it held back
    /// before (module doc): the audio attempt's refusal and its `done` are
    /// held back, and while armed every frame but `state` from an `error`
    /// frame with no gateway error behind it on.
    pub fn screen(&mut self, f: TurnFrame) -> Vec<TurnFrame> {
        if self.refused.is_some() {
            // What the refused attempt still says: its `done`.
            return Vec::new();
        }
        // The model answered from the audio: it carried it (module doc).
        if matches!(f.event, "delta" | "reasoning" | "tool" | "usage" | "stop") {
            self.carried(true);
        }
        if !self.armed {
            return vec![f];
        }
        match f.event {
            "delta" | "reasoning" | "tool" => self.disarm(f),
            "error" => {
                let Some(e) = f.failure.clone() else {
                    // An MCP server that did not answer: nothing about the
                    // audio — the model has not been asked yet.
                    self.held_back.push(f);
                    return Vec::new();
                };
                let sent = f.sent.clone().unwrap_or_default();
                let model = sent.answered_by.unwrap_or_else(|| self.model.clone());
                let kind = refusal(&e, sent.images)
                    .or_else(|| dropped(&e, sent.llama_server).then_some(Refusal::Dropped));
                if let Some(kind) = kind {
                    self.refused = Some(HeldRefusal {
                        kind,
                        error: e,
                        model,
                    });
                    return Vec::new();
                }
                if crashed(&e, sent.llama_server) {
                    let note = format!(
                        "{model}'s server failed on the audio ({e}): later turns of this \
                         session go to it as their transcript"
                    );
                    let why = format!("its server failed on the audio this session: {e}");
                    self.remember(Refused { model, why }, Some(note));
                }
                self.disarm(f)
            }
            "state" => vec![f],
            _ if !self.held_back.is_empty() => {
                self.held_back.push(f);
                Vec::new()
            }
            _ => vec![f],
        }
    }

    /// The attempt said something of the model's, or failed for good: what
    /// it held back, then `f`.
    fn disarm(&mut self, f: TurnFrame) -> Vec<TurnFrame> {
        self.armed = false;
        let mut out = std::mem::take(&mut self.held_back);
        out.push(f);
        out
    }

    /// The turn's frames ended: the response is over, or a refused audio
    /// attempt goes again as its transcript (module doc).
    pub async fn next(&mut self, starter: &Starter<'_>, stop: &StopSignal) -> Next {
        self.armed = false;
        // Ended, or refused, before the model said anything: not carried.
        self.carried(false);
        let Some(HeldRefusal { kind, error, model }) = self.refused.take() else {
            return Next::End(std::mem::take(&mut self.held_back));
        };
        // The retry says them again.
        self.held_back.clear();
        self.note(kind.why(&error));
        // The model could not take the audio: the retry's request rows say
        // its transcript went instead (`request_logs.degraded`). A guard, a
        // dropped connection or a refusal that names something else is no
        // capability it lacks.
        let lacked = kind == Refusal::Unheard || (kind == Refusal::Server && names_audio(&error));
        let degraded =
            lacked.then(|| crate::degraded::lacks(&model, false, "audio", "transcript sent"));
        if kind == Refusal::Server {
            let why = format!("it refused the audio this session: {error}");
            let kept = Refused { model, why };
            if names_audio(&error) {
                self.remember(kept, None);
            } else {
                self.pending = Some(kept);
            }
        }
        match settled(self.row.clone(), stop).await.frames() {
            None => match starter.start(None, None, degraded).await {
                Ok(started) => Next::Again(started),
                Err(e) => Next::End(settled::refused(&e)),
            },
            Some(frames) => Next::End(frames),
        }
    }

    /// The response's stop was raised (or its voice failed) while the turn
    /// ran: an attempt the model had not answered yet carried nothing
    /// (module doc).
    pub fn halted(&mut self) {
        self.carried(false);
    }

    /// Tell the journal the attempt's fate — the generation it began at (or
    /// that none began) and whether it carried the audio — and the core
    /// whether it did ([`Msg::Carried`]), once (module doc). Nothing for a
    /// response nothing was heard for.
    fn carried(&mut self, carried: bool) {
        if self.row.is_none() || std::mem::replace(&mut self.told, true) {
            return;
        }
        if let Some(j) = &self.journal {
            let _ = j.send(In::Began {
                gen: self.gen,
                generation: self.generation,
                carried,
            });
        }
        let _ = self.tx.send((self.gen, Msg::Carried(carried)));
    }

    /// The response's turn is over; `whole`: it answered. A server's
    /// refusal that waited for the transcript retry is kept once the retry
    /// answered (module doc).
    pub fn ended(&mut self, whole: bool) {
        if let Some(kept) = self.pending.take().filter(|_| whole) {
            self.remember(kept, None);
        }
    }

    /// Tell the core the response goes as its transcript ([`Msg::Input`]).
    fn note(&self, why: String) {
        let _ = self.tx.send((
            self.gen,
            Msg::Input {
                input: InputPath::Transcript,
                why,
            },
        ));
    }

    /// Tell the core to keep `refused` for the session ([`Msg::Refused`]),
    /// saying `note` when no retry said why.
    fn remember(&self, refused: Refused, note: Option<String>) {
        let _ = self.tx.send((self.gen, Msg::Refused { refused, note }));
    }
}
