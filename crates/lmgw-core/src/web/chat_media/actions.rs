//! `POST /chat/api/attachments/{id}/media` — an image card's actions
//! (chat-media-tools design §8.3): re-roll (the same prompt and settings, a
//! new seed), vary or edit (an image call with this picture as the input),
//! upscale (an ESRGAN upscale). Run directly, with no chat model in the
//! loop and no claim to let go — a turn running in the thread at the same
//! time is just another request to admission.
//!
//! **Refused before anything runs** (JSON, the Chat API's flat shape):
//! `404 not_found` for an attachment that is not there or not the caller's,
//! `422 not_an_image` for any other kind, `409 not_sent` for a picture that
//! belongs to no message yet (a draft, or a reply still being written), and
//! `422` with the plan's code ([`plan`]) for what the picture lacks.
//!
//! **Then SSE**: `action {call_id, action, model?, source}` first — the
//! `act-<boot>-<n>` id the action runs under (unique across restarts),
//! which its progress frames carry as `call_id` and the media cancel route
//! stops (`cancel_route`); `progress` frames while it runs (lossy; the tool
//! calls' `ToolProgressFrame`, previews included); then
//! `done {call_id, attachment, text}` or
//! `error {message, code}`.
//!
//! **The result** is a `chat_attachments` row with `source = 'action'`, the
//! action id as its `call_id`, bound to the source's message, its
//! `meta.media` the facts with `parent` (an edit's input, else the picture
//! it was made from). It is **not shown to the model** unless the user
//! attaches it (the owner's decision, §12 decision 12): the turn's history
//! renders only the tool results' refs, never a message's action files.
//!
//! The action runs in its own task: a reader that goes away does not stop
//! it, and its picture is stored all the same — a reload shows it. What
//! stops it: the cancel; its thread going away (deleted, kept as a stored
//! chat, purged — `MediaCalls::thread_gone`, beside the thread's turn being
//! cancelled), its `error` frame then `thread_gone`; and its caller's key
//! revoked (Disable, Rotate, Delete, expiry), its `error` frame then
//! `revoked`. A running job is cancelled by its guard (design §5.4) as a
//! tool call's is.
//!
//! The model rows are the caller's (a device's key, the owner's internal
//! identity), labelled as the thread's speech calls are (`chat`, `admin`
//! for an Admin Chat thread). No tool row: no model called a tool.

use std::convert::Infallible;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use lmgw_api_types::chat_media::{
    MediaActionDone, MediaActionRequest, MediaActionStarted, SOURCE_ACTION,
};
use lmgw_api_types::chat_threads::AttachmentFacts;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::mcp::media::{self, Outcome};
use crate::proxy::image_call::{self, upscale, ImageCall, InputImage};
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::store::{ChatAttachmentFull, NewAttachment};

use super::super::chat::{err_json, unreachable_attachment};
use super::super::chat_caller::Caller;
use super::super::chat_extract::{ChatJson, ChatPath};
use super::super::chat_repo::ChatRepo;
use super::super::chat_turn::TurnFrame;

mod plan;
mod running;

use plan::Plan;
use running::{ActionHooks, Running};

/// How many frames wait for a slow reader before progress frames are
/// dropped: progress is lossy, the last frames are sent with a wait.
const FRAMES: usize = 32;

/// `POST /chat/api/attachments/{id}/media` (module doc).
pub(crate) async fn run_action(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<MediaActionRequest>,
) -> Response {
    if let Some(refused) = unreachable_attachment(&state, &caller, id).await {
        return refused;
    }
    let not_found = || err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found");
    let src = match ChatRepo::of(id).attachment(&state, id).await {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let repo = ChatRepo::of(src.thread_id);
    let Some(thread) = repo
        .thread_as(&state, &caller, src.thread_id)
        .await
        .ok()
        .flatten()
    else {
        return not_found();
    };
    if src.kind != "image" {
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_an_image",
            format!(
                "the card actions work on pictures, and this is {}",
                src.kind
            ),
        );
    }
    let Some(message_id) = src.message_id else {
        return err_json(
            StatusCode::CONFLICT,
            "not_sent",
            "this picture belongs to no message yet — a draft, or a reply still being \
             written; its actions work once it is part of the conversation",
        );
    };
    let ctx = caller.ctx();
    let offer = media::offer(&state)
        .await
        .within(&state.snapshot(), ctx.client_key.as_deref());
    let thread_id = src.thread_id;
    let parent_of = |pid: i64| {
        let state = state.clone();
        async move {
            repo.attachments_by_ids(&state, thread_id, &[pid])
                .await
                .ok()
                .and_then(|v| v.into_iter().next())
        }
    };
    let planned = match plan::plan(&state, &src, &req, &offer, parent_of).await {
        Ok(p) => p,
        Err((code, message)) => {
            return err_json(StatusCode::UNPROCESSABLE_ENTITY, code, message);
        }
    };

    let (stop_handle, stop_signal) = crate::proxy::stop_pair();
    let running = Running::start(&state, thread_id, stop_handle);
    let call_id = running.id.clone();
    // The caller's key revoked while it runs stops it (module doc).
    let watch = crate::devices::watch(&state, &ctx);
    let started = MediaActionStarted {
        call_id: call_id.clone(),
        action: req.action.as_str().to_string(),
        model: planned.model(),
        source: src.id,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(FRAMES);
    let hooks = ActionHooks {
        running,
        started: Instant::now(),
        tx: tx.clone(),
    };
    let job = Job {
        state: state.clone(),
        ctx,
        proto: super::super::chat_voice::speech_proto(&thread),
        src,
        message_id,
    };
    // Its own task (module doc): the reader going away does not stop it.
    tokio::spawn(async move {
        let run = job.run(planned, &hooks, &stop_signal, &call_id);
        let (ended, revoked) = until_revoked(run, watch, &hooks.running).await;
        let frame = match (ended, revoked) {
            (Ok(done), _) => TurnFrame::of("done", &done),
            (Err(_), Some(why)) => TurnFrame::error(&GatewayError::Refused {
                status: 403,
                code: "revoked",
                message: format!("{why}; the action stopped and its picture was not kept"),
            }),
            (Err(_), None) if hooks.running.thread_gone() => TurnFrame::error(&thread_went()),
            (Err(e), None) => TurnFrame::error(&e),
        };
        let _ = tx.send(frame).await;
        // Off the running calls once it has ended.
        drop(hooks);
    });
    let first = futures::stream::once(async move { TurnFrame::of("action", &started) });
    let frames = first
        .chain(ReceiverStream::new(rx))
        .map(|f| Ok::<_, Infallible>(f.into_sse()));
    caller.sse_response(&state, frames)
}

/// `run` to its end, its stop raised should `watch`'s key be revoked first:
/// what it ended with, and the revocation's sentence when one stopped it.
async fn until_revoked<F, T>(
    run: F,
    watch: Option<crate::devices::Watched>,
    running: &Running,
) -> (T, Option<String>)
where
    F: std::future::Future<Output = T>,
{
    let mut run = std::pin::pin!(run);
    let Some(mut watch) = watch else {
        return (run.await, None);
    };
    tokio::select! {
        biased;
        out = &mut run => (out, None),
        reason = watch.revoked() => {
            running.stop();
            let said = watch.said(reason).message;
            tracing::info!("{said}: card action {} stopped", running.id);
            (run.await, Some(said))
        }
    }
}

/// The refusal for an action whose thread went away while it ran.
fn thread_went() -> GatewayError {
    GatewayError::Refused {
        status: 409,
        code: "thread_gone",
        message: "the conversation was deleted, or kept as a stored chat, while the action \
                  ran: the action stopped and its picture was not kept — run it again on the \
                  kept chat"
            .into(),
    }
}

/// One action's run: the call, then its file stored and bound.
struct Job {
    state: SharedState,
    ctx: RequestCtx,
    proto: ClientProto,
    src: ChatAttachmentFull,
    message_id: i64,
}

impl Job {
    async fn run(
        &self,
        plan: Plan,
        hooks: &ActionHooks,
        stop: &StopSignal,
        call_id: &str,
    ) -> Result<MediaActionDone, GatewayError> {
        let state = &self.state;
        let outcome = match plan {
            Plan::Image {
                tool,
                alias,
                action,
                params,
                parent,
            } => {
                let call = ImageCall {
                    alias: &alias,
                    action,
                    params: params.clone(),
                };
                let out =
                    image_call::run(state, &self.ctx, self.proto, call, hooks, Some(stop)).await?;
                let mut said = media::said_image(tool, &alias, &params, out);
                for f in &mut said.files {
                    f.facts.parent = Some(parent);
                }
                said
            }
            Plan::Upscale {
                upscaler,
                repeats,
                made_by,
                rows,
            } => {
                let call = upscale::UpscaleCall {
                    image: InputImage {
                        mime: self.src.mime.clone(),
                        bytes: self.src.data.clone(),
                    },
                    upscaler: upscaler.as_deref(),
                    repeats,
                    made_by: made_by.as_deref(),
                    rows: Some(&rows),
                };
                let out =
                    upscale::run(state, &self.ctx, self.proto, call, hooks, Some(stop)).await?;
                media::upscale::said(&self.src.name, Some(self.src.id), out)
            }
        };
        self.save(outcome, call_id).await
    }

    /// Store the picture as the thread's `action` attachment and bind it to
    /// the source's message.
    async fn save(&self, outcome: Outcome, call_id: &str) -> Result<MediaActionDone, GatewayError> {
        let failed = |m: String| GatewayError::Upstream {
            status: 500,
            provider_type: None,
            message: m,
        };
        let Some(file) = outcome.files.first() else {
            return Err(failed("the action made no picture".into()));
        };
        let new = NewAttachment {
            kind: file.kind.to_string(),
            name: file.name.clone(),
            mime: file.mime.clone(),
            data: file.bytes.clone(),
            extracted: None,
            meta: serde_json::to_value(AttachmentFacts {
                media: Some(file.facts.clone()),
                ..Default::default()
            })
            .unwrap_or_default(),
            mode: None,
            source: SOURCE_ACTION.to_string(),
            call_id: Some(call_id.to_string()),
        };
        let thread_id = self.src.thread_id;
        let repo = ChatRepo::of(thread_id);
        // A thread that is no longer there (deleted, or kept as a stored
        // chat, while the action ran) says so, rather than that its
        // message went or that the store failed.
        let thread_there = || async {
            repo.thread(&self.state, thread_id)
                .await
                .is_ok_and(|t| t.is_some())
        };
        let id = match repo.insert_tool_file(&self.state, thread_id, &new).await {
            Ok(id) => id,
            Err(_) if !thread_there().await => return Err(thread_went()),
            Err(e) => return Err(failed(format!("the picture could not be stored: {e}"))),
        };
        let bound = repo
            .bind_action_file(&self.state, thread_id, self.message_id, id)
            .await
            .unwrap_or(false);
        if !bound {
            let _ = repo.drop_tool_files(&self.state, thread_id, &[id]).await;
            if !thread_there().await {
                return Err(thread_went());
            }
            return Err(failed(
                "the picture's message was deleted while the action ran, so the result was \
                 not kept"
                    .into(),
            ));
        }
        let meta = new.into_meta(id, thread_id);
        Ok(MediaActionDone {
            call_id: call_id.to_string(),
            attachment: super::super::chat_wire::attachment(&meta),
            text: outcome.text_for(Some(&[id])),
        })
    }
}
