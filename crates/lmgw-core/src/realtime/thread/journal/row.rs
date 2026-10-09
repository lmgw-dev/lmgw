//! A heard response's user row (voice-audio-input design §3.3): the first
//! phase of its slot, written once the core said what was heard
//! ([`In::Heard`](super::In::Heard)) and the responder said its attempt's
//! fate ([`In::Began`](super::In::Began)) — or a `Saved` came from a turn
//! that never said it — before the slot waits for its reply.
//!
//! - **Written as an insert that moves no generation**
//!   (`bound::append_spoken_user`), under the thread's conditional write at
//!   the generation the turn began at: the reply being generated can still
//!   be saved after it. If the history moved meanwhile (another window's
//!   turn, an edit), or no turn began, it is written as today — a history
//!   write; a superseded reply is not saved anyway.
//! - **What it holds:** the new turns' words; `voice` as a spoken turn's,
//!   with `input: audio` and — when an audio turn's transcription failed —
//!   `transcript_error`, only when the attempt carried the audio to the
//!   model (WP3 review #3): a skipped or refused attempt heard nothing, and
//!   its row is a transcript turn's. No words and no heard failure: no row.
//!   A **veto** (every turn the response answers came back without words):
//!   no row, and nothing saved or said.
//! - **The answer** goes to the pre-save barrier: the row's id, `failed`
//!   (written, no words, the transcription failed), no row, the veto, or
//!   `unwritten` — the store refused the row (its words lead the next user
//!   entry) or the thread went: the reply is not saved, and a retry waiting
//!   for the row says the row's failure (WP3 review #8).
//!
//! `lmgw.chat.user` carries the response's id, so the page puts the user
//! bubble before that response's reply (the release and this write race by
//! one store write); a row that names an untitled thread says the new title
//! (`lmgw.chat.thread`, through the session's compare).

use serde_json::Value;

use super::super::super::protocol::ServerEvent;
use super::user::user_voice;
use super::{RowTx, Task, UserTurn};
use crate::store::InputPath;
use crate::web::chat_voice::bound::{self, UserRow};

#[cfg(test)]
mod tests;

/// A heard response's deferred row, as its inputs arrive.
pub(super) struct Row {
    tx: RowTx,
    /// What the core said once the response's audio turns were heard: the
    /// new turns, and the veto.
    pub heard: Option<(Vec<UserTurn>, bool)>,
    /// The attempt's fate, once the responder said it.
    pub began: Option<Began>,
}

/// A heard response's attempt, as the responder says it
/// ([`In::Began`](super::In::Began)).
#[derive(Debug, Clone, Copy)]
pub(super) struct Began {
    /// The generation its turn began at; `None`: none began.
    pub generation: Option<u64>,
    /// It carried the audio to the model: the model heard it.
    pub carried: bool,
}

impl Row {
    pub fn new(tx: RowTx) -> Self {
        Self {
            tx,
            heard: None,
            began: None,
        }
    }

    /// Whether it can be written: heard, and the attempt's fate said, or
    /// the turn said what it saved without saying it.
    pub fn ready(&self, saved: bool) -> bool {
        self.heard.is_some() && (self.began.is_some() || saved)
    }
}

impl Task {
    /// Write slot `gen`'s deferred row (module doc), and answer its barrier.
    pub(super) async fn row(&mut self, gen: u64) {
        let Some(slot) = self.slots.get_mut(&gen) else {
            return;
        };
        let Some(row) = slot.row.take() else {
            return;
        };
        let response_id = slot.response_id.clone();
        // A turn that said what it saved without `Began` never began, and
        // carried nothing.
        let began = row.began.unwrap_or(Began {
            generation: None,
            carried: false,
        });
        let Some((turns, veto)) = row.heard else {
            // The session ended before the turn was transcribed.
            tracing::warn!(
                "{}: response {response_id}'s spoken turn was never transcribed (the session \
                 ended first); nothing of it is written to chat thread {}",
                self.label,
                self.thread_id
            );
            return;
        };
        if veto {
            if let Some(slot) = self.slots.get_mut(&gen) {
                slot.vetoed = true;
            }
            row.tx.send_replace(Some(UserRow::Veto));
            return;
        }
        let answer = self.write_row(turns, began, &response_id).await;
        row.tx.send_replace(Some(answer));
    }

    /// The row of `turns` (module doc): the barrier's answer.
    async fn write_row(
        &mut self,
        turns: Vec<UserTurn>,
        began: Began,
        response_id: &str,
    ) -> UserRow {
        // Heard only when the attempt carried the audio (module doc).
        let new: Vec<UserTurn> = std::mem::take(&mut self.unwritten)
            .into_iter()
            .chain(turns.into_iter().map(|mut t| {
                t.heard &= began.carried;
                if !t.heard {
                    t.error = None;
                }
                t
            }))
            .filter(|t| !self.written.contains_key(&t.item_id))
            .collect();
        let spoken: Vec<&UserTurn> = new.iter().filter(|t| !t.text.trim().is_empty()).collect();
        let failed = new.iter().filter(|t| t.heard).find_map(|t| t.error.clone());
        if spoken.is_empty() && failed.is_none() {
            return UserRow::NoRow;
        }
        let thread = match bound::thread_checked(&self.state, self.thread_id).await {
            Ok(Some(thread)) => thread,
            Ok(None) => {
                tracing::warn!(
                    "{}: chat thread {} is gone; the spoken turn is not written",
                    self.label,
                    self.thread_id
                );
                return UserRow::Unwritten;
            }
            Err(why) => {
                self.not_written(new, &why);
                return UserRow::Unwritten;
            }
        };
        // A device's thread that left its reach since the bind (the
        // self-admin toolset attached, or the device's admin-tools switch
        // turned off) is gone for it (L3, review W4-3): nothing the device
        // said is written there. The writes below check again under the
        // thread's lock, against a change that lands in between.
        if !self.caller.sees(&self.state.snapshot(), &thread) {
            tracing::warn!(
                "{}: chat thread {} is out of this device's reach now; the spoken turn is not \
                 written",
                self.label,
                self.thread_id
            );
            return UserRow::Unwritten;
        }
        let content = spoken
            .iter()
            .map(|t| t.text.trim())
            .collect::<Vec<_>>()
            .join("\n");
        let counted: Vec<&UserTurn> = if spoken.is_empty() {
            new.iter().filter(|t| t.heard).collect()
        } else {
            spoken.clone()
        };
        let mut voice = user_voice(&counted);
        // How it reached the model: as audio once heard; a transcript
        // turn's row says nothing new (§5).
        voice.input = new.iter().any(|t| t.heard).then_some(InputPath::Audio);
        voice.transcript_error = failed;
        let untitled = thread.title == "New chat" || thread.title.trim().is_empty();
        let written = match began.generation {
            Some(generation) => {
                bound::append_spoken_user(
                    &self.state,
                    &thread,
                    generation,
                    &content,
                    &voice,
                    &self.caller,
                )
                .await
            }
            None => Ok(None),
        };
        let id = match written {
            Ok(Some(id)) => id,
            // The history moved, or no turn began: a history write, as
            // today.
            Ok(None) => {
                // No turn follows this write: its hold drops here, and
                // what waits enters once no turn runs.
                match bound::write_user(&self.state, &thread, &content, &voice, &self.caller).await
                {
                    Ok((id, _sent)) => id,
                    Err(why) => {
                        self.not_written(new, &why);
                        return UserRow::Unwritten;
                    }
                }
            }
            Err(why) => {
                self.not_written(new, &why);
                return UserRow::Unwritten;
            }
        };
        for t in &new {
            self.written.insert(t.item_id.clone(), id);
        }
        // A turn began since any reply finalized before: none can be re-cut
        // any more (§8.3), as after any user message.
        for done in self.done.values_mut() {
            done.original = String::new();
            done.record = None;
            done.moved = true;
        }
        self.event(ServerEvent::LmgwChatUser {
            message_id: id,
            content: content.clone(),
            voice: serde_json::to_value(&voice).unwrap_or(Value::Null),
            response_id: Some(response_id.to_string()),
        });
        if untitled && !content.is_empty() {
            if let Some(named) = bound::thread(&self.state, self.thread_id).await {
                let snap = self.state.snapshot();
                self.event(ServerEvent::LmgwChatThread {
                    chat_thread: bound::thread_ref(&snap, &named, &self.caller),
                });
            }
        }
        if spoken.is_empty() {
            UserRow::Failed
        } else {
            UserRow::Written(id)
        }
    }
}
