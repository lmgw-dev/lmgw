//! The `image__*` and `audio__*` tools as a [`ToolExecutor`]
//! (chat-media-tools design §6.1): the same [`media::run`] `/mcp` reaches,
//! for an in-process run.
//!
//! What differs per caller is **where files go** and **what happens before
//! admission**: thread-less by default — `att:N` names nothing and the files
//! come back in the result — and a Chat thread's with
//! [`MediaExecutor::for_thread`], which stores each call's files as the
//! thread's attachments and names them `att:N`. A tool loop that holds a GPU
//! claim — the Chat's, a `/v1/responses` run's, an agent run's — lets it go
//! before a GPU call through its hooks ([`MediaExecutor::with_hooks`],
//! `agent::claim::media`).
//!
//! A call made by a run's loop takes the loop's stop for the call
//! (`agent::take_call_stop`): a stopped turn ends the call at its next
//! await — a running image job is cancelled (`proxy::image_call`) — and
//! its model row and its tool row are still written.
//!
//! A run that follows its calls while they run ([`LiveCalls`], the Chat's)
//! gives each call its progress reader and a stop of its own, which the
//! run's per-call cancel raises (design §6.5); the turn's stop raises it
//! too, so both take the same path.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::agent::{ToolExecutor, ToolOutcome};
use crate::erased::Erased;
use crate::ingress::ClientProto;
use crate::ir::ToolResultBlock;
use crate::proxy::image_call::{Hooks, NoHooks};
use crate::proxy::{RequestCtx, StopHandle, StopSignal};
use crate::state::SharedState;

use lmgw_api_types::chat_media::MediaDefaults;

use super::super::media::{self, CallCx, NoThread, Progress, Refs};

/// A run that follows its media calls while they run (module doc).
pub(crate) trait LiveCalls: Send + Sync {
    /// Call `call_id` starts.
    fn begin(&self, call_id: &str) -> LiveCall;
}

/// One followed call: who reads its progress, and its own stop. Dropped
/// when the call ends, which ends what follows it.
pub(crate) struct LiveCall {
    pub progress: Box<dyn Progress>,
    pub stop: StopSignal,
    /// Raises [`Self::stop`]: the turn's stop is passed on through it.
    pub raise: Arc<StopHandle>,
}

/// Executes the media tools for an in-process run.
pub struct MediaExecutor {
    state: SharedState,
    ctx: RequestCtx,
    /// The tool rows' `ingress_proto`.
    proto: &'static str,
    /// The model rows' label where the run's tool label does not say it
    /// ([`Self::with_model_proto`]).
    model_proto: Option<ClientProto>,
    /// Whose request the model rows are, and checked against: a device's
    /// key in a device's turn (design §3.7). `None`: `ctx`'s.
    charged: Option<RequestCtx>,
    refs: Arc<dyn Refs>,
    hooks: Arc<dyn Hooks>,
    live: Option<Arc<dyn LiveCalls>>,
    /// The default models the run's tool block named at its start (design
    /// §3.8, [`Self::with_defaults`]); `None`: Settings → Chat's at each
    /// call.
    defaults: Option<MediaDefaults>,
}

impl MediaExecutor {
    pub fn new(state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            state,
            ctx,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
            model_proto: None,
            charged: None,
            refs: Arc::new(NoThread),
            hooks: Arc::new(NoHooks),
            live: None,
            defaults: None,
        }
    }

    /// The default models the run's tool block named
    /// (`Resolved::media_defaults`, design §3.8): a call that leaves out
    /// `model` runs on what the description promised, whatever the
    /// settings say by the time it is made.
    pub(crate) fn with_defaults(mut self, defaults: MediaDefaults) -> Self {
        self.defaults = Some(defaults);
        self
    }

    /// `live` follows each call while it runs (module doc).
    pub(crate) fn with_live(mut self, live: Arc<dyn LiveCalls>) -> Self {
        self.live = Some(live);
        self
    }

    /// See [`super::McpExecutor::with_proto`].
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }

    /// Label the model rows `proto`: an Admin Chat thread's are `admin`,
    /// as its model turns, voice and card actions are, while its tool rows
    /// stay `chat-tool` like the thread's other tools.
    pub(crate) fn with_model_proto(mut self, proto: ClientProto) -> Self {
        self.model_proto = Some(proto);
        self
    }

    /// Charge the model calls to `caller`, as [`super::DocsExecutor::charged_to`].
    pub fn charged_to(mut self, caller: Option<RequestCtx>) -> Self {
        self.charged = caller;
        self
    }

    /// A Chat thread's: its files are `refs`'.
    pub(crate) fn for_thread(mut self, refs: Arc<dyn Refs>) -> Self {
        self.refs = refs;
        self
    }

    /// `hooks` run before each call's admission: a loop's claim let go.
    pub(crate) fn with_hooks(mut self, hooks: Arc<dyn Hooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// The model rows' label: the Chat's own, `admin` for an Admin Chat
    /// thread ([`Self::with_model_proto`]), `realtime` for a session,
    /// `openai` for the rest — as their other model calls are labelled.
    fn model_proto(&self) -> ClientProto {
        if let Some(p) = self.model_proto {
            return p;
        }
        match self.proto {
            crate::telemetry::CHAT_TOOL_PROTO => ClientProto::Chat,
            crate::telemetry::ADMIN_TOOL_PROTO => ClientProto::AdminChat,
            crate::telemetry::REALTIME_TOOL_PROTO => ClientProto::Realtime,
            _ => ClientProto::OpenaiChat,
        }
    }

    async fn run(
        &self,
        (name, args): (&str, &Value),
        stop: Option<&StopSignal>,
        progress: Option<&dyn Progress>,
    ) -> ToolOutcome {
        if self.state.snapshot().tool_disabled(name) {
            return ToolOutcome::error(super::super::disabled_message(name));
        }
        let args = match args {
            Value::Object(m) => m.clone(),
            _ => Default::default(),
        };
        let charged = self.charged.as_ref().unwrap_or(&self.ctx);
        let cx = CallCx {
            ctx: charged,
            proto: self.model_proto(),
            refs: &*self.refs,
            hooks: &*self.hooks,
            progress,
            stop,
            defaults: self.defaults.as_ref(),
        };
        let out = match media::run(&self.state, name, &args, &cx).await {
            Ok(o) => o,
            Err(e) => return ToolOutcome::error(e.to_string()),
        };
        if out.is_error || out.files.is_empty() {
            let blocks = ToolResultBlock::one(out.text_for(None));
            return ToolOutcome {
                is_error: out.is_error,
                ..ToolOutcome::ok(blocks)
            };
        }
        let call_id = crate::agent::current_call_id().unwrap_or_default();
        match self.refs.store(&call_id, &out.files).await {
            None => ToolOutcome::ok(out.inline_blocks()),
            Some(Ok(ids)) => ToolOutcome::ok(out.thread_blocks(&ids)),
            Some(Err(e)) => ToolOutcome::error(format!(
                "{} — but it could not be stored in this conversation: {e}",
                out.text_for(None)
            )),
        }
    }
}

impl ToolExecutor for MediaExecutor {
    fn call<'a>(&'a self, name: &'a str, args: &'a Value) -> Erased<'a, ToolOutcome> {
        Box::pin(async move {
            let started = Instant::now();
            // Taken first: from here the call ends itself on a stop.
            let turn_stop = crate::agent::take_call_stop();
            let call_id = crate::agent::current_call_id().unwrap_or_default();
            let live = self.live.as_ref().map(|l| l.begin(&call_id));
            let stop = match &live {
                Some(l) => Some(l.stop.clone()),
                None => turn_stop.clone(),
            };
            let run = self.run(
                (name, args),
                stop.as_ref(),
                live.as_ref().map(|l| &*l.progress),
            );
            let outcome = match (&live, &turn_stop) {
                // The turn's stop is the call's own too (module doc).
                (Some(l), Some(turn)) => {
                    tokio::pin!(run);
                    tokio::select! {
                        biased;
                        () = turn.raised() => {
                            l.raise.stop();
                            run.await
                        }
                        o = &mut run => o,
                    }
                }
                _ => run.await,
            };
            drop(live);
            let server = Some(media::server_of(name).to_string());
            if outcome.is_error && stop.as_ref().is_some_and(StopSignal::is_raised) {
                // Stopped with its turn: `canceled`, as a dropped call's row.
                super::super::ingress::record_tool_canceled(
                    &self.state,
                    &self.ctx,
                    self.proto,
                    (name, server),
                    started,
                    crate::agent::ABANDONED_CALL,
                )
                .await;
                return outcome;
            }
            super::super::ingress::record_tool_call(
                &self.state,
                &self.ctx,
                self.proto,
                name,
                server,
                started,
                if outcome.is_error {
                    Some(super::preview(&outcome.blocks))
                } else {
                    None
                },
            )
            .await;
            outcome
        })
    }
}
