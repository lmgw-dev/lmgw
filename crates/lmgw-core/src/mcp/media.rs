//! The `image__*` and `audio__*` built-in toolsets (chat-media-tools design
//! §3): a chat model draws and edits pictures with the gateway's image rows,
//! speaks text and transcribes recordings with its audio rows — on the
//! aggregate `/mcp` plane next to `docs__*` and `kb__*`, and attachable to a
//! Chat thread, a `/v1/responses` request, an agent run or a realtime
//! session by the reserved labels `image` and `audio`.
//!
//! **One tool per action, not per row** (decision 1): `image__generate`,
//! `image__edit`, `audio__speak`, `audio__transcribe`, each with a `model`
//! whose enum is the rows and cloud aliases that can do it, listed from the
//! snapshot on every `tools/list` ([`list`]). A tool with nothing to offer
//! is not listed; `image__edit` offers only the models that edit
//! ([`edit_rows`]). `model` is required unless a default model resolves for
//! the call ([`defaults`], design §3.8).
//!
//! **Calls run in process** through the same gate every `/v1` request takes
//! (`proxy::image_call`, `proxy::synthesize`, `proxy::transcribe`): the key
//! policy, the hold swap, admission, one `request_logs` row charged to the
//! caller. A Chat thread's files go through [`Refs`]: a picture or a
//! recording the model names as `att:N`, and what a call made, stored as
//! the thread's attachments ([`result`]).
//!
//! **While a call runs** (design §5, §6.2, §7.2) it reports its stages,
//! steps and previews to the caller's [`Progress`], and ends itself on its
//! stop — a running image job cancelled by its guard
//! (`proxy::image_call`).
//!
//! `image__upscale` is offered only when the image models dir's
//! `upscalers/` holds a file: an ESRGAN upscale on a local image row
//! (`proxy::image_call::upscale`), no `model` parameter — the row is the
//! gateway's choice (design §9) — and its `upscaler` enum the file stems.

use lmgw_api_types::chat_media::MediaDefaults;
use serde_json::{Map, Value};

use crate::ingress::ClientProto;
use crate::proxy::image_call::Hooks;
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;

use super::CallError;

mod args;
mod audio;
pub mod defaults;
mod edit_rows;
mod hooks;
mod image;
pub(crate) mod list;
pub(crate) mod refs;
pub(crate) mod result;
pub(crate) mod upscale;

pub use defaults::MediaLayers;
pub(crate) use hooks::Progress;
pub(crate) use image::said as said_image;
pub use list::{offer, Offer};
pub(crate) use refs::{NoThread, Refs, Source};
pub(crate) use result::{Outcome, Produced};

/// The label the image tools are attached by, and their namespace's prefix.
pub const IMAGE_LABEL: &str = super::RESERVED_IMAGE_PREFIX;
/// The label the audio tools are attached by.
pub const AUDIO_LABEL: &str = super::RESERVED_AUDIO_PREFIX;

pub const IMAGE_GENERATE: &str = "image__generate";
pub const IMAGE_EDIT: &str = "image__edit";
pub const IMAGE_UPSCALE: &str = "image__upscale";
pub const AUDIO_SPEAK: &str = "audio__speak";
pub const AUDIO_TRANSCRIBE: &str = "audio__transcribe";

/// `upstream_name` on a call's log row (`exec/served_by.rs`).
pub const IMAGE_SERVER: &str = "lmgw (image)";
/// The same, for the audio tools.
pub const AUDIO_SERVER: &str = "lmgw (audio)";

/// Whether `name` is one of these tools' names.
pub fn owns(name: &str) -> bool {
    name.starts_with(super::RESERVED_IMAGE_NAMESPACE)
        || name.starts_with(super::RESERVED_AUDIO_NAMESPACE)
}

/// The log row's server for a tool of these toolsets.
pub fn server_of(name: &str) -> &'static str {
    if name.starts_with(super::RESERVED_AUDIO_NAMESPACE) {
        AUDIO_SERVER
    } else {
        IMAGE_SERVER
    }
}

/// How one call runs: who it is charged to, what its row says, whose files
/// its refs name, what it does before admission, and its stop.
pub(crate) struct CallCx<'a> {
    /// The caller: a device's key in a device's turn, the `/mcp` request's
    /// key, the gateway's own otherwise (design §3.7).
    pub ctx: &'a RequestCtx,
    /// The model rows' `ingress_proto`.
    pub proto: ClientProto,
    pub refs: &'a dyn Refs,
    pub hooks: &'a dyn Hooks,
    /// Who follows the call's progress, if anyone.
    pub progress: Option<&'a dyn Progress>,
    pub stop: Option<&'a StopSignal>,
    /// The default models the run's tool block named at its start
    /// (design §3.8): a model-less call runs on what its description
    /// promised. `None`: Settings → Chat's as they are now (an `/mcp`
    /// call, a request of its own).
    pub defaults: Option<&'a MediaDefaults>,
}

/// Run one of these tools. `Err` means "not one of these tools"; every
/// other failure is an [`Outcome`] the model reads (`isError`).
pub(crate) async fn run(
    state: &SharedState,
    name: &str,
    args: &Map<String, Value>,
    cx: &CallCx<'_>,
) -> Result<Outcome, CallError> {
    // The models the caller's key may use: one outside its scope is refused
    // with the choices it has, as an unknown one is.
    let offer = list::offer(state)
        .await
        .within(&state.snapshot(), cx.ctx.client_key.as_deref());
    // The defaults a call that leaves out `model` uses (design §3.8): the
    // ones the run's tool block named, not the levels as they are now.
    let offer = match cx.defaults {
        Some(d) => offer.frozen_to(d),
        None => offer,
    };
    // The claim hook and the progress as one, repeats left out.
    let hooks = hooks::CallHooks::new(cx.hooks, cx.progress);
    let cx = &CallCx {
        hooks: &hooks,
        progress: None,
        ..*cx
    };
    let out = match name {
        IMAGE_GENERATE | IMAGE_EDIT => image::run(state, name, args, &offer, cx).await,
        IMAGE_UPSCALE => upscale::run(state, args, &offer, cx).await,
        AUDIO_SPEAK => audio::speak(state, args, &offer, cx).await,
        AUDIO_TRANSCRIBE => audio::transcribe(state, args, &offer, cx).await,
        other => return Err(CallError::ToolNotFound(other.to_string())),
    };
    Ok(out.unwrap_or_else(|e| Outcome::error(name, e)))
}

/// One `/mcp` call (design §3.5's right column): the result as MCP has it,
/// the pictures and the audio inline. No thread, so `att:N` is refused.
/// `progress` follows it when the client asked (§7.2); `stop` is raised by
/// its `notifications/cancelled` (§7.3).
pub(crate) async fn call_mcp(
    state: &SharedState,
    (name, args): (&str, Option<Map<String, Value>>),
    ctx: &RequestCtx,
    progress: Option<&dyn Progress>,
    stop: Option<&StopSignal>,
) -> Result<Value, CallError> {
    let hooks = crate::proxy::image_call::NoHooks;
    // No thread, and a request of its own: Settings → Chat's defaults as
    // they are now.
    let cx = CallCx {
        ctx,
        proto: ClientProto::OpenaiChat,
        refs: &NoThread,
        hooks: &hooks,
        progress,
        stop,
        defaults: None,
    };
    let out = run(state, name, &args.unwrap_or_default(), &cx).await?;
    Ok(out.mcp_result())
}
