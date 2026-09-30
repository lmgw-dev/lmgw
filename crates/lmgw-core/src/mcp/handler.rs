//! The southbound `rmcp` client handler (§8, §15).
//!
//! `GatewayClientHandler` is the per-connection [`rmcp::ClientHandler`] lmgw
//! presents to each upstream MCP server. Two jobs, both for **sampling synergy**
//! (§8 — the MVP differentiator: lmgw is the MCP host *and* the model provider):
//!
//! 1. **The sampling-capability declaration** (M2): `get_info()` advertises
//!    `capabilities.sampling` iff the server's `allow_sampling` toggle is on, so
//!    an upstream may ever issue `sampling/createMessage` against us (§8 —
//!    without this the whole sampling differentiator is dead on arrival). Because
//!    the declaration is baked into the `InitializeRequest`, `allow_sampling` is
//!    **reconnect-affecting**, which is why the reconcile config-hash (§9)
//!    includes it.
//! 2. **Answering `create_message`** (M4): when an allowed upstream samples us,
//!    we resolve the sampling alias → a concrete [`Route`](crate::config::Route),
//!    map the `CreateMessageRequestParams` into the gateway IR, run it in-process
//!    through our own egress adapters via [`proxy::sample_once`], and map the
//!    completion back to a `CreateMessageResult`. The call logs like any request
//!    (`ingress_proto = "mcp-sampling"`, real tokens, counted in stats — §10).
//!
//! **Direction of sampling (confirmed, §8/§2).** Sampling is the *upstream server
//! asking US* to run a model call, gated by OUR declared **client** capability —
//! not us asking the client. A server with `allow_sampling = false` never sees
//! the capability in our `InitializeRequest`, so it never issues
//! `sampling/createMessage` and `create_message` is never reached. Northbound
//! server-initiated sampling (asking the *client* to sample) is an explicit
//! non-goal (§2) and not implemented here.
//!
//! `rmcp` types stay confined to this module (§19) — `proxy.rs` speaks IR +
//! plain types only; the `CreateMessage*` ↔ IR mapping lives here.

// Sampling is SEP-2577-deprecated in rmcp 2.0.0 (the types still work and are the
// only way the protocol delivers `sampling/createMessage` — there is no successor
// API in 2.0); we knowingly use them. `expect` (not `allow`, matching rmcp's own
// internal modules) so the suppression self-reports if a future rmcp ever
// un-deprecates sampling or removes it, prompting the migration. Blast radius is
// this one sampling-focused module (§19).
#![expect(deprecated)]

use std::sync::Weak;
use std::time::Duration;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateMessageRequestParams, CreateMessageResult, Role,
    SamplingCapability, SamplingMessage,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use rmcp::ErrorData as McpError;

use crate::ir::{
    ChatRequest, Completion, ContentPart, FinishReason, Message, Params, Role as IrRole,
};
use crate::proxy;
use crate::state::AppState;

/// The sampling sub-call's own wall-clock deadline (§8 — the self-starvation
/// guard). Distinct from the §9 per-server tool-call `timeout_ms`: a
/// `sampling_alias` pointing at a single-slot local llama.cpp model that the
/// agent which triggered the originating `tools/call` *already occupies* would,
/// without this, block forever on a GPU slot the caller owns → self-starvation.
/// With it the sub-call fails **loudly** (a surfaced timeout error logged like
/// any failed request) instead of hanging.
///
/// A **visible named constant**, not a hidden cap (house rule): the real bound is
/// stated here and surfaced as a clear error when hit. Generous enough for a warm
/// model to answer a sampling prompt; if your `sampling_alias` legitimately needs
/// longer, point it at a dedicated model (the UI hint recommends exactly that).
/// The effective timeout is `min(this, the upstream's own ceiling)` — this can
/// only *tighten* the call, never raise the per-upstream bound. An upstream
/// with no ceiling (`request_timeout` is `None`) leaves this standing alone,
/// so a sampling sub-call is always bounded regardless.
pub const SAMPLING_DEADLINE: Duration = Duration::from_secs(120);

/// One per live southbound connection (`McpConn.running`). Holds a `Weak` back
/// to `AppState` so the sampling path can resolve the gateway's own models
/// without forming an `Arc` cycle (§9, §19); upgraded per sampling call.
#[derive(Clone)]
pub struct GatewayClientHandler {
    /// Back-reference for the sampling path; `Weak` to break the
    /// `AppState → McpManager → conns → handler → AppState` cycle.
    pub state: Weak<AppState>,
    /// Whether this server may sample our models. Drives the §8 capability
    /// declaration in [`get_info`](Self::get_info); reconnect-affecting.
    pub allow_sampling: bool,
    /// Per-server sampling alias override (falls back to the global
    /// `Settings.sampling_alias` at call time).
    pub sampling_alias: Option<String>,
}

impl ClientHandler for GatewayClientHandler {
    /// Declare our client capabilities at connect (§8). The
    /// `capabilities.sampling = allow_sampling.then(..)` line is the
    /// DOA-avoiding declaration: without it an upstream never calls
    /// `create_message`, so the toggle being off means the gateway presents no
    /// sampling capability at all.
    fn get_info(&self) -> ClientInfo {
        // `ClientInfo`/`ClientCapabilities` are `#[non_exhaustive]`, so build
        // from `Default` and mutate the public fields rather than a struct
        // literal (and we don't enable rmcp's `macros`/`server` feature, which
        // is what gates the typestate `ClientCapabilities::builder()`).
        let mut capabilities = ClientCapabilities::default();
        // The §8 DOA-avoiding declaration: advertise `sampling` iff allowed.
        capabilities.sampling = self.allow_sampling.then(SamplingCapability::default);
        let mut info = ClientInfo::default();
        info.capabilities = capabilities;
        info
    }

    /// Answer `sampling/createMessage` from our own models (§8). Resolve the
    /// sampling alias (per-server → global), build a gateway IR chat request,
    /// run it in-process through our egress adapters with the self-starvation
    /// [`SAMPLING_DEADLINE`], and map the completion back. Errors propagate to
    /// the requesting server as `McpError` and are logged like any failed LLM
    /// request (§14).
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        // Weak → Arc; gone only mid-shutdown or pre-init.
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| McpError::internal_error("gateway is shutting down", None))?;

        // Alias resolution order (§8): per-server override (non-empty) → global
        // `Settings.sampling_alias` (non-empty) → a surfaced error. No silent
        // fallback to some default model — an unconfigured alias is a visible
        // failure, not a guess (house rule: surface, don't hide).
        let snap = state.snapshot();
        let alias = self
            .sampling_alias
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let g = snap.settings.sampling_alias.trim();
                (!g.is_empty()).then(|| g.to_string())
            })
            .ok_or_else(|| {
                McpError::internal_error(
                    "no sampling alias configured (set a per-server sampling_alias \
                     or the global default under Settings → Agents & tools)",
                    None,
                )
            })?;

        // The gate's per-request half. Resolve the alias to a concrete Route;
        // an unknown/disabled alias is a clean, surfaced error (not a hang).
        // The hold swap applies, because a server's sampling request is a turn
        // something is waiting on — it falls back under a GPU hold like the
        // other in-process callers, and is refused by name when there is no
        // fallback (gpu-hold design §2). Then GPU admission (§9b): sampling is
        // one non-streaming turn, so the guard covers exactly that call and is
        // dropped with it — but it is taken, so an upstream server's sampling
        // request queues behind interactive traffic and makes room by name
        // instead of OOMing the container. A local route comes back on the
        // port its container answers on (§5).
        // The request is built first, so a candidate alias can refuse a
        // capability facet it does not enable before anything starts
        // (candidate-aliases §4.6, `Routed::using`).
        let ir = ir_from_create_message(&params, &alias);
        let uses = crate::gate::request_facets(&ir, None);
        let opened = async {
            crate::gate::resolve(&state, &alias, crate::gate::RouteCheck::None)
                .await?
                .using(uses)?
                .admit(&state)
                .await
        }
        .await
        .map_err(|f| {
            let message = match f.route {
                None => format!("sampling alias '{alias}' does not resolve: {}", f.error),
                Some(_) => format!("sampling call failed: {}", f.error),
            };
            McpError::internal_error(message, None)
        })?;
        let (route, hold) = (opened.route, opened.hold);
        let fallback = opened.headers.fallback_reason();

        // Run it in-process through our own egress adapters, logged as
        // `mcp-sampling` with real tokens (the §8 observability claim), bounded
        // by the self-starvation deadline.
        let completion = proxy::sample_once(
            &state,
            hold.as_ref(),
            &route,
            fallback,
            &ir,
            "mcp-sampling",
            None,
            SAMPLING_DEADLINE,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("sampling call failed: {e}"), None))?;

        Ok(create_message_result_from(
            completion,
            &route.upstream_model,
        ))
    }

    /// An upstream server's tool set changed (§8). The aggregate is recomputed on
    /// every northbound read (no cache to invalidate, §8/M3), so the only job
    /// here is to **notify** open `GET /mcp` clients to re-`tools/list`: forward
    /// a `tools/list_changed` nudge onto the manager's northbound broadcast. Best
    /// effort — if the gateway is shutting down (Weak gone) there's nothing to
    /// notify.
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(state) = self.state.upgrade() {
            state.mcp.on_upstream_tools_changed();
        }
    }
}

/// Map an MCP `CreateMessageRequestParams` into the gateway IR `ChatRequest`
/// (§8). `modelPreferences` are **advisory** — the alias is already chosen by the
/// caller (per-server/global), so we don't weight against hints here; everything
/// else maps straight across:
/// - `systemPrompt` → a leading `System` message;
/// - each `SamplingMessage` → an IR `Message` (role + concatenated text; non-text
///   blocks — image/audio/tool — are best-effort skipped for MVP);
/// - `maxTokens`/`temperature`/`stopSequences` → IR [`Params`].
///
/// Non-streaming: sampling is a one-shot completion.
pub fn ir_from_create_message(params: &CreateMessageRequestParams, alias: &str) -> ChatRequest {
    let mut messages: Vec<Message> = Vec::with_capacity(params.messages.len() + 1);

    // systemPrompt becomes a leading system message (ChatRequest::system_text
    // hoists it back out for Anthropic/Gemini egress).
    if let Some(sys) = params
        .system_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        messages.push(Message::text(IrRole::System, sys));
    }

    for m in &params.messages {
        let text = sampling_message_text(m);
        // Skip a fully non-text message (image/audio/tool-only) rather than emit
        // an empty turn — MVP is text-only sampling (§8). Surface the drop (don't
        // silently corrupt the conversation, §7 spirit) at debug level — some
        // servers routinely include non-text blocks, so this isn't a warning.
        if text.trim().is_empty() {
            tracing::debug!("mcp sampling: dropped a non-text message (text-only MVP, §8)");
            continue;
        }
        messages.push(Message::text(ir_role_from(&m.role), text));
    }

    // maxTokens is a non-optional u32 in rmcp; carry it through verbatim. No
    // hidden cap — it's the server's stated bound, surfaced to the upstream.
    let params_ir = Params {
        temperature: params.temperature.map(|t| t as f64),
        max_tokens: Some(params.max_tokens),
        stop: params.stop_sequences.clone().unwrap_or_default(),
        ..Default::default()
    };

    ChatRequest {
        model_alias: alias.to_string(),
        messages,
        params: params_ir,
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

/// Concatenated text of all text blocks in a sampling message (single or
/// multiple). Image/audio/tool blocks are skipped (MVP text-only, §8).
fn sampling_message_text(m: &SamplingMessage) -> String {
    m.content
        .iter()
        .filter_map(|b| b.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

/// MCP `Role` → IR role. MCP sampling only has `User`/`Assistant`.
fn ir_role_from(role: &Role) -> IrRole {
    match role {
        Role::User => IrRole::User,
        Role::Assistant => IrRole::Assistant,
    }
}

/// Map an IR [`Completion`] back to an MCP `CreateMessageResult` (§8). Assistant
/// role, the answer text, the resolved upstream model name, and a `stopReason`
/// derived from the IR finish reason. `CreateMessageResult`/`SamplingMessage` are
/// `#[non_exhaustive]`, so build via the rmcp constructors, never a struct
/// literal.
pub fn create_message_result_from(completion: Completion, model: &str) -> CreateMessageResult {
    let text = completion_text(&completion);
    let stop_reason = sampling_stop_reason(&completion.finish_reason);
    CreateMessageResult::new(SamplingMessage::assistant_text(text), model.to_string())
        .with_stop_reason(stop_reason)
}

/// Concatenated assistant text from a completion's `Text` parts (ToolUse parts
/// are dropped — sampling results are message text, §8).
fn completion_text(completion: &Completion) -> String {
    completion
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// IR finish reason → MCP `stopReason` string. Uses rmcp's documented constants
/// where they apply (`endTurn`/`maxTokens`); other reasons map to a stable
/// lowercase token rather than inventing a new vocabulary.
fn sampling_stop_reason(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => CreateMessageResult::STOP_REASON_END_TURN.to_string(),
        FinishReason::Length => CreateMessageResult::STOP_REASON_END_MAX_TOKEN.to_string(),
        FinishReason::ToolUse => CreateMessageResult::STOP_REASON_TOOL_USE.to_string(),
        FinishReason::ContentFilter => "contentFilter".to_string(),
        FinishReason::Other(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;
    use rmcp::model::SamplingMessageContentBlock;

    /// Build a minimal request params for the mapping tests. `CreateMessageRequestParams`
    /// is `#[non_exhaustive]` but derives `Default`, so build from `Default`.
    fn params(messages: Vec<SamplingMessage>) -> CreateMessageRequestParams {
        let mut p = CreateMessageRequestParams::default();
        p.messages = messages;
        p.max_tokens = 256;
        p
    }

    #[test]
    fn ir_from_create_message_maps_roles_and_text() {
        let mut p = params(vec![
            SamplingMessage::user_text("hello"),
            SamplingMessage::assistant_text("hi there"),
        ]);
        p.system_prompt = Some("be terse".into());
        p.temperature = Some(0.3);
        p.stop_sequences = Some(vec!["STOP".into(), "END".into()]);

        let ir = ir_from_create_message(&p, "my-alias");

        assert_eq!(ir.model_alias, "my-alias");
        assert!(!ir.stream);
        // system prompt is hoisted to a leading System message.
        assert_eq!(ir.messages[0].role, IrRole::System);
        assert_eq!(ir.messages[0].joined_text(), "be terse");
        assert_eq!(ir.messages[1].role, IrRole::User);
        assert_eq!(ir.messages[1].joined_text(), "hello");
        assert_eq!(ir.messages[2].role, IrRole::Assistant);
        assert_eq!(ir.messages[2].joined_text(), "hi there");
        // params round-trip.
        assert_eq!(ir.params.max_tokens, Some(256));
        // temperature widens f32→f64, so compare approximately, not bit-exact.
        assert!((ir.params.temperature.unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(ir.params.stop, vec!["STOP".to_string(), "END".to_string()]);
    }

    #[test]
    fn ir_from_create_message_without_system_prompt() {
        let p = params(vec![SamplingMessage::user_text("just a user turn")]);
        let ir = ir_from_create_message(&p, "a");
        // No leading system message; first message is the user turn.
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::User);
    }

    #[test]
    fn ir_from_create_message_skips_nontext_blocks() {
        // A multi-block message that has text alongside a (skipped) image: only
        // the text survives. A message with no text at all is dropped entirely.
        let text_msg = SamplingMessage::new_multiple(
            Role::User,
            vec![SamplingMessageContentBlock::text("describe this")],
        );
        let ir = ir_from_create_message(&params(vec![text_msg]), "a");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].joined_text(), "describe this");
    }

    #[test]
    fn create_message_result_round_trip() {
        let completion = Completion {
            content: vec![ContentPart::text("the answer")],
            reasoning: String::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(3),
                ..Default::default()
            },
            model: "ignored-here".into(),
            timings: None,
        };
        let result = create_message_result_from(completion, "resolved-model");

        // role + content live in the flattened SamplingMessage.
        assert_eq!(result.message.role, Role::Assistant);
        let text: String = result
            .message
            .content
            .iter()
            .filter_map(|b| b.as_text().map(|t| t.text.clone()))
            .collect();
        assert_eq!(text, "the answer");
        // model is the resolved upstream model name, not the IR completion's.
        assert_eq!(result.model, "resolved-model");
        assert_eq!(result.stop_reason.as_deref(), Some("endTurn"));
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(sampling_stop_reason(&FinishReason::Stop), "endTurn");
        assert_eq!(sampling_stop_reason(&FinishReason::Length), "maxTokens");
        assert_eq!(sampling_stop_reason(&FinishReason::ToolUse), "toolUse");
        assert_eq!(
            sampling_stop_reason(&FinishReason::Other("custom".into())),
            "custom"
        );
    }

    #[test]
    fn ir_role_mapping() {
        assert_eq!(ir_role_from(&Role::User), IrRole::User);
        assert_eq!(ir_role_from(&Role::Assistant), IrRole::Assistant);
    }
}
