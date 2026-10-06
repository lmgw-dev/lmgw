//! The Pending phase (realtime design §4.3): an automatic response owed to
//! committed turns, before its `response.created` — and a client's
//! `response.create` held while the user speaks.
//!
//! A turn the detector commits with `create_response` on is **owed** a
//! response. The debt is settled by a response that renders the turn — the
//! automatic one, or any the client starts meanwhile — and by nothing else:
//! - **New speech defers it.** The user is not done; the next turn's commit
//!   joins the owed turns, and the renderer merges the adjacent user items
//!   (§7.2). It is not dropped: a cough's `speech_started` must not strand
//!   the question before it.
//! - **It is decided over every owed turn** once the last one's transcript is
//!   in: a response if any of them has words, none if all are empty (noise)
//!   or failed to transcribe.
//! - **A deferral that ends without a commit** — the buffer cleared, turn
//!   detection switched off, a turn committed with `create_response` off —
//!   decides at once, on the turns owed so far.
//! - **A response already running was rendered before the turn existed**, so
//!   it does not answer it: the owed response starts after that one's
//!   `response.done`, unless the client's own queued `response.create`
//!   (which renders the turn) comes first. One still awaiting transcripts
//!   renders the turn, and settles the debt — so the owed turns join what it
//!   waited for: whether it may answer is judged on them too, and a turn
//!   whose transcription failed before them does not fail it (WP1c review
//!   #1, `input::transcripts_failed`).
//!
//! **No response starts while the user's turn is open** (owner's decision
//! Q3, a deliberate departure from OpenAI, which would start it): a client's
//! `response.create` that arrives while the user speaks — or that was
//! queued behind a response a barge-in cancelled — is **carried** with the
//! debt and starts once, after the turn, answering it too. `@openai/agents`
//! sends its tool follow-up exactly then: right after the cancel's
//! `response.done`, while the user still talks. Started at once it would
//! talk over the user; refused, the client would report an error on every
//! barge-in after a tool preamble and never retry. A second one meanwhile is
//! refused as `conversation_already_has_active_response` — but one that
//! arrives while the held create is the cut response's own, carried again,
//! is absorbed into it (`absorb`, realtime-server-tools §2.5). A carried
//! create is answered even when the turn turns out to have no words: the
//! client asked.
//!
//! **A barge-in** (§6.4, `interrupt`) defers the debt before it cancels
//! anything (WP1c review H5), so neither an owed response that was due nor
//! a queued create starts while the user talks. The cancelled response's own
//! turns are owed again only if the client heard nothing of it — it
//! answered nothing yet — and a client's `response.create` it ran for is
//! held to start again (`interrupt`, B3 review 1). An answer the client was
//! already hearing is **not resumed** when the turn that interrupted it has
//! no words (owner's decision Q2, OpenAI's behaviour; the evidence gate
//! already filters coughs): no response follows, and the log says why.

use super::super::input::has_words;
use super::super::protocol::ErrorObject;
use super::{Core, Create};

mod absorb;

/// The **Pending** phase (module doc).
#[derive(Debug)]
pub(crate) struct Pending {
    /// The committed turns the response is owed to, in order.
    owed: Vec<String>,
    /// New speech started after the last of them, and has not committed.
    deferred: bool,
    /// Decided — a response is due — and waiting for the active response's
    /// end.
    due: bool,
    /// A client's `response.create` held while the user spoke (module doc).
    carried: Option<Create>,
    /// `carried` is a cut response's own create, held again (`interrupt`):
    /// the client's next create is absorbed into it (`absorb`).
    again: bool,
    /// The response a barge-in cut while it was being heard: not resumed if
    /// the turn has no words (module doc), which the log says.
    interrupted: Option<String>,
}

impl Pending {
    fn deferred() -> Self {
        Self {
            owed: Vec::new(),
            deferred: true,
            due: false,
            carried: None,
            again: false,
            interrupted: None,
        }
    }
}

impl Core {
    /// The detector committed `item_id`: with `auto`, a response is owed to
    /// it; without, a deferral it ended decides now. A turn the chat model
    /// `heard` decides at once: it has words until its transcript says
    /// otherwise (voice-audio-input design §3.1).
    pub(in crate::realtime) fn pending_commit(&mut self, item_id: &str, auto: bool, heard: bool) {
        if !auto {
            return self.pending_undefer();
        }
        let p = self.pending.get_or_insert_with(Pending::deferred);
        p.owed.push(item_id.to_string());
        p.deferred = false;
        p.due = false;
        if heard {
            self.pending_decide();
        }
    }

    /// New speech: the owed response waits for the turn it starts — even
    /// one already due, so a barge-in that ends the active response does not
    /// start it while the user talks.
    pub(in crate::realtime) fn pending_defer(&mut self) {
        let sid = self.session.id.as_deref().unwrap_or("?");
        if let Some(p) = self.pending.as_mut() {
            tracing::debug!(
                "realtime {sid}: new speech defers the automatic response to {:?}",
                p.owed
            );
            p.deferred = true;
            p.due = false;
        }
    }

    /// A barge-in cancels a response before the client heard any of it:
    /// what it was to answer is owed again, ahead of anything owed since,
    /// and waits for the turn that interrupted it (module doc).
    pub(in crate::realtime) fn pending_reowe(&mut self, answers: Vec<String>) {
        let p = self.pending.get_or_insert_with(Pending::deferred);
        let mut owed = answers;
        owed.retain(|id| !p.owed.contains(id));
        owed.append(&mut p.owed);
        p.owed = owed;
        p.deferred = true;
        p.due = false;
    }

    /// A barge-in cut response `rid` while it was being heard: it is not
    /// resumed, and if the turn has no words the log says so (Q2).
    pub(in crate::realtime) fn pending_interrupted(&mut self, rid: String) {
        let p = self.pending.get_or_insert_with(Pending::deferred);
        p.deferred = true;
        p.due = false;
        p.interrupted = Some(rid);
    }

    /// Whether a client's `response.create` is held for the user's turn.
    pub(super) fn pending_carries(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.carried.is_some())
    }

    /// A second `response.create` while one is held (module doc).
    pub(super) fn pending_refuse(&mut self, create: Create) {
        let e = ErrorObject::invalid(
            "conversation_already_has_active_response",
            "a response.create is already waiting for the user's turn to end; it starts when \
             the turn is committed and transcribed",
        )
        .for_event(create.event_id.as_deref());
        self.error(e);
    }

    /// Hold the client's `create` until the user's turn is over (module
    /// doc); a second one meanwhile is refused. Only ever called while the
    /// turn is open: its commit, a clear or a switch to manual turns ends
    /// the wait.
    pub(super) fn pending_carry(&mut self, create: Create) {
        if self.pending_carries() {
            return self.pending_second(create);
        }
        tracing::debug!(
            "realtime {}: the user is speaking — the client's response.create waits for the \
             end of the turn",
            self.id()
        );
        let p = self.pending.get_or_insert_with(Pending::deferred);
        p.carried = Some(create);
        p.again = false;
        p.due = false;
        p.deferred = true;
    }

    /// A barge-in cut a client's response before it was heard (`interrupt`):
    /// its `response.create` is held to start again after the turn — unless
    /// one is held already, which renders everything this one would.
    pub(super) fn pending_carry_again(&mut self, create: Create) {
        if self.pending_carries() {
            tracing::debug!(
                "realtime {}: the cut response's own response.create is not held again — one is \
                 held already",
                self.id()
            );
            return;
        }
        tracing::debug!(
            "realtime {}: nothing of the cut response was heard — its response.create starts \
             again after the turn",
            self.id()
        );
        self.pending_carry(create);
        if let Some(p) = self.pending.as_mut() {
            p.again = true;
        }
    }

    /// The speech that deferred the owed response will not commit (module
    /// doc): decide now — or when the last owed transcript is in.
    pub(in crate::realtime) fn pending_undefer(&mut self) {
        let Some(p) = self.pending.as_mut().filter(|p| p.deferred) else {
            return;
        };
        p.deferred = false;
        let waiting = p.owed.last().is_some_and(|id| self.transcriber.has(id));
        if !waiting {
            self.pending_decide();
        }
    }

    /// `item_id`'s transcript is in: if it was the last owed turn's, the
    /// owed response is decided. A turn the chat model `heard` was decided
    /// at its commit: a debt it made due is decided again, so one whose
    /// turns all came back empty is dropped as noise is.
    pub(in crate::realtime) fn pending_resolve(&mut self, item_id: &str, heard: bool) {
        let Some(p) = self.pending.as_ref().filter(|p| !p.deferred) else {
            return;
        };
        let last = !p.due && p.owed.last().is_some_and(|l| l == item_id);
        let again = heard && p.due && p.owed.iter().any(|id| id == item_id);
        if last || again {
            self.pending_decide();
        }
    }

    /// A response rendered every committed turn: nothing is owed — the turns
    /// it settled are returned. A carried create stays: it waits for a turn
    /// still open.
    pub(super) fn pending_answered(&mut self) -> Vec<String> {
        match self.pending.take() {
            Some(mut p) if p.carried.is_some() => {
                let owed = std::mem::take(&mut p.owed);
                // Answered: no "not resumed" line is due for these turns
                // (B3 review 13).
                p.interrupted = None;
                self.pending = Some(p);
                owed
            }
            Some(p) => p.owed,
            None => Vec::new(),
        }
    }

    /// Whether an automatic response is owed to `item_id`.
    pub(super) fn pending_owes(&self, item_id: &str) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.owed.iter().any(|id| id == item_id))
    }

    /// The active response is over: an owed response that was due starts.
    pub(super) fn pending_after_response(&mut self) {
        if let Some(p) = self.pending.take_if(|p| p.due) {
            self.respond(p);
        }
    }

    /// The response the debt is settled by: the carried `response.create`,
    /// answering the owed turns too, or the automatic one. A carried create
    /// that cannot start after all has had its error, and the owed turns
    /// with words still get their automatic response (WP1c review #2).
    fn respond(&mut self, p: Pending) {
        let Some(mut create) = p.carried else {
            return self.respond_to(p.owed);
        };
        let words = self.has_words(&p.owed);
        create.answers.extend(p.owed.iter().cloned());
        self.start_response(create);
        if self.active.is_none() && words {
            self.respond_to(p.owed);
        }
    }

    /// The automatic response to the `owed` turns, as a client's
    /// `response.create` would start it.
    fn respond_to(&mut self, owed: Vec<String>) {
        self.start_response(Create {
            event_id: None,
            params: None,
            answers: owed,
            auto: true,
        });
    }

    /// Whether any of the `owed` turns has words — a turn the chat model
    /// hears counts until its transcript is in.
    fn has_words(&self, owed: &[String]) -> bool {
        let hearing = self.hearing();
        owed.iter().any(|id| {
            self.conversation
                .get(id)
                .is_some_and(|i| has_words(i, hearing))
        })
    }

    fn pending_decide(&mut self) {
        let Some(p) = self.pending.as_ref() else {
            return;
        };
        let owed = p.owed.clone();
        let carried = p.carried.is_some();
        if !carried && !self.has_words(&owed) {
            let interrupted = p.interrupted.clone().filter(|_| !owed.is_empty());
            if let Some(rid) = interrupted {
                tracing::info!(
                    "realtime {}: the turn that interrupted response {rid} has no words; the \
                     interrupted answer is not resumed (OpenAI's behaviour, §4.3)",
                    self.id()
                );
            }
            if !owed.is_empty() {
                tracing::info!(
                    "realtime {}: {owed:?} has no words (noise, or a failed transcription) — no \
                     response",
                    self.id()
                );
            }
            self.pending = None;
            // No response is these turns': none is timed from them either.
            self.timing_unanswered(&owed);
            return;
        }
        match self.active.as_ref().map(|a| (a.phase, a.output.id.clone())) {
            None => {
                if let Some(p) = self.pending.take() {
                    self.respond(p);
                }
            }
            // It renders these turns when it starts its call, and answers
            // them: they are what it waits for now as well (module doc).
            Some((phase, _)) if !phase.launched() && !carried => {
                self.pending = None;
                if let Some(active) = self.active.as_mut() {
                    for id in owed {
                        if !active.awaited.contains(&id) {
                            active.awaited.push(id.clone());
                        }
                        if !active.answers.contains(&id) {
                            active.answers.push(id);
                        }
                    }
                }
            }
            Some((_, rid)) => {
                tracing::info!(
                    "realtime {}: the response to {owed:?} starts after response {rid}, which \
                     was rendered before them",
                    self.id()
                );
                if let Some(p) = &mut self.pending {
                    p.due = true;
                }
            }
        }
    }
}
