//! One assistant turn of a Chat thread (chat-complete design §3): everything
//! from reading the history to handing the request to a worker, and the
//! worker's one persist step at the end.
//!
//! `send`, an edited user message, a regenerate and a continue are all the
//! same turn over a different history: [`start_turn`] reads the thread as it
//! now stands, builds the request, and spawns the plain stream
//! ([`super::chat`]'s `run_send`) or the tool loop
//! ([`super::agentchat::run_send`]). The only difference the workers see is
//! [`Turn`] — whether the reply is a new row or the continuation of the
//! trailing one.
//!
//! A turn is the thread's one live turn ([`super::chat_live`]): starting it
//! cancels the one before, its waits before the first token give way the
//! moment it is stopped or replaced, and its reply is saved only onto the
//! history it answered (review R1 finding 1).

use std::collections::HashMap;
use std::future::Future;

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use tokio::sync::mpsc;

use super::agentchat::{self, ToolPlan, ADMIN_KIND};
use super::chat::{err_json, run_send};
use super::chat_attach_gate::Caps;
use super::chat_caller::Caller;
use super::chat_live::Ticket;
use super::chat_repo::ChatRepo;
use super::chat_voice::ReadAloud;
use super::{chat_attach, chat_knowledge, chat_reasoning, chat_sampling};
use crate::config::{Protocol, Route, Snapshot};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, ContentPart, Message, Params, ReasoningControl, Role};
use crate::state::{AppState, SharedState};
use crate::store::{ChatAttachmentFull, ChatMessageRow, ChatReply, ChatThread, ContinueSave};

mod blind;
pub(super) use blind::Blind;
mod merge;
mod out;
pub(crate) mod resume;
mod spoken;
pub(crate) use out::{refuse, refuse_sent, SentAs, TurnFrame, TurnLanguage, TurnOpts, VoiceTurn};
pub(crate) use spoken::{
    row_text, tools_may_run, RowWatch, ToolsHeld, UserRow, AUDIO_NOT_HEARD, NOT_TRANSCRIBED,
};
pub use spoken::{spoken_turn_for_tests, spoken_turn_held_for_tests, HeldTurnForTests};
#[cfg(test)]
mod seam_tests;

/// Where a turn's frames go: its caller's channel ([`out`]), read along by
/// the change feed ([`events`]).
mod events;
pub(crate) use events::Events;

/// What a turn answers.
#[derive(Debug, Clone, Copy)]
pub(super) enum TurnMode {
    /// A new reply to the history as it stands. `user_message_id` is the user
    /// message this turn answers when the caller just wrote or rewrote it
    /// (send, edit, regenerate on a user message): the stream opens with a
    /// `turn {user_message_id}` event so the page's optimistic bubble learns
    /// its id. `None` for a regenerate of a reply.
    Fresh { user_message_id: Option<i64> },
    /// The model continues the thread's trailing reply `message_id`
    /// (assistant prefill): the stream's deltas are the continuation only,
    /// and the persist step appends them to that row.
    Continue { message_id: i64 },
    /// The thread's trailing reply `message_id` stopped on calls that were
    /// decided since (client-apps design §6.3): the tool loop settles them
    /// first ([`TurnOpts::resume`]), and the persist step appends what the
    /// turn adds to that row ([`resume`]).
    Resume { message_id: i64 },
    /// A fresh reply to MCP task results nothing answered yet, with no new
    /// user message (`POST …/answer`, MCP Tasks design §3.3): it starts
    /// only on an idle thread — never cancelling the turn that runs — and,
    /// once its start delivered what waits, only on a history that still
    /// ends in an unanswered result; refused otherwise, as the route
    /// refuses it.
    Answer,
}

/// Where a turn's reply goes when its stream ends — carried by the worker.
pub(super) struct Turn {
    pub repo: ChatRepo,
    pub thread_id: i64,
    persist: Persist,
    /// The alias this turn asked for (the thread's model when it started):
    /// the saved reply's `model`.
    model: String,
    /// This turn's hold on its thread: cancelled by a newer turn, and the
    /// proof its history has not moved when the reply is saved.
    ticket: Ticket,
    /// The caller's own stop ([`TurnOpts::stop`]).
    stop: Option<crate::proxy::StopSignal>,
    /// The request carries the user's speech as audio: it goes only to a
    /// model that takes it ([`spoken::may_hear`]).
    hears: bool,
    /// A heard turn's new turns that went as their transcript beside the
    /// audio: what a tool loop takes out again when a later call goes as
    /// text (`agentchat::unheard`, review V2). Text only, never audio.
    spoken_texts: Vec<String>,
    /// The pre-save barrier ([`TurnOpts::user_row`]).
    user_row: Option<RowWatch>,
    /// What the turn sends a fallback that cannot see ([`blind`]): set once
    /// its request is built.
    blind: std::sync::Arc<blind::Blind>,
    /// What the turn's content lost to the thread's own model, which lacks
    /// a capability — attachments as notes or transcripts, a heard turn as
    /// its transcript — for its request rows (`request_logs.degraded`).
    degraded: Option<String>,
    /// Who the turn runs as ([`TurnOpts::caller`]): every model call it
    /// makes is checked against and charged to a device's key, and its
    /// tools resolve under that key's scope (client-apps design L4).
    caller: Caller,
    /// The turn in the change feed (client-apps design §2.2): live from its
    /// start, `turn.done` with what it saved when the worker drops it.
    feed: crate::web::chat_feed::TurnWatch,
    /// A device's concurrency slot ([`TurnOpts::slot`]), released when the
    /// turn ends.
    _slot: Option<crate::policy::ConcurrencyGuard>,
    /// A resumed turn's decided calls, taken by the tool loop
    /// ([`Self::take_resume`]).
    resume: Vec<crate::agent::DecidedCall>,
}

/// Why a turn gave up before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stopped {
    /// A newer turn of the thread started, or the thread went away.
    Superseded,
    /// The page stopped reading: Stop, a reload, a closed tab.
    ClientGone,
    /// The caller raised its own stop ([`TurnOpts::stop`]). The partial
    /// reply is saved as for [`Self::ClientGone`], and the caller, which is
    /// still reading, gets `done`.
    Interrupted,
}

enum Persist {
    /// A new assistant row.
    Insert,
    /// Appended to a reply whose calls were decided ([`resume`]).
    Resume(Box<resume::Prior>),
    /// Appended to the row being continued: `content` is its text as the
    /// model was shown it (trailing whitespace trimmed), `reasoning` its trace.
    Append {
        message_id: i64,
        content: String,
        reasoning: String,
    },
}

/// What a worker has to persist once its stream ended (partial or not).
pub(super) struct Reply<'a> {
    pub text: &'a str,
    pub reasoning: &'a str,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// The tool loop's record of the turn, `None` for a plain reply.
    pub ir_messages: Option<&'a str>,
    /// The alias that answered instead of the thread's model, when one did
    /// ([`answered_by`]).
    pub answered_by: Option<String>,
    /// The turn was stopped before its end. A stopped fresh reply that
    /// produced nothing is not saved — it would be an empty bubble.
    pub stopped: bool,
    /// The turn failed before its end (the upstream refused, the stream
    /// broke). A failed fresh reply that produced nothing is not saved
    /// either: the empty row would be replayed, and would keep the next send
    /// from merging with the user message it leaves owed (§7.4).
    pub failed: bool,
    /// The calls the turn stopped on for an approval (client-apps design
    /// §6.2), `None` when it did not.
    pub pending: Option<crate::store::PendingApprovals>,
}

/// What [`Turn::persist`] came to: the saved row's id (`0`: nothing was
/// saved) and whether a reply that had something to say was *refused* — the
/// history moved, the row was edited meanwhile, or the write failed. The
/// page is told either way (`done.saved`), and a refusal also gets an
/// `error` event, so a reply that is not stored never looks like one that is.
#[derive(Debug, Clone, Copy)]
pub(super) struct Persisted {
    pub id: i64,
    pub refused: bool,
}

impl Persisted {
    pub fn saved(self) -> bool {
        self.id != 0
    }
}

/// The `error` event's text when a reply's save was refused.
pub(super) const NOT_SAVED: &str =
    "this reply was not saved: the conversation changed while it was being written";

impl Turn {
    /// The alias this turn asked for — the `done` event's `model`.
    pub(super) fn model(&self) -> &str {
        &self.model
    }

    /// Who the turn runs as (client-apps design L4).
    pub(super) fn caller(&self) -> &Caller {
        &self.caller
    }

    /// Whom its rows are charged to: the caller's key, or nobody for the
    /// owner (`internal:chat`).
    pub(super) fn key(&self) -> crate::proxy::KeyRef {
        self.caller.key()
    }

    /// This turn's hold on its thread.
    pub(super) fn ticket(&self) -> &Ticket {
        &self.ticket
    }

    /// The request carries the user's speech as audio (voice-audio-input
    /// design §3.4): only a model that takes it may get it.
    pub(super) fn hears(&self) -> bool {
        self.hears
    }

    /// The spoken parts that went as text ([`Self::spoken_texts`] field).
    pub(super) fn spoken_texts(&self) -> &[String] {
        &self.spoken_texts
    }

    /// What the turn sends a fallback that cannot see, and what its reply
    /// says of it ([`blind`]).
    pub(super) fn blind(&self) -> &std::sync::Arc<blind::Blind> {
        &self.blind
    }

    /// The turn, its request built with `text_form` (a PDF sent as page
    /// images, as its text) beside it, and `lacked`, what its attachments
    /// lost to the thread's model.
    fn with_request(self, text_form: Option<Vec<Message>>, lacked: Option<String>) -> Self {
        Self {
            blind: std::sync::Arc::new(blind::Blind::new(text_form)),
            degraded: crate::degraded::join([self.degraded.clone(), lacked]),
            ..self
        }
    }

    /// What the turn's content lost to the thread's own model before any
    /// route was taken ([`Self::degraded`] field): its row's marker, joined
    /// with what a send's route took off ([`blind::Blind::marker`]).
    pub(super) fn degraded(&self) -> Option<String> {
        self.degraded.clone()
    }

    /// A heard voice turn's pre-save barrier, which its tools wait on too
    /// (`spoken::tools_may_run`).
    pub(super) fn user_row(&self) -> Option<RowWatch> {
        self.user_row.clone()
    }

    /// A continue, which only a route that takes a prefill can serve.
    pub(super) fn is_continue(&self) -> bool {
        matches!(self.persist, Persist::Append { .. })
    }

    /// A turn resumed after its calls were decided.
    pub(super) fn is_resume(&self) -> bool {
        matches!(self.persist, Persist::Resume(_))
    }

    /// A resumed turn's decided calls, once (empty for any other turn).
    pub(super) fn take_resume(&mut self) -> Vec<crate::agent::DecidedCall> {
        std::mem::take(&mut self.resume)
    }

    /// A resumed turn that stopped before its tool loop took its decided
    /// calls: they never ran, and are closed saying so
    /// (`chat_approvals::unrun`). Nothing for any other turn.
    pub(super) async fn unrun(&self, state: &AppState) {
        let Persist::Resume(prior) = &self.persist else {
            return;
        };
        if self.resume.is_empty() {
            return;
        }
        let r = resume::Resume {
            message_id: prior.message_id,
            decided: self.resume.clone(),
        };
        super::chat_approvals::unrun(state, self.repo, self.thread_id, &r).await;
    }

    /// The key of the principal the turn runs as, and its name: what a
    /// gated turn stores for the turn that resumes it (L13).
    pub(super) fn starter(&self) -> (Option<i64>, Option<String>) {
        if let Persist::Resume(prior) = &self.persist {
            return (prior.pending.key_id, prior.pending.key_name.clone());
        }
        let ctx = self.caller.ctx();
        let id = self.caller.key_id();
        (id, id.and(ctx.client_key.clone()))
    }

    /// The reply text a continue was shown (its prefill) — `None` for a
    /// fresh reply.
    pub(super) fn continued_text(&self) -> Option<&str> {
        match &self.persist {
            Persist::Append { content, .. } => Some(content),
            Persist::Insert | Persist::Resume(_) => None,
        }
    }

    /// Resolves when this turn should stop: a newer turn of the thread
    /// started (or the thread went away), the page stopped reading `tx`, or
    /// the caller raised its stop.
    pub(super) async fn stopped(&self, tx: &Events) -> Stopped {
        tokio::select! {
            biased;
            () = self.ticket.superseded() => Stopped::Superseded,
            () = tx.closed() => Stopped::ClientGone,
            () = crate::proxy::stopped(self.stop.as_ref()) => Stopped::Interrupted,
        }
    }

    /// Await `fut` unless the turn is stopped first — then `fut` is
    /// *dropped*, and whatever it held (a GPU admission, an upstream
    /// request, a retrieval) goes with it.
    pub(super) async fn or_stop<T>(
        &self,
        tx: &Events,
        fut: impl Future<Output = T>,
    ) -> Result<T, Stopped> {
        tokio::select! {
            biased;
            why = self.stopped(tx) => Err(why),
            v = fut => Ok(v),
        }
    }

    /// Say why the turn ended before anything was saved, to a reader that
    /// may still be there: a superseded turn's page (another tab, when a
    /// newer turn replaced this one) gets an `error` and `done {aborted}`; a
    /// caller that raised its own stop, `done {aborted}` alone — no
    /// `message_id`, which says nothing was saved. Nothing to say to a page
    /// that left.
    pub(super) async fn report_stop(&self, why: Stopped, tx: &Events) {
        match why {
            Stopped::ClientGone => return,
            Stopped::Interrupted => {}
            Stopped::Superseded => {
                let msg = "a newer turn of this thread started (or the thread went away), so \
                           this reply was stopped and is not saved";
                let data = json!({ "message": msg, "code": "superseded" }).to_string();
                let _ = tx.send(TurnFrame::new("error", data)).await;
            }
        }
        let done = json!({ "aborted": true }).to_string();
        let _ = tx.send(TurnFrame::new("done", done)).await;
    }

    /// The persist step: insert the reply as a new row, or append it to the
    /// continued one — its text and trace grow, its token counts and model
    /// become this call's. The row's id for the `done` event (`0` when
    /// nothing was saved) and whether a save was refused:
    ///
    /// - the thread's history moved since the turn started (an edit, a
    ///   delete, a newer turn — review R1 finding 1): the reply answers a
    ///   history that is gone and is dropped, and logged;
    /// - a continued row's text is no longer what the model was shown;
    /// - a stopped or failed fresh reply that produced nothing;
    /// - the write failed (the thread is gone, or the DB refused), logged.
    pub(super) async fn persist(&self, state: &AppState, r: Reply<'_>) -> Persisted {
        let p = self.persist_reply(state, r).await;
        self.feed.persisted(p.id);
        p
    }

    async fn persist_reply(&self, state: &AppState, r: Reply<'_>) -> Persisted {
        let nothing = r.text.is_empty() && r.reasoning.is_empty() && r.ir_messages.is_none();
        if nothing && (r.stopped || r.failed || self.is_continue() || self.is_resume()) {
            // Nothing came: a stopped or failed reply is not an empty
            // bubble, and a continue that added nothing leaves its row as it
            // was (and still names it: `done` says `saved` for that row,
            // which is unchanged, not a partial reply).
            let id = match &self.persist {
                Persist::Append { message_id, .. } => *message_id,
                Persist::Resume(prior) => prior.message_id,
                Persist::Insert => 0,
            };
            return Persisted { id, refused: false };
        }
        // A heard response's reply follows its user row (voice-audio-input
        // design §3.4, `spoken::save_after`).
        if let Some(row) = &self.user_row {
            let superseded = self.ticket.superseded();
            let saved = spoken::save_after(row.clone(), nothing, superseded, self.thread_id);
            if let Some(refused) = saved.await {
                return Persisted { id: 0, refused };
            }
        }
        let Some(proof) = self.ticket.save_lock().await else {
            tracing::warn!(
                thread = self.thread_id,
                "chat: the thread changed while this reply was being written (an edit, a delete \
                 or a newer turn); the reply was not saved"
            );
            return Persisted {
                id: 0,
                refused: true,
            };
        };
        let mut reply = ChatReply {
            content: r.text.to_string(),
            reasoning: r.reasoning.to_string(),
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            ir_messages: r.ir_messages.map(str::to_string),
            model: Some(self.model.clone()),
            answered_by: r.answered_by,
            // What a fallback that cannot see was sent in the images' place,
            // when one answered (`blind`).
            images_note: self.blind.note(),
            voice: None,
            pending_approvals: r.pending.clone(),
        };
        let res = match &self.persist {
            Persist::Insert => {
                self.repo
                    .save_reply(state, &proof, &self.caller, self.thread_id, &reply)
                    .await
            }
            Persist::Resume(prior) => {
                let reply = prior.merged(reply, r.pending);
                self.repo
                    .save_resume(
                        state,
                        &proof,
                        &self.caller,
                        self.thread_id,
                        prior.message_id,
                        &reply,
                    )
                    .await
                    .map(|saved| if saved { prior.message_id } else { 0 })
            }
            Persist::Append {
                message_id,
                content,
                reasoning,
            } => {
                reply.content = format!("{content}{}", r.text);
                reply.reasoning = format!("{reasoning}{}", r.reasoning);
                self.repo
                    .save_continue(
                        state,
                        &proof,
                        &self.caller,
                        self.thread_id,
                        *message_id,
                        content,
                        &reply,
                    )
                    .await
                    .map(|saved| match saved {
                        ContinueSave::Saved => *message_id,
                        ContinueSave::Changed => {
                            tracing::warn!(
                                thread = self.thread_id,
                                message = message_id,
                                "chat: the continued reply was edited meanwhile; the \
                                 continuation was not saved"
                            );
                            0
                        }
                        ContinueSave::Gone => 0,
                    })
            }
        };
        let id = res.unwrap_or_else(|e| {
            tracing::warn!(
                thread = self.thread_id,
                "chat: the reply was not saved: {e}"
            );
            0
        });
        Persisted {
            id,
            refused: id == 0,
        }
    }
}

/// Who answered a turn in place of the model it asked for, from the gate's
/// headers for the route that answered: a GPU-hold, outside-VRAM or ladder
/// fallback's alias, or a candidate alias's pick (by its public name).
/// `None` when the model asked for answered itself.
/// The model that answers on a route the gate opened with `headers`, for
/// the capability check (`capabilities::hears::Model`): `answered_by` (this
/// turn's name for it, else `alias`), and a candidate's pick read from its
/// row, so an alias of the same name never stands in for it (review V8).
pub(super) fn answering<'a>(
    answered_by: Option<&'a str>,
    headers: &crate::gate::GateHeaders,
    alias: &'a str,
) -> crate::capabilities::hears::Model<'a> {
    use crate::capabilities::hears::Model;
    match answered_by {
        Some(name) if headers.fallback().is_none() && headers.candidate().is_some() => {
            Model::Pick(name)
        }
        Some(name) => Model::Named(name),
        None => Model::Named(alias),
    }
}

pub(super) fn answered_by(snap: &Snapshot, headers: &crate::gate::GateHeaders) -> Option<String> {
    headers
        .fallback()
        .map(str::to_string)
        .or_else(|| headers.candidate().map(|m| snap.local_public_name(m)))
}

/// Start one turn and answer with its frames as SSE ([`start_turn_into`],
/// [`out::sse`]): `turn` / `retrieval` / `delta` / `reasoning` / `tool` /
/// `usage` / `stats` / `stop` / `error` / `done`. With `speak`, the reply is
/// also read aloud as it streams, its speech frames interleaved (the speech
/// tee, chat-voice design §6.4). The turn runs as `caller` ([`Turn::caller`]).
pub(super) async fn start_turn(
    state: &SharedState,
    caller: &Caller,
    repo: ChatRepo,
    thread: &ChatThread,
    mode: TurnMode,
    caps: Caps,
    speak: Option<ReadAloud>,
) -> Response {
    let turn = TurnAs {
        caller: caller.clone(),
        resume: None,
        slot: None,
        sent: None,
    };
    start_turn_as(state, caller, turn, repo, thread, (mode, caps), speak).await
}

/// Who a turn runs as when that is not its request's caller, and what it
/// resumes: a resumed turn runs as its starter (client-apps design L13),
/// while its frames stream to the approver's request.
pub(super) struct TurnAs {
    pub caller: Caller,
    pub resume: Option<resume::Resume>,
    /// `Some`: the concurrency slot was settled before — a resume takes its
    /// starter's before anything is decided (`chat_approvals::approve`) —
    /// and is this (`None` inside: none to hold). `None`: taken here.
    pub slot: Option<Option<crate::policy::ConcurrencyGuard>>,
    /// A send's ([`TurnOpts::sent`]).
    pub sent: Option<super::chat_tasks::deliver::Sent>,
}

/// [`start_turn`] for a turn that runs as `turn.caller`, its frames
/// streamed to `caller`'s request (and a read-aloud charged to it).
pub(super) async fn start_turn_as(
    state: &SharedState,
    caller: &Caller,
    turn: TurnAs,
    repo: ChatRepo,
    thread: &ChatThread,
    (mode, caps): (TurnMode, Caps),
    speak: Option<ReadAloud>,
) -> Response {
    let TurnAs {
        caller: runs_as,
        resume,
        slot: settled,
        sent,
    } = turn;
    let started = std::time::Instant::now();
    let (tx, rx) = mpsc::channel::<TurnFrame>(64);
    // A reply read aloud as it streams is heard: in the thread's reply
    // language, when it has one — once its read-aloud says its speech plan stands
    // (chat-voice design §8.5).
    let language = speak
        .is_some()
        .then(|| super::chat_voice::turn_language(&state.snapshot(), thread, true))
        .flatten();
    let (planned, heard) = match language {
        Some(_) => {
            let (planned, heard) = tokio::sync::oneshot::channel();
            (Some(planned), Some(heard))
        }
        None => (None, None),
    };
    // A device's turn takes one of its key's concurrent-request slots for
    // its length (review W3-8); a refusal is the key's own, before anything
    // starts.
    let slot = match settled {
        Some(slot) => slot,
        None => match runs_as
            .turn_slot(
                state,
                crate::ingress::ClientProto::Chat,
                &thread.model_alias,
            )
            .await
        {
            Ok(slot) => slot,
            Err(e) => return err_json(e.http_status(), e.code(), e.to_string()),
        },
    };
    let opts = TurnOpts {
        language,
        heard,
        caller: runs_as,
        slot,
        resume,
        sent,
        ..TurnOpts::default()
    };
    match start_turn_into(state, repo, thread, mode, caps, tx, opts).await {
        Ok(()) => match speak {
            Some(read) => super::chat_voice::speaking_turn(
                state,
                caller,
                repo,
                thread,
                rx,
                (started, planned),
                read,
            ),
            None => out::sse(state, caller, rx),
        },
        Err(refused) => refused,
    }
}

/// Start one turn: read the thread's history, spawn the worker, and write
/// its frames into `out`. `out` must have room for the opening `turn`
/// frame: it is written before this returns, and a full channel is refused
/// rather than waited on, since a caller that reads only once this returned
/// would wait forever. The worker first runs an auto-mode knowledge retrieval when the
/// turn has one ([`chat_knowledge`]; after the `turn` frame, so the reader
/// learns its message id while the search runs), then builds the request
/// with the thread's current settings. `caps` is
/// [`thread_caps`](super::chat_attach_gate::thread_caps)'s, which the caller
/// already needed for its own checks. `Err`: the turn was refused before it
/// started (a continue whose reply is no longer the last message), as the
/// response to give. A resumed turn refused so closes its decided calls as
/// not run (`chat_approvals::unrun`): they never will.
pub(crate) async fn start_turn_into(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
    mode: TurnMode,
    caps: Caps,
    out: mpsc::Sender<TurnFrame>,
    opts: TurnOpts,
) -> Result<(), Response> {
    let resumed = opts.resume.clone();
    let started = start_turn_inner(state, repo, thread, mode, caps, out, opts).await;
    if let (Err(_), Some(r)) = (&started, &resumed) {
        super::chat_approvals::unrun(state, repo, thread.id, r).await;
    }
    started
}

/// `thread`'s approval floor as its turn reads it
/// ([`crate::mcp::exec::ApprovalFloor`]): its own, and in a folder the
/// folder's floor and defaults. A folder gone since is none.
async fn approval_floor(
    state: &SharedState,
    thread: &ChatThread,
) -> Result<crate::mcp::exec::ApprovalFloor, Response> {
    let folder = match thread.folder_id {
        Some(id) => crate::store::get_chat_folder(&state.db, id)
            .await
            .map_err(|e| {
                err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    format!("the folder's approval rules could not be read: {e}"),
                )
            })?
            .map(|f| {
                let mut rules = f.approval_floor;
                rules.extend(f.defaults.mcp_tools.unwrap_or_default());
                rules
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Ok(crate::mcp::exec::ApprovalFloor {
        thread: thread.approval_floor.clone(),
        folder,
    })
}

async fn start_turn_inner(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
    mode: TurnMode,
    caps: Caps,
    out: mpsc::Sender<TurnFrame>,
    opts: TurnOpts,
) -> Result<(), Response> {
    let continue_gone = || {
        err_json(
            StatusCode::CONFLICT,
            "continue_unavailable",
            "the reply to continue is no longer the thread's last message",
        )
    };
    if let TurnMode::Continue { message_id } = mode {
        // Checked before the turn takes the thread, so a refused continue
        // does not cancel a turn that is still answering.
        let last = repo.last_message(state, thread.id).await.ok().flatten();
        if !last.is_some_and(|m| m.id == message_id && m.role == "assistant") {
            return Err(continue_gone());
        }
    }
    if let TurnMode::Resume { message_id } = mode {
        // The same for a resumed turn: its reply must still be the last.
        let last = repo.last_message(state, thread.id).await.ok().flatten();
        if !last.is_some_and(|m| m.id == message_id && m.role == "assistant") {
            return Err(resume::moved_on());
        }
    }
    // The owner's approval floor the turn's tools run under (client-apps
    // design §6.6), read before the turn takes the thread: a read that
    // fails refuses the turn rather than running it ungated.
    let floor = approval_floor(state, thread).await?;
    // Room for the opening `turn` frame, before the turn takes the thread: a
    // refused call cancels no live turn and wakes no archived thread (WP11
    // server review n6). This caller is the channel's only writer so far.
    if matches!(
        mode,
        TurnMode::Fresh {
            user_message_id: Some(_)
        }
    ) && out.capacity() == 0
    {
        let why = "the turn's channel has no room for its first frame";
        return Err(err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", why));
    }
    // The thread's one live turn from here: the previous one is cancelled,
    // and anything that rewrites the history after this point keeps this
    // turn's reply from being saved onto it. The history is read after. A
    // device's turn is cancelled when the thread leaves its reach (P-8).
    let device = opts
        .caller
        .is_device()
        .then(|| opts.caller.key_id())
        .flatten();
    let out_of_reach = || err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    // The reach was checked when the thread was read. Asked again before
    // the turn takes the thread, so a device whose level dropped since never
    // cancels the turn running there, the owner's perhaps (the branch
    // review's verification, V-10) ...
    if device.is_some() && !opts.caller.sees(&state.snapshot(), thread) {
        return Err(out_of_reach());
    }
    // Once the turn let the thread go — its worker's end, or a refusal from
    // here on — MCP task results that ended while it held it enter the
    // thread (MCP Tasks design §3.1). Taken before the ticket, so it drops
    // after it.
    let deliver = super::chat_tasks::deliver::after_turn(state, thread.id, !repo.is_temp());
    let ticket = match mode {
        TurnMode::Answer => {
            let ticket = state
                .chat_live
                .begin_idle_as(thread.id, device, thread.reach_level())
                .await;
            let Some(ticket) = ticket else {
                return Err(super::chat_tasks::turn_running());
            };
            ticket
        }
        _ => {
            state
                .chat_live
                .begin_as(thread.id, device, thread.reach_level())
                .await
        }
    };
    // ... and after: a level that moved between that check and this
    // registration found no turn to cancel, so it is asked again now that
    // one would be found (review G-6).
    if device.is_some() && !opts.caller.sees(&state.snapshot(), thread) {
        drop(ticket);
        return Err(out_of_reach());
    }
    let TurnOpts {
        stop,
        voice,
        language,
        heard,
        began,
        spoken,
        user_row,
        degraded,
        caller,
        slot,
        resume,
        sent,
    } = opts;
    // Begun: the thread is this turn's now, and a result that ends from
    // here on waits for its end (MCP Tasks design §3.1).
    let sent = match sent {
        Some(hold) => {
            drop(hold);
            true
        }
        None => false,
    };
    if let Some(began) = began {
        let _ = began.send(ticket.generation());
    }
    // Turning an archived thread over restores it (chat-archive design §1).
    let _ = repo.wake(state, thread, &caller).await;
    // MCP task results that ended before this turn enter the history it
    // reads (MCP Tasks design §3.1). A continue or a resumed turn extends
    // the last reply, which must stay last: what waits enters when it ends.
    // A send's entered before its message, and one that ended after the
    // message waits for this turn's end.
    if matches!(mode, TurnMode::Fresh { .. } | TurnMode::Answer) && !repo.is_temp() && !sent {
        // Boxed, as every await this deep in a turn: its future stays off
        // the stack.
        Box::pin(super::chat_tasks::deliver::at_start(
            state, &ticket, thread.id,
        ))
        .await;
    }
    let mut history = repo.messages(state, thread.id).await.unwrap_or_default();

    let (user_message_id, persist) = match mode {
        TurnMode::Fresh { user_message_id } => (user_message_id, Persist::Insert),
        // Asked again under the ticket: a turn that ended between the
        // route's check and this start may have answered the results.
        TurnMode::Answer if !super::chat_tasks::render::unanswered(&history) => {
            return Err(super::chat_tasks::nothing_to_answer());
        }
        TurnMode::Answer => (None, Persist::Insert),
        TurnMode::Resume { message_id } => match resume::prior(&history, message_id) {
            Some(prior) => (None, Persist::Resume(Box::new(prior))),
            None => return Err(resume::moved_on()),
        },
        TurnMode::Continue { message_id } => {
            let Some(last) = history
                .last_mut()
                .filter(|m| m.id == message_id && m.role == "assistant")
            else {
                return Err(continue_gone());
            };
            // The prefill ends where the text does: Anthropic refuses a final
            // assistant turn ending in whitespace, and a model tokenizes the
            // break better when it writes it itself. The row is saved as the
            // model was shown it, plus what it wrote.
            last.content.truncate(last.content.trim_end().len());
            let persist = Persist::Append {
                message_id,
                content: last.content.clone(),
                reasoning: last.reasoning.clone(),
            };
            (None, persist)
        }
    };

    // Every *sent* attachment this thread has ever carried — text attachments
    // need their bytes exactly as much as image ones do, so this is not gated
    // on the thread having an image. Drafts are excluded at the query, and an
    // image's bytes are skipped too when `vision` already says this model
    // cannot see it — that image is about to become a text placeholder.
    let attachments = group_by_message(
        repo.sent_attachments(state, thread.id, caps.vision)
            .await
            .unwrap_or_default(),
    );

    // The knowledge bases this turn uses: a retrieval to run before the
    // model is called (auto mode), or the `kb__*` tools (tool mode).
    let kb = chat_knowledge::plan(
        state.snapshot().settings.chat_kb_budget_tokens,
        thread,
        &history,
        mode,
    );

    // A thread with tools — the self-admin plane, attached MCP servers, its
    // knowledge bases in tool mode, or any mix — needs the agent loop; a bare
    // one keeps the 1:1 stream it has always had, so plain chat gains no
    // tool-loop machinery it never uses.
    let plan = ToolPlan {
        admin: thread.kind == ADMIN_KIND,
        mcp: thread.mcp_tools.clone(),
        kb: kb.tools,
        floor,
    };
    // Live in the change feed from here (client-apps design §2.2): who
    // started it, and whether it is a bound session's voice turn.
    let feed = state.chat_live.feed().turn_started(
        thread.id,
        thread.reach_level(),
        caller.named(),
        voice.is_some(),
    );
    let tx = Events::new(out, feed.observer());
    if let Some(id) = user_message_id {
        let data = json!({ "user_message_id": id }).to_string();
        // Room was checked before `begin`; a closed channel is a reader
        // gone, which the turn hears itself.
        let _ = tx.try_send(TurnFrame::new("turn", data));
    }
    let turn = Turn {
        repo,
        thread_id: thread.id,
        persist,
        model: thread.model_alias.clone(),
        ticket,
        stop,
        hears: spoken::hears(spoken.as_deref()),
        spoken_texts: spoken
            .as_deref()
            .filter(|s| spoken::hears(Some(s)))
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        user_row,
        blind: Default::default(),
        degraded,
        caller,
        feed,
        _slot: slot,
        resume: resume.map(|r| r.decided).unwrap_or_default(),
    };
    let state = state.clone();
    let thread = thread.clone();
    // Held until the turn has saved what it had and written its row: a
    // stopping server waits for it (review F-2), counted with the server
    // its request came in on. Taken before the spawn, so a stop that comes
    // before the task first runs still counts it.
    let running = state
        .stops
        .running_at(state.stops.at_or_now(turn.caller.served_at()));
    tokio::spawn(async move {
        let _running = running;
        // Bound before the turn, so it drops after the turn's ticket.
        let _deliver = deliver;
        let turn = turn;
        // The waits before the request goes out give way the moment the turn
        // is stopped or replaced (review R1 finding 1): nothing is saved and
        // nothing more is started for a reply nobody will read.
        if let Some(auto) = kb.auto {
            let ran = turn
                .or_stop(
                    &tx,
                    chat_knowledge::run_auto(
                        &state,
                        turn.caller(),
                        (repo, thread.id),
                        &mut history,
                        auto,
                        turn.ticket(),
                        &tx,
                    ),
                )
                .await;
            if let Err(why) = ran {
                turn.report_stop(why, &tx).await;
                turn.unrun(&state).await;
                return;
            }
        }
        // Page images and transcripts are made here, on first need; the
        // request itself is built from what came out.
        let render = async {
            let mut rendered = HashMap::with_capacity(attachments.len());
            for (mid, atts) in &attachments {
                let mut parts = Vec::with_capacity(atts.len());
                for att in atts {
                    parts
                        .push(chat_attach::render(&state, turn.caller(), att, caps, &thread).await);
                }
                rendered.insert(*mid, parts);
            }
            rendered
        };
        let rendered = match turn.or_stop(&tx, render).await {
            Ok(r) => r,
            Err(why) => {
                turn.report_stop(why, &tx).await;
                turn.unrun(&state).await;
                return;
            }
        };
        drop(attachments);
        let language = match turn.or_stop(&tx, out::heard(language, heard)).await {
            Ok(l) => l,
            Err(why) => {
                turn.report_stop(why, &tx).await;
                turn.unrun(&state).await;
                return;
            }
        };
        let (ir, text_form) = request(
            &state,
            &thread,
            (&history, spoken.as_deref()),
            &rendered,
            (voice.as_ref(), language.as_ref()),
        );
        let lacked = chat_attach::lacked_marker(&thread.model_alias, rendered.values().flatten());
        let turn = turn.with_request(text_form, lacked);
        // The request holds the spoken parts now; the turn keeps no copy
        // (voice-audio-input design §4).
        drop(spoken);
        // A resumed turn settles its decided calls in the tool loop, even
        // if the thread's tools went meanwhile.
        if plan.is_empty() && !turn.is_resume() {
            run_send(state, turn, ir, tx).await;
        } else {
            agentchat::run_send(state, turn, ir, plan, tx).await;
        }
    });
    Ok(())
}

/// The turn's request: the history as IR with the thread's current settings
/// and its profile's prompt parts (`chat_profile::system_message`) — and,
/// for a voice turn (chat-voice design §8.5), the spoken-style block after
/// the prompt and reasoning off unless the thread or its profile sets it;
/// for a turn with a reply language, the languages
/// (`chat_voice::prompt::turn_block`). `spoken`: a heard response's new
/// turns, after the history (voice-audio-input design §3.4).
///
/// Beside it, when a PDF went as page images, the same messages with each
/// such PDF in its text form (`Rendered::text_form`): what a fallback that
/// cannot see is sent instead ([`blind`]). Merging goes by role alone, so
/// the two lists hold the same messages.
fn request(
    state: &SharedState,
    thread: &ChatThread,
    (history, spoken): (&[ChatMessageRow], Option<&[ContentPart]>),
    attachments: &HashMap<i64, Vec<chat_attach::Rendered>>,
    (voice, language): (Option<&VoiceTurn>, Option<&TurnLanguage>),
) -> (ChatRequest, Option<Vec<Message>>) {
    // The system message — the thread's profile's parts, a voice turn's
    // block, a language — and the reasoning (§8.5; personality-profiles
    // design §2.2).
    let snap = state.snapshot();
    let answering = effective_alias(state, &thread.model_alias);
    let today = chrono::Local::now().date_naive();
    let prompt = super::chat_profile::Prompt::of(&snap, thread, &answering, today);
    let (system, reasoning) =
        super::chat_profile::system_message(&prompt, &snap.settings.realtime, voice, language);
    let messages =
        |text_form: bool| build_messages(&system, (history, spoken), (attachments, text_form));
    let paged = attachments
        .values()
        .flatten()
        .any(|r| r.text_form.is_some());
    let text_form = paged.then(|| messages(true));
    let ir = ChatRequest {
        model_alias: thread.model_alias.clone(),
        messages: messages(false),
        params: Params {
            // temperature plus the thread's other sampling choices; the relay
            // drops what the route cannot take and reports it.
            // No silent cap: forward the user's max_tokens or none (use the
            // model's real remaining budget).
            max_tokens: thread.max_tokens.map(|v| v.max(0) as u32),
            // The thread's overrides, at the tier the `x-lmgw-reasoning*`
            // headers take: the route's defaults fill in around them.
            reasoning,
            ..chat_sampling::params_of(thread)
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    (ir, text_form)
}

// ---------------------------------------------------------------------------
// Vision
// ---------------------------------------------------------------------------

/// The **effective** resolved model's `vision` capability — the one that will
/// actually answer this turn, not necessarily the thread's own alias (review
/// finding 9).
///
/// A thread pointed at a local model that the GPU hold has re-routed to a
/// fallback (gpu-hold design §4, `Snapshot::resolve_for_request`) answers
/// through the fallback, not through `alias` — checking `alias`'s own vision
/// flag gates on a model that will never see the request. `resolve_for_request`
/// already decides the fallback eagerly, at resolve time (the same call
/// `run_send` makes to dispatch the turn), so it is not a "decided later
/// inside dispatch" case: the effective target is knowable right here, before
/// anything is written.
///
/// Resolve failure (unknown alias, or a hold with no usable fallback) falls
/// back to the thread's own alias — the send is doomed either way and
/// `run_send`'s own resolve reports the real error; this only has to pick
/// something sane to gate the *this* request's rendering on.
///
/// The capability lookup itself is [`crate::capabilities::exposed::exposed_entry`]
/// — a single-model resolution, not the whole catalog — which is exactly what
/// `GET /v1/models/{id}` resolves through too, so the two can never disagree
/// about one model (factored out of `capabilities::exposed` for exactly this
/// reason). `None` when unknown, which the render path treats as "send the
/// image, the upstream can say so itself".
///
/// One lookup answers both questions attachments ask of a model: `vision`
/// (`model_vision`) and whether `input_modalities` contains `audio`
/// (`model_audio_input`) — [`Caps`] carries both, because the lookup probes the
/// upstream's catalog and is not worth paying twice.
///
/// *Changed 2026-10-06* (the owner's ruling: a configured fallback is always
/// used, with no exception by content): a hold's fallback that cannot see
/// no longer decides `vision`. The send gives it the images as placeholders
/// (`gate::fallback_images`), and the turn says so; so the thread's own
/// model decides, as it does without the hold, and a draft image is refused
/// only when that model cannot see it. A fallback that sees still lets a
/// thread whose own model cannot send its images.
pub(super) async fn model_caps(state: &SharedState, alias: &str) -> Caps {
    let effective = effective_alias(state, alias);
    let caps = crate::capabilities::exposed::exposed_entry(state, &effective)
        .await
        .and_then(|e| e.capabilities);
    let mut vision = caps.as_ref().and_then(|c| c.vision);
    if vision == Some(false) && effective != alias {
        vision = crate::capabilities::exposed::exposed_entry(state, alias)
            .await
            .and_then(|e| e.capabilities)
            .and_then(|c| c.vision);
    }
    // Audio by the voice turn's predicate (`capabilities::hears`, review
    // V7): the egress must have an audio part and the server must not have
    // said it loaded no audio projector, beside what the model publishes.
    // A name with no static route (a candidate alias) is read as published.
    use crate::capabilities::hears::{self, Hears};
    let route = state
        .snapshot()
        .resolve_for_request(alias)
        .ok()
        .map(|r| r.route);
    let audio = match &route {
        Some(route) => {
            let facts = hears::server_facts(state, None, route);
            match hears::hears_from(caps.as_ref(), route, facts.as_deref()) {
                Hears::Yes => Some(true),
                Hears::No(_) => Some(false),
                Hears::Unknown => None,
            }
        }
        None => caps
            .and_then(|c| c.input_modalities)
            .map(|m| m.iter().any(|x| x == "audio")),
    };
    Caps { vision, audio }
}

/// The alias that will answer a turn sent to `alias`: its GPU-hold fallback
/// when the hold re-routes it (decided at resolve time, as `run_send`'s own
/// resolve decides it), else `alias` itself — also when it does not resolve,
/// since that send fails with its own error either way.
pub(super) fn effective_alias(state: &SharedState, alias: &str) -> String {
    match state.snapshot().resolve_for_request(alias) {
        Ok(resolved) => resolved.fallback.unwrap_or_else(|| alias.to_string()),
        Err(_) => alias.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Continue
// ---------------------------------------------------------------------------

/// Whether the thread's last reply can be continued, and why not — the
/// thread JSON's `continue: {ok, reason}`.
pub(super) use lmgw_api_types::chat::ContinueState;

/// [`ContinueState`] for `thread` whose newest message is `last`: that is a
/// reply, it carries no tool record, and the thread's model resolves
/// ([`Snapshot::resolve`], the thread's own model — a GPU-hold re-route is
/// only known at send time, and refused there) to a route that takes an
/// assistant prefill ([`prefill_refusal`]).
///
/// A **candidate alias** has no one route (`resolve` refuses it by design):
/// which candidate answers is decided per request. Its verdict is its
/// candidates' — every one of them a local chat model, served by
/// llama-server — and says no only when one of them could not take the
/// prefill. Whatever answers at send time (a candidate, or a fallback the
/// hold or a ladder hands it to) is checked again there and refused by name
/// (review R1 finding 3).
pub(super) fn continue_state(
    snap: &Snapshot,
    thread: &ChatThread,
    last: Option<&ChatMessageRow>,
) -> ContinueState {
    // The reasoning a text turn asks for: the thread's, else its profile's
    // (personality-profiles design D7).
    let profile = thread.profile_id.and_then(|id| snap.chat_profile(id));
    let own = Params {
        reasoning: chat_reasoning::control_with(thread, profile.and_then(|p| p.reasoning)),
        ..Default::default()
    };
    let refusal = |route: &Route| {
        let control = own
            .clone()
            .with_defaults(&route.param_defaults)
            .reasoning_control();
        prefill_refusal(route, &control)
    };
    let why = match last {
        None => Some("there is no reply to continue yet".to_string()),
        Some(m) if m.role != "assistant" => {
            Some("the last message is not a reply — only the last reply can be continued".into())
        }
        Some(m) if has_tool_record(m) => {
            Some("this reply ran tools, and a reply with a tool record cannot be continued".into())
        }
        Some(_) => match snap.candidate_alias(&thread.model_alias) {
            Some(ca) => ca.candidates.iter().find_map(|c| {
                refusal(&snap.chat_local_route(c))
                    .map(|why| format!("candidate '{c}' of '{}': {why}", thread.model_alias))
            }),
            None => match snap.resolve(&thread.model_alias) {
                Err(e) => Some(e.to_string()),
                Ok(route) => refusal(&route),
            },
        },
    };
    ContinueState {
        ok: why.is_none(),
        reason: why,
    }
}

/// Whether a reply carries the tool loop's record of its turn. An empty
/// record (`[]`, what a tool thread's plain answer used to store) is none.
pub(super) fn has_tool_record(m: &ChatMessageRow) -> bool {
    m.ir_messages.as_deref().is_some_and(|raw| {
        serde_json::from_str::<Vec<serde_json::Value>>(raw).map_or(true, |v| !v.is_empty())
    })
}

/// Why `route` cannot continue a reply with the reasoning `control` that goes
/// out with it, `None` when it can.
///
/// - **Anthropic** takes a trailing assistant message as a prefill — but not
///   with extended thinking on, which wants the turn to open with a thinking
///   block.
/// - **llama-server** treats a trailing assistant message as the message to
///   continue (`--no-prefill-assistant` switches that off): the template
///   renders everything before it, then its text as the start of the answer,
///   and streams only what it generates.
/// - An OpenAI-compatible **cloud** API and **Gemini** have no prefill: they
///   answer a trailing assistant message with a new message.
pub(super) fn prefill_refusal(route: &Route, control: &ReasoningControl) -> Option<String> {
    let name = &route.upstream.name;
    match route.upstream.protocol {
        Protocol::Anthropic if control.enabled == Some(true) => Some(
            "extended thinking is on for this thread, and Anthropic cannot continue a reply \
             while thinking — switch reasoning off in the thread settings to continue"
                .into(),
        ),
        Protocol::Anthropic | Protocol::LlamaCpp => None,
        Protocol::Openai => Some(format!(
            "the route answering this thread (upstream '{name}') is an OpenAI-compatible API, \
             which cannot continue a reply — Continue needs an Anthropic or llama-server route"
        )),
        Protocol::Gemini => Some(format!(
            "the route answering this thread (upstream '{name}') is Gemini, which cannot \
             continue a reply — Continue needs an Anthropic or llama-server route"
        )),
    }
}

/// What a turn's request needs on the route it goes out on — the one the gate
/// admitted, and again on every route it re-routes the send to before
/// anything was sent (a ladder climb's fallback, a candidate alias's next
/// pick). One helper for the plain stream and the tool loop, so the two
/// cannot disagree (review R1 finding 2); a continue then also needs
/// [`mark_continuation`] on that route.
pub(super) struct RouteFit {
    /// The thread's own sampling choices this route takes (the route's
    /// defaults are merged in by whoever sends).
    pub params: Params,
    /// Whether `params` dropped anything of the request's own.
    pub params_changed: bool,
    /// The thread's reasoning and sampling overrides this route drops.
    pub ignored: Vec<&'static str>,
}

/// [`RouteFit`] for `route`, with `ir.params` the thread's own choices.
/// `continuing`: `ir` ends with the reply being continued — refused by name
/// on a route without a prefill ([`prefill_refusal`]). `hears`: `ir`
/// carries the user's speech as audio — refused with
/// `audio_input_unsupported` unless `answering`, the model that answers on
/// `route` (as a turn names who answered), takes it; `hold` is the claim
/// the send goes out on, whose server's `/props` is read (voice-audio-input
/// design §3.4, changed 2026-10-06). A fallback that cannot see the
/// request's images is not refused: the turn's request is given to it in
/// the form it takes first ([`Turn::blind`]), and this fits that.
pub(super) async fn fit_route(
    state: &SharedState,
    (route, hold): (&Route, Option<&crate::vram::LocalHold>),
    answering: crate::capabilities::hears::Model<'_>,
    ir: &ChatRequest,
    (continuing, hears): (bool, bool),
) -> Result<RouteFit, GatewayError> {
    if hears {
        spoken::may_hear(state, route, hold, answering).await?;
    }
    let mut ignored = chat_reasoning::ignored(ir, route);
    let (params, sampling_ignored) = chat_sampling::split(&ir.params, route);
    let params_changed = !sampling_ignored.is_empty();
    ignored.extend(sampling_ignored);
    if continuing {
        let control = params
            .clone()
            .with_defaults(&route.param_defaults)
            .reasoning_control();
        if let Some(why) = prefill_refusal(route, &control) {
            return Err(GatewayError::BadRequest(format!(
                "cannot continue this reply: {why}"
            )));
        }
    }
    Ok(RouteFit {
        params,
        params_changed,
        ignored,
    })
}

/// Say so when a continue goes to llama-server: `continue_final_message:
/// true` with `add_generation_prompt: false`, the explicit (vLLM /
/// transformers) form of what llama-server otherwise infers from the trailing
/// assistant message. The prompt is the same either way, but only the
/// explicit form tells llama-server's parser the answer began before the
/// generation (`is_continuation`), so the prefill is not streamed back as the
/// first delta — and appended to the reply a second time. A build that does
/// not know the fields ignores them and prefills as before. An alias's own
/// value for either is left alone. Returns the keys it inserted, for the
/// caller to take out again should another route answer instead; nothing for
/// any other route, or a request that does not end with a reply.
pub(super) fn mark_continuation(route: &Route, ir: &mut ChatRequest) -> Vec<&'static str> {
    let llama = route.upstream.protocol == Protocol::LlamaCpp;
    let ends_with_reply = ir
        .messages
        .last()
        .is_some_and(|m| m.role == Role::Assistant);
    if !llama || !ends_with_reply {
        return Vec::new();
    }
    let mut added = Vec::new();
    for (key, value) in [
        ("continue_final_message", true),
        ("add_generation_prompt", false),
    ] {
        if !ir.passthrough.contains_key(key) {
            ir.passthrough
                .insert(key.to_string(), serde_json::Value::Bool(value));
            added.push(key);
        }
    }
    added
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// Group a thread's attachments by the message that owns them, keeping the
/// order each message sent them in (the query is already `ORDER BY ord, id`) —
/// drafts (`message_id == None`) are dropped, since only a sent message
/// renders into a request.
fn group_by_message(atts: Vec<ChatAttachmentFull>) -> HashMap<i64, Vec<ChatAttachmentFull>> {
    let mut by_message: HashMap<i64, Vec<ChatAttachmentFull>> = HashMap::new();
    for a in atts {
        if let Some(mid) = a.message_id {
            by_message.entry(mid).or_default().push(a);
        }
    }
    by_message
}

/// Build the IR message list: an optional system prompt (its `{{model}}` and
/// `{{date}}` filled in — `{{model}}` with `answering`, the alias that will
/// answer, which under a GPU hold is the fallback's), then the stored turns.
///
/// An agentic turn (Admin Chat) is replayed from its stored IR rather than its
/// rendered text: it is assistant tool calls and their results interleaved, and
/// flattening that to a string would make the next turn forget every tool it
/// just ran — the model would re-run them, or answer from nothing.
///
/// `attachments` is every sent attachment already rendered for the answering
/// model ([`chat_attach::render`]), grouped by the message id it was bound to
/// (design §2). *New* attachments were gated by the caller (`send`); one the
/// model cannot take that reaches here is already in history, and came out of
/// the render as a note rather than a refusal.
///
/// Adjacent user messages go out as one ([`merge`]); a late MCP task
/// result goes as its synthetic call and result where it is stored, its
/// call joined to a directly preceding assistant message
/// (`chat_tasks::render`). `system` is the whole system message (`chat_profile::system_message`: the prompt, a voice
/// turn's block, the admin wrapper), none when empty. A heard
/// response's `spoken` parts follow the history as a user message, and a
/// spoken row with no words says so ([`spoken`]). With `text_form`, a PDF
/// rendered as page images goes as its text form instead
/// (`Rendered::text_form`, [`blind`]).
pub(in crate::web) fn build_messages(
    system: &str,
    (history, spoken): (&[ChatMessageRow], Option<&[ContentPart]>),
    (attachments, text_form): (&HashMap<i64, Vec<chat_attach::Rendered>>, bool),
) -> Vec<Message> {
    let mut msgs = merge::Messages::with_capacity(history.len() + 2);
    if !system.is_empty() {
        msgs.push(Message::text(Role::System, system.to_string()), None);
    }
    // Late MCP task results where they are stored, in chronological order
    // (MCP Tasks design §3.2).
    for m in history {
        if m.is_task_result() {
            msgs.extend_joined(super::chat_tasks::render::pair_of(m).unwrap_or_default());
            continue;
        }
        if let Some(raw) = &m.ir_messages {
            if let Ok(turn) = serde_json::from_str::<Vec<Message>>(raw) {
                // The record stops short of the final answer — the tool loop
                // keeps that in `content` alone (`agentchat`'s
                // `trim_trailing_text`) — so it follows the record here, or
                // the model would never see what it answered.
                let answer = final_answer(&turn, &m.content);
                msgs.extend(turn);
                if m.role == "assistant" && !answer.is_empty() {
                    msgs.push(Message::text(Role::Assistant, answer.to_string()), None);
                }
                continue;
            }
        }
        let role = match m.role.as_str() {
            "assistant" => Role::Assistant,
            "system" => Role::System,
            _ => Role::User,
        };
        // A message's parts: its attachments in the order they were sent in,
        // then its reasoning trace (assistant only), then its typed text
        // (design §2).
        let mut content = Vec::with_capacity(2);
        let mut had_attachment_parts = false;
        if let Some(atts) = attachments.get(&m.id) {
            for r in atts {
                had_attachment_parts = true;
                let parts = match &r.text_form {
                    Some(text) if text_form => text,
                    _ => &r.parts,
                };
                content.extend(parts.iter().cloned());
            }
        }
        // A plain assistant turn keeps its trace in its own column; replay it
        // ahead of the answer so a reasoning model behind
        // `--reasoning-preserve` sees its earlier thinking here too, not only
        // through the API.
        if role == Role::Assistant && !m.reasoning.is_empty() {
            content.push(ContentPart::reasoning(m.reasoning.clone()));
        }
        // A user message's knowledge retrieval (chat-complete design §9.3):
        // its `<context>` block ahead of the typed text, rendered from the
        // stored context — the same bytes on every turn that replays it.
        let mut context_at = None;
        if let Some(block) = m.context.as_ref().and_then(chat_knowledge::context_part) {
            context_at = Some(content.len());
            content.push(block);
        }
        // The typed text — except an empty one tacked onto a message whose
        // attachments already carry its content ("content may be empty when
        // attachments are present", design §2). Every other empty-content
        // case (a reasoning-only turn, say) keeps its always-present text
        // part exactly as before attachments existed.
        let text = spoken::row_text(m);
        if !text.is_empty() || !had_attachment_parts {
            content.push(ContentPart::text(text));
        }
        // Two user messages in a row (a send that saved no reply, then
        // another) go out as one: strict templates refuse the pair.
        msgs.push(Message { role, content }, context_at);
    }
    if let Some(parts) = spoken {
        msgs.push(
            Message {
                role: Role::User,
                content: parts.to_vec(),
            },
            None,
        );
    }
    msgs.into_vec()
}

/// The part of a tool turn's `content` its stored record does not already
/// hold. `content` is every text the loop streamed, in order; the record keeps
/// the text of the turns before the last (beside their tool calls) and drops
/// only the last one's, so what is left after the record's text is the final
/// answer. A `content` that does not start with it (a record written some
/// other way) is taken whole rather than guessed at.
fn final_answer<'a>(record: &[Message], content: &'a str) -> &'a str {
    let said = record_said(record);
    content.strip_prefix(said.as_str()).unwrap_or(content)
}

/// The text a tool turn's record holds: its assistant messages' text, in
/// order — what `content` starts with, before the final answer.
pub(super) fn record_said(record: &[Message]) -> String {
    record
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ir: Option<&str>) -> ChatMessageRow {
        ChatMessageRow {
            role: "assistant".into(),
            ir_messages: ir.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_record_is_no_tool_record() {
        assert!(!has_tool_record(&row(None)));
        assert!(!has_tool_record(&row(Some("[]"))));
        assert!(has_tool_record(&row(Some(
            r#"[{"role":"assistant","content":[]}]"#
        ))));
        // Unreadable: kept out of reach of a prefill rather than guessed at.
        assert!(has_tool_record(&row(Some("not json"))));
    }

    #[test]
    fn the_final_answer_is_what_the_record_has_not_said() {
        let record = vec![
            Message::text(Role::Assistant, "Checking. "),
            Message::text(Role::User, "tool output"),
        ];
        assert_eq!(final_answer(&record, "Checking. all good"), "all good");
        assert_eq!(final_answer(&[], "all good"), "all good");
        assert_eq!(
            final_answer(&record, "something else"),
            "something else",
            "a content the record does not lead is taken whole"
        );
    }
}
