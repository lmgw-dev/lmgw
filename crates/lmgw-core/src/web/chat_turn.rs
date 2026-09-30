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
use std::convert::Infallible;
use std::future::Future;

use axum::http::StatusCode;
use axum::response::sse::{Event as SseFrame, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::agentchat::{self, ToolPlan, ADMIN_KIND};
use super::chat::{err_json, run_send};
use super::chat_attach_gate::Caps;
use super::chat_live::Ticket;
use super::chat_repo::ChatRepo;
use super::{chat_attach, chat_knowledge, chat_reasoning, chat_sampling};
use crate::config::{Protocol, Route, Snapshot, UpstreamKind};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, ContentPart, Message, Params, ReasoningControl, Role};
use crate::state::{AppState, SharedState};
use crate::store::{ChatAttachmentFull, ChatMessageRow, ChatReply, ChatThread, ContinueSave};

/// A turn's SSE channel.
pub(super) type Events = mpsc::Sender<Result<SseFrame, Infallible>>;

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
}

/// Why a turn gave up before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stopped {
    /// A newer turn of the thread started, or the thread went away.
    Superseded,
    /// The page stopped reading: Stop, a reload, a closed tab.
    ClientGone,
}

enum Persist {
    /// A new assistant row.
    Insert,
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

    /// This turn's hold on its thread.
    pub(super) fn ticket(&self) -> &Ticket {
        &self.ticket
    }

    /// A continue, which only a route that takes a prefill can serve.
    pub(super) fn is_continue(&self) -> bool {
        matches!(self.persist, Persist::Append { .. })
    }

    /// The reply text a continue was shown (its prefill) — `None` for a
    /// fresh reply.
    pub(super) fn continued_text(&self) -> Option<&str> {
        match &self.persist {
            Persist::Append { content, .. } => Some(content),
            Persist::Insert => None,
        }
    }

    /// Resolves when this turn should stop: a newer turn of the thread
    /// started (or the thread went away), or the page stopped reading `tx`.
    pub(super) async fn stopped(&self, tx: &Events) -> Stopped {
        tokio::select! {
            biased;
            () = self.ticket.superseded() => Stopped::Superseded,
            () = tx.closed() => Stopped::ClientGone,
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

    /// Say why the turn ended early, to a page that may still be reading
    /// (another tab, when a newer turn replaced this one): an `error` and
    /// `done {aborted}`. Nothing to say to a page that left.
    pub(super) async fn report_stop(&self, why: Stopped, tx: &Events) {
        if why == Stopped::ClientGone {
            return;
        }
        let msg = "a newer turn of this thread started (or the thread went away), so this reply \
                   was stopped and is not saved";
        let _ = tx
            .send(Ok(SseFrame::default().event("error").data(
                json!({ "message": msg, "code": "superseded" }).to_string(),
            )))
            .await;
        let _ = tx
            .send(Ok(SseFrame::default()
                .event("done")
                .data(json!({ "aborted": true }).to_string())))
            .await;
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
    /// - a stopped fresh reply that produced nothing;
    /// - the write failed (the thread is gone, or the DB refused), logged.
    pub(super) async fn persist(&self, state: &AppState, r: Reply<'_>) -> Persisted {
        let nothing = r.text.is_empty() && r.reasoning.is_empty() && r.ir_messages.is_none();
        if nothing && (r.stopped || self.is_continue()) {
            // Nothing came: a stopped reply is not an empty bubble, and a
            // continue that added nothing leaves its row as it was.
            let id = match &self.persist {
                Persist::Append { message_id, .. } => *message_id,
                Persist::Insert => 0,
            };
            return Persisted { id, refused: false };
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
        };
        let res = match &self.persist {
            Persist::Insert => {
                self.repo
                    .save_reply(state, &proof, self.thread_id, &reply)
                    .await
            }
            Persist::Append {
                message_id,
                content,
                reasoning,
            } => {
                reply.content = format!("{content}{}", r.text);
                reply.reasoning = format!("{reasoning}{}", r.reasoning);
                self.repo
                    .save_continue(state, &proof, self.thread_id, *message_id, content, &reply)
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
pub(super) fn answered_by(snap: &Snapshot, headers: &crate::gate::GateHeaders) -> Option<String> {
    headers
        .fallback()
        .map(str::to_string)
        .or_else(|| headers.candidate().map(|m| snap.local_public_name(m)))
}

/// Start one turn: read the thread's history, spawn the worker, and answer
/// with its SSE stream (`turn` / `retrieval` / `delta` / `reasoning` / `tool`
/// / `usage` / `stats` / `error` / `done`). The worker first runs an auto-mode
/// knowledge retrieval when the turn has one ([`chat_knowledge`]; after the
/// `turn` event, so the page learns its message id while the search runs),
/// then builds the request with the thread's current settings. `caps` is
/// [`thread_caps`](super::chat_attach_gate::thread_caps)'s, which the caller
/// already needed for its own checks.
pub(super) async fn start_turn(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
    mode: TurnMode,
    caps: Caps,
) -> Response {
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
            return continue_gone();
        }
    }
    // The thread's one live turn from here: the previous one is cancelled,
    // and anything that rewrites the history after this point keeps this
    // turn's reply from being saved onto it. The history is read after.
    let ticket = state.chat_live.begin(thread.id).await;
    // Turning an archived thread over restores it (chat-archive design §1).
    let _ = repo.wake(state, thread).await;
    let mut history = repo.messages(state, thread.id).await.unwrap_or_default();

    let (user_message_id, persist) = match mode {
        TurnMode::Fresh { user_message_id } => (user_message_id, Persist::Insert),
        TurnMode::Continue { message_id } => {
            let Some(last) = history
                .last_mut()
                .filter(|m| m.id == message_id && m.role == "assistant")
            else {
                return continue_gone();
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
    };
    let (tx, rx) = mpsc::channel::<Result<SseFrame, Infallible>>(64);
    if let Some(id) = user_message_id {
        // A fresh channel with room: this cannot wait.
        let _ = tx
            .send(Ok(SseFrame::default()
                .event("turn")
                .data(json!({ "user_message_id": id }).to_string())))
            .await;
    }
    let turn = Turn {
        repo,
        thread_id: thread.id,
        persist,
        model: thread.model_alias.clone(),
        ticket,
    };
    let state = state.clone();
    let thread = thread.clone();
    tokio::spawn(async move {
        // The waits before the request goes out give way the moment the turn
        // is stopped or replaced (review R1 finding 1): nothing is saved and
        // nothing more is started for a reply nobody will read.
        if let Some(auto) = kb.auto {
            let ran = turn
                .or_stop(
                    &tx,
                    chat_knowledge::run_auto(
                        &state,
                        repo,
                        thread.id,
                        &mut history,
                        auto,
                        turn.ticket(),
                        &tx,
                    ),
                )
                .await;
            if let Err(why) = ran {
                turn.report_stop(why, &tx).await;
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
                    parts.push(chat_attach::render(&state, att, caps, &thread.model_alias).await);
                }
                rendered.insert(*mid, parts);
            }
            rendered
        };
        let rendered = match turn.or_stop(&tx, render).await {
            Ok(r) => r,
            Err(why) => {
                turn.report_stop(why, &tx).await;
                return;
            }
        };
        drop(attachments);
        let ir = request(&state, &thread, &history, &rendered);
        if plan.is_empty() {
            run_send(state, turn, ir, tx).await;
        } else {
            agentchat::run_send(state, turn, ir, plan, tx).await;
        }
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The turn's request: the history as IR with the thread's current settings.
fn request(
    state: &SharedState,
    thread: &ChatThread,
    history: &[ChatMessageRow],
    attachments: &HashMap<i64, Vec<chat_attach::Rendered>>,
) -> ChatRequest {
    ChatRequest {
        model_alias: thread.model_alias.clone(),
        messages: build_messages(
            thread,
            &effective_alias(state, &thread.model_alias),
            history,
            attachments,
        ),
        params: Params {
            // temperature plus the thread's other sampling choices; the relay
            // drops what the route cannot take and reports it.
            // No silent cap: forward the user's max_tokens or none (use the
            // model's real remaining budget).
            max_tokens: thread.max_tokens.map(|v| v.max(0) as u32),
            // The thread's overrides, at the tier the `x-lmgw-reasoning*`
            // headers take: the route's defaults fill in around them.
            reasoning: chat_reasoning::control(thread),
            ..chat_sampling::params_of(thread)
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
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
pub(super) async fn model_caps(state: &SharedState, alias: &str) -> Caps {
    let effective = effective_alias(state, alias);
    let caps = crate::capabilities::exposed::exposed_entry(state, &effective)
        .await
        .and_then(|e| e.capabilities);
    Caps {
        vision: caps.as_ref().and_then(|c| c.vision),
        audio: caps
            .and_then(|c| c.input_modalities)
            .map(|m| m.iter().any(|x| x == "audio")),
    }
}

/// The alias that will answer a turn sent to `alias`: its GPU-hold fallback
/// when the hold re-routes it (decided at resolve time, as `run_send`'s own
/// resolve decides it), else `alias` itself — also when it does not resolve,
/// since that send fails with its own error either way.
fn effective_alias(state: &SharedState, alias: &str) -> String {
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
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(super) struct ContinueState {
    pub ok: bool,
    pub reason: Option<String>,
}

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
    let own = Params {
        reasoning: chat_reasoning::control(thread),
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
    match (route.upstream.protocol, route.upstream.kind) {
        (Protocol::Anthropic, _) if control.enabled == Some(true) => Some(
            "extended thinking is on for this thread, and Anthropic cannot continue a reply \
             while thinking — switch reasoning off in the thread settings to continue"
                .into(),
        ),
        (Protocol::Anthropic, _) | (Protocol::Openai, UpstreamKind::LlamaServer) => None,
        (Protocol::Openai, _) => Some(format!(
            "the route answering this thread (upstream '{name}') is an OpenAI-compatible API, \
             which cannot continue a reply — Continue needs an Anthropic or llama-server route"
        )),
        (Protocol::Gemini, _) => Some(format!(
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
/// on a route without a prefill ([`prefill_refusal`]).
pub(super) fn fit_route(
    route: &Route,
    ir: &ChatRequest,
    continuing: bool,
) -> Result<RouteFit, GatewayError> {
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
    let llama = route.upstream.kind == UpstreamKind::LlamaServer
        && route.upstream.protocol == Protocol::Openai;
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
fn build_messages(
    thread: &ChatThread,
    answering: &str,
    history: &[ChatMessageRow],
    attachments: &HashMap<i64, Vec<chat_attach::Rendered>>,
) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(history.len() + 1);
    let sys = crate::config::expand_chat_prompt(
        thread.system_prompt.trim(),
        answering,
        chrono::Local::now().date_naive(),
    );
    if thread.kind == ADMIN_KIND {
        msgs.push(Message::text(Role::System, agentchat::system_prompt(&sys)));
    } else if !sys.is_empty() {
        msgs.push(Message::text(Role::System, sys));
    }
    for m in history {
        if let Some(raw) = &m.ir_messages {
            if let Ok(turn) = serde_json::from_str::<Vec<Message>>(raw) {
                // The record stops short of the final answer — the tool loop
                // keeps that in `content` alone (`agentchat`'s
                // `trim_trailing_text`) — so it follows the record here, or
                // the model would never see what it answered.
                let answer = final_answer(&turn, &m.content);
                msgs.extend(turn);
                if m.role == "assistant" && !answer.is_empty() {
                    msgs.push(Message::text(Role::Assistant, answer.to_string()));
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
                content.extend(r.parts.iter().cloned());
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
        if let Some(block) = m.context.as_ref().and_then(chat_knowledge::context_part) {
            content.push(block);
        }
        // The typed text — except an empty one tacked onto a message whose
        // attachments already carry its content ("content may be empty when
        // attachments are present", design §2). Every other empty-content
        // case (a reasoning-only turn, say) keeps its always-present text
        // part exactly as before attachments existed.
        if !m.content.is_empty() || !had_attachment_parts {
            content.push(ContentPart::text(m.content.clone()));
        }
        msgs.push(Message { role, content });
    }
    msgs
}

/// The part of a tool turn's `content` its stored record does not already
/// hold. `content` is every text the loop streamed, in order; the record keeps
/// the text of the turns before the last (beside their tool calls) and drops
/// only the last one's, so what is left after the record's text is the final
/// answer. A `content` that does not start with it (a record written some
/// other way) is taken whole rather than guessed at.
fn final_answer<'a>(record: &[Message], content: &'a str) -> &'a str {
    let said: String = record
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    content.strip_prefix(said.as_str()).unwrap_or(content)
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
