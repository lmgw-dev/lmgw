//! `POST /v1/messages/count_tokens`: the Anthropic SDKs'
//! `messages.count_tokens()` (api-docs design §5.2).
//!
//! A thin adapter: Anthropic's request and `{"input_tokens": N}` answer, and
//! its error dialect, outside; inside, the universal counter's own machinery
//! (`count.rs`) plus the backend's native whole-request count where it has
//! one. By route:
//!
//! - **Anthropic protocol:** the client's body, passed through to the
//!   provider's own `/v1/messages/count_tokens` — exact, server tools
//!   included.
//! - **Gemini protocol:** `:countTokens` on the whole translated request —
//!   exact.
//! - **A local llama.cpp chat row:** the rendered chat template counted on
//!   the running server, as the request gate counts it — exact for text;
//!   images at the projector's per-image bound (`media_bound`) or not at all
//!   (`media_omitted`).
//! - **Everything else** (a remote llama-server, an aux row, a generic
//!   OpenAI-compatible provider): the request flattened to text and counted
//!   by the universal counter — `flattened`, plus that route's own flags.

use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::config::{Protocol, Route, UpstreamKind};
use crate::error::GatewayError;
use crate::gate::{GateHeaders, MediaParts, OpenFailed, Opened, RouteCheck};
use crate::ingress::anthropic::{parse_messages_request, parse_reasoning};
use crate::ingress::ClientProto;
use crate::ir::{
    flatten_tool_result, tool_image_placeholder, ChatRequest, ContentPart, Params, Role,
    ToolResultBlock, TEXT_ONLY_SLOT,
};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::*;

/// The dialect every answer of this route is in, errors included.
const PROTO: ClientProto = ClientProto::AnthropicMessages;

/// `POST /v1/messages/count_tokens` (api-docs design §5.2): `{"input_tokens":
/// N}`, with the gate's headers (`x-lmgw-fallback`, its reason, the
/// candidate) and `x-lmgw-count-approximate` when the number is not the
/// backend's exact count of this request.
///
/// Like `/v1/count_tokens`, a cheap count is not written to the request log;
/// a failed one is — a key refused by its alias scope included (§5.4) — and
/// so is one that brought its model up, or whose client went away first
/// (`Unanswered`). No budget applies (§12 entry 15). Nothing is reserved in
/// a pool, nothing is clamped and no rung is stamped: a count is not a send.
pub async fn handle_messages_count_tokens(
    state: SharedState,
    ctx: RequestCtx,
    body: Value,
) -> Response {
    let proto = PROTO;
    let mut row = counter_row(&state, proto, &ctx, &body);
    let Some(alias) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let e = GatewayError::BadRequest("missing 'model'".into());
        row.failed(&e).await;
        return error_response(proto, &e);
    };
    if let Some(e) = counter_policy(&state, &ctx, &alias) {
        row.failed(&e).await;
        return error_response(proto, &e);
    }
    let parsed = parse_messages_request(&body);
    let Opened {
        route,
        hold,
        headers,
    } = match open_for_count(&state, &ctx, &alias, parsed.as_ref(), &mut row).await {
        Ok(o) => o,
        Err((headers, e)) => {
            row.failed(&e).await;
            return headers.stamp(error_response(proto, &e));
        }
    };
    let counted = count_messages_route(&state, &ctx, &body, parsed, &route, hold.as_ref()).await;
    // The count is read: the container no longer has to stay up for it.
    drop(hold);
    match counted {
        Ok(count) => {
            row.answered(200, None).await;
            stamp_count(
                headers.stamp(axum::Json(json!({"input_tokens": count.tokens})).into_response()),
                &count,
            )
        }
        // A count that failed on the fallback still names it ([`Failed`]).
        Err(e) => {
            row.failed(&e).await;
            headers.stamp(error_response(proto, &e))
        }
    }
}

/// The gate's per-request half, as `/v1/messages` runs it (review R1 #2,
/// #3): a local row that is not running is admitted and started, a held one
/// is served by its fallback or refused with `gpu_hold` — and the request is
/// parsed before any of that, so a body only an Anthropic route could count
/// is refused before it can evict or cold-load anything.
///
/// `parsed` is the body read into the IR. When it reads, the request's facets
/// go to [`crate::gate::Routed::using`] exactly as `/v1/messages`' do: a
/// candidate alias refuses a facet it does not enable. When it does not
/// read (a server tool, a block the IR has no part for), only a route that
/// resolved to the Anthropic protocol goes on — the provider reads the body
/// itself, and lmgw, unable to read it, states no facets for it. Anything
/// else is the parse error's 400, with whatever the resolve already settled
/// (a hold's fallback) named on it. A refusal comes back with the headers to
/// stamp on it; `row` learns the route and whether the count brought its
/// model up, as [`admit_counter`]'s does.
async fn open_for_count(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    parsed: Result<&ChatRequest, &GatewayError>,
    row: &mut Unanswered,
) -> Result<Opened, (GateHeaders, GatewayError)> {
    let refused = |f: OpenFailed| (f.headers, f.error);
    let routed = crate::gate::resolve(state, alias, RouteCheck::Text("/v1/messages/count_tokens"))
        .await
        .map_err(refused)?;
    let routed = match parsed {
        Ok(ir) => routed
            .using(crate::gate::request_facets(
                ir,
                ctx.reasoning_control().as_ref(),
            ))
            .map_err(refused)?,
        Err(e) if routed.resolved().upstream.protocol != Protocol::Anthropic => {
            return Err((routed.headers().clone(), e.clone()));
        }
        Err(_) => routed,
    };
    admit_routed(state, routed, false, Some(row)).await
}

/// The count itself, on the route the gate settled on — the module doc's
/// table. The Anthropic route passes the body through and never reads
/// `parsed`; every other one counts the IR, and a body that did not parse is
/// the client's 400 there — [`open_for_count`] already refused it before
/// admission, unless admission swapped an Anthropic route for one of another
/// protocol. What is counted is what the send would carry: on a fallback
/// that cannot see, the IR with its images as placeholders
/// (`gate::fallback_images`), and on an Anthropic route the client's body
/// with the same placeholders in it ([`unseen`]).
async fn count_messages_route(
    state: &SharedState,
    ctx: &RequestCtx,
    body: &Value,
    parsed: Result<ChatRequest, GatewayError>,
    route: &Route,
    hold: Option<&LocalHold>,
) -> Result<Count, GatewayError> {
    match route.upstream.protocol {
        Protocol::Anthropic => {
            let mut body = body.clone();
            // A Gemini step's thought signatures in the call ids stay with
            // Gemini, and out of the count (gateway design §7.1).
            crate::ir::wire_call_ids_in_messages_body(&mut body);
            with_lmgw_reasoning(&mut body, ctx, route);
            let requested = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let requested = requested.to_string();
            unseen::counted(state, route, &requested, &mut body).await;
            let n = send_count(route, hold, |r| {
                crate::egress::anthropic::count_messages_request(
                    &state.http,
                    &r.upstream,
                    &r.upstream_model,
                    &body,
                    &ctx.anthropic_beta,
                )
            })
            .await?;
            Ok(Count::exact(n))
        }
        Protocol::Gemini => {
            let parsed = parsed?;
            let ir = crate::gate::fallback_images::counted(state, route, &parsed).await;
            let params = resolve_params(&ir, ctx, route);
            let n = send_count(route, hold, |r| {
                crate::egress::gemini::count_chat_request(
                    &state.http,
                    &r.upstream,
                    &r.upstream_model,
                    &ir,
                    &params,
                )
            })
            .await?;
            Ok(Count::exact(n))
        }
        Protocol::Openai | Protocol::LlamaCpp => {
            let parsed = parsed?;
            let ir = crate::gate::fallback_images::counted(state, route, &parsed).await;
            // Only a local chat container is counted on its template: a remote
            // llama-server may want the upstream's key, which the gate's
            // count does not send, and an aux row has no chat template
            // (api-docs design §10 choice 5).
            let local_chat = hold.filter(|h| {
                route.upstream.kind == UpstreamKind::LlamaServer && h.class() == Class::Chat
            });
            match local_chat {
                Some(hold) => count_on_template(state, ctx, &ir, route, hold).await,
                None => {
                    let mut count =
                        count_text_route(state, &flatten_for_count(&ir), route, hold).await?;
                    count.flag(Approx::Flattened);
                    let media = crate::gate::media_parts(&ir);
                    if media.images > 0 || media.audio > 0 {
                        count.flag(Approx::MediaOmitted);
                    }
                    Ok(count)
                }
            }
        }
    }
}

/// A local chat row: `/apply-template` on the exact body the llama.cpp egress
/// would send, then `/tokenize` with the completion path's own flags — the
/// request gate's count ([`crate::gate::count_chat_prompt`]), measured equal
/// to a real completion's `usage.prompt_tokens`.
///
/// The body is the one the send would carry: the tool-image decision is made
/// here as the fit makes it, by the same function on the same inputs (llama
/// egress design §3.2, [`crate::gate::tool_images::decide`]), and where it
/// lets tool images go they are media like the user's.
///
/// The gate refuses media it cannot bound; a counter reports it instead. An
/// image is added at the projector's per-image bound when the row has one
/// ([`Approx::MediaBound`]); without one, and for audio, the part is not in
/// the number ([`Approx::MediaOmitted`]) — and not in what the template is
/// asked to render either ([`without_media`]): llama-server refuses an image
/// on a row that loads no projector, and audio on one whose projector does
/// not hear, with a 500, so a part the count only had to say it left out made
/// the whole count a 502 (review R1 #1). A tool image the decision keeps back
/// is no media at all: it goes as its placeholder, which the template renders
/// and the count includes.
async fn count_on_template(
    state: &SharedState,
    ctx: &RequestCtx,
    ir: &ChatRequest,
    route: &Route,
    hold: &LocalHold,
) -> Result<Count, GatewayError> {
    let guard = crate::gate::guard_facts(Some(hold));
    let decided =
        crate::gate::tool_images::decide(state, Some(hold), route, ir, guard.as_deref()).await;
    let route = crate::gate::tool_images::on_route(route, decided.llama.as_ref());
    let tool_images_go = decided
        .llama
        .as_ref()
        .is_some_and(|l| l.tool_images.allowed());

    let user = crate::gate::media_parts(ir);
    let media = MediaParts {
        images: user.images.saturating_add(decided.images),
        ..user
    };
    let bound = match (media.images, decided.bound) {
        (0, _) => None,
        (_, Some(read)) => read.ok().flatten(),
        (_, None) => match hold.gate_facts() {
            Some(facts) => crate::gate::image_token_bound(&facts.models_dir, facts.projector_row())
                .await
                .ok()
                .flatten(),
            None => None,
        },
    };
    let omit_images = media.images > 0 && bound.is_none();
    let stripped;
    let rendered = if omit_images || media.audio > 0 {
        stripped = without_media(ir, omit_images, tool_images_go);
        &stripped
    } else {
        ir
    };

    let params = resolve_params(ir, ctx, &route);
    let http = &state.http;
    let (route, params) = (&route, &params);
    // Rendered per attempt, as a send renders it: a count retried on a
    // recovered container carries that container's tool-image verdict
    // (`gate::tool_images::recheck`).
    let prompt = crate::gate::on_running_server(route, hold, move |root| {
        let attempt = hold.on_attempt(route);
        let body = crate::egress::llama_cpp::chat_body(
            rendered,
            &route.upstream_model,
            params,
            false,
            &attempt.upstream,
        );
        async move {
            crate::gate::count_chat_prompt(http, &root, &body, MediaParts::default(), None).await
        }
    })
    .await?;

    let mut count = Count::exact(prompt.text_tokens);
    if let Some(b) = bound {
        count.tokens = count
            .tokens
            .saturating_add(media.images.saturating_mul(b.tokens));
        count.flag(Approx::MediaBound);
    }
    if omit_images || media.audio > 0 {
        count.flag(Approx::MediaOmitted);
    }
    Ok(count)
}

/// `ir` without the media [`count_on_template`] leaves out of its number:
/// every audio part (no row has a per-audio bound), and every image too when
/// `images` (the row gives no per-image bound). A turn left with no parts
/// renders as empty text, as the llama.cpp egress renders any text-only turn.
///
/// A tool result's image is media only where the send's decision lets it go
/// (`tool_images`, llama egress design §8.2); left out, it becomes today's
/// placeholder text in place, so the template is never asked to render an
/// image the number does not hold. One the decision keeps back stays as it
/// is: the egress renders it as its own placeholder, as the send will. A tool
/// result's audio is a placeholder on every rendering already.
fn without_media(ir: &ChatRequest, images: bool, tool_images: bool) -> ChatRequest {
    let mut ir = ir.clone();
    for m in &mut ir.messages {
        m.content.retain(|p| match p {
            ContentPart::Audio { .. } => false,
            ContentPart::Image { .. } => !images,
            _ => true,
        });
        if !(images && tool_images) {
            continue;
        }
        for p in &mut m.content {
            let ContentPart::ToolResult { content, .. } = p else {
                continue;
            };
            for block in content.iter_mut() {
                if let ToolResultBlock::Image { mime, data } = block {
                    let text = tool_image_placeholder(mime, data, TEXT_ONLY_SLOT);
                    *block = ToolResultBlock::text(text);
                }
            }
        }
    }
    ir
}

/// lmgw's own reasoning tiers — the `x-lmgw-reasoning*` headers and the
/// alias's default — rendered into a passed-through Anthropic body the way
/// `/v1/messages` renders them ([`resolve_params`], then the Anthropic
/// egress's own `apply_reasoning`).
///
/// Without either tier the body stays the client's: every key as it was sent,
/// only `model` rewritten by the egress. With one, the count is made for the
/// thinking `/v1/messages` would actually send, not for the one the client
/// wrote — otherwise a documented header would be silently ignored on exactly
/// the route that promises an exact count. Only the thinking is replaced: the
/// rest of the client's `output_config` (a `format`) is part of the request
/// being counted, and `effort` is merged into it rather than taking its place
/// (review R3 #6).
fn with_lmgw_reasoning(body: &mut Value, ctx: &RequestCtx, route: &Route) {
    if ctx.reasoning_control().is_none() && route.param_defaults.reasoning.is_none() {
        return;
    }
    let tiers = ChatRequest {
        model_alias: String::new(),
        messages: Vec::new(),
        params: Params {
            reasoning: parse_reasoning(body),
            ..Params::default()
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let control = resolve_params(&tiers, ctx, route).reasoning_control();
    let Some(obj) = body.as_object_mut().filter(|_| !control.is_empty()) else {
        return;
    };
    obj.remove("thinking");
    // `apply_reasoning` builds a fresh body's `output_config`, so the client's
    // is taken out first and merged back around whatever it sets. One that
    // is not an object stays where it was, for the provider to refuse.
    let kept = match obj.remove("output_config") {
        Some(Value::Object(mut config)) => {
            config.remove("effort");
            Some(config)
        }
        Some(other) => {
            obj.insert("output_config".into(), other);
            None
        }
        None => None,
    };
    crate::egress::anthropic::apply_reasoning(obj, &control);
    if let Some(mut config) = kept {
        if let Some(Value::Object(set)) = obj.remove("output_config") {
            config.extend(set);
        }
        if !config.is_empty() {
            obj.insert("output_config".into(), Value::Object(config));
        }
    }
}

/// The request as plain text, for a backend that can only count a string
/// (api-docs design §5.2): joined with blank lines, in order — the system
/// text; each tool as `name`, `description` and its schema's JSON on lines of
/// their own; each message as `role: ` followed by its text and reasoning,
/// each tool call as `name(<args JSON>)` and each tool result's text (as the
/// OpenAI egress flattens it). Media parts contribute nothing; the caller
/// flags them.
pub(crate) fn flatten_for_count(ir: &ChatRequest) -> String {
    let mut blocks: Vec<String> = Vec::new();
    if let Some(system) = ir.system_text() {
        blocks.push(system);
    }
    for t in &ir.tools {
        let mut lines = vec![t.name.clone()];
        if let Some(d) = t.description.as_deref().filter(|d| !d.is_empty()) {
            lines.push(d.to_string());
        }
        lines.push(t.parameters.to_string());
        blocks.push(lines.join("\n"));
    }
    for m in ir.non_system_messages() {
        let parts: Vec<String> = m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
                    Some(text.clone())
                }
                ContentPart::ToolUse { name, args, .. } => Some(format!("{name}({args})")),
                ContentPart::ToolResult { content, .. } => Some(flatten_tool_result(content).0),
                ContentPart::Image { .. } | ContentPart::Audio { .. } => None,
            })
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        blocks.push(format!("{role}: {}", parts.join("\n")));
    }
    blocks.join("\n\n")
}

mod unseen;

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(body: Value) -> ChatRequest {
        parse_messages_request(&body).unwrap()
    }

    #[test]
    fn flatten_for_count_orders_system_tools_then_messages() {
        let text = flatten_for_count(&ir(json!({
            "model": "m",
            "system": "Be brief.",
            "tools": [{"name": "get_weather", "description": "Weather by city",
                       "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "Weather in Oslo?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Use the tool.", "signature": "s"},
                    {"type": "tool_use", "id": "t1", "name": "get_weather",
                     "input": {"city": "Oslo"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "4 °C"}
                ]}
            ]
        })));
        assert_eq!(
            text,
            "Be brief.\n\n\
             get_weather\nWeather by city\n{\"type\":\"object\"}\n\n\
             user: Weather in Oslo?\n\n\
             assistant: Use the tool.\nget_weather({\"city\":\"Oslo\"})\n\n\
             tool: 4 °C"
        );
    }

    #[test]
    fn flatten_for_count_leaves_media_out() {
        let text = flatten_for_count(&ir(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                             "data": "iVBORw0KGgo="}},
                {"type": "text", "text": "What is this?"}
            ]}]
        })));
        assert_eq!(text, "user: What is this?");
    }

    /// Audio cannot arrive in Anthropic's dialect today, so the stripping is
    /// pinned here on a hand-built request: audio always goes, images only
    /// when asked, and every other part stays where it was.
    #[test]
    fn without_media_drops_audio_always_and_images_when_asked() {
        let mut req = ir(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                             "data": "iVBORw0KGgo="}},
                {"type": "text", "text": "Listen and look."}
            ]}]
        }));
        req.messages[0].content.push(ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "UklGRg==".into(),
        });

        let kept = without_media(&req, false, false);
        assert_eq!(
            crate::gate::media_parts(&kept),
            MediaParts {
                images: 1,
                audio: 0
            }
        );
        assert_eq!(kept.messages[0].content.len(), 2);

        let text_only = without_media(&req, true, false);
        assert_eq!(crate::gate::media_parts(&text_only), MediaParts::default());
        assert_eq!(
            text_only.messages[0].content,
            vec![ContentPart::text("Listen and look.")]
        );
        // The rendered turn is plain text, as llama-server takes on any row.
        let body = crate::egress::llama_cpp::chat_body(
            &text_only,
            "m",
            &Params::default(),
            false,
            &crate::config::Snapshot::default().router_upstream(),
        );
        assert_eq!(body["messages"][0]["content"], "Listen and look.");
    }

    /// Leaving images out where the decision lets tool images go, a tool
    /// result's image becomes today's placeholder in place: the body renders
    /// as today's even on a route that sends images. Kept, or kept back by
    /// the decision, it stays an image block for the renderer.
    #[test]
    fn without_media_leaves_tool_images_out_as_their_placeholder() {
        // A whole PNG: one stb_image decodes, so the route sends it.
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = ir(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "shot", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "chart:"},
                        {"type": "image", "source": {"type": "base64",
                                                     "media_type": "image/png",
                                                     "data": PNG}}
                    ]}
                ]}
            ]
        }));
        let tool_blocks = |r: &ChatRequest| match &r.messages[1].content[0] {
            ContentPart::ToolResult { content, .. } => content.clone(),
            other => panic!("not a tool result: {other:?}"),
        };

        assert_eq!(
            tool_blocks(&without_media(&req, false, true)),
            tool_blocks(&req)
        );
        assert_eq!(
            tool_blocks(&without_media(&req, true, false)),
            tool_blocks(&req)
        );

        let stripped = without_media(&req, true, true);
        assert_eq!(
            tool_blocks(&stripped),
            vec![
                ToolResultBlock::text("chart:"),
                ToolResultBlock::text(
                    "[image/png image, 96 base64 bytes — omitted: this upstream's tool-result \
                     slot is text-only]"
                ),
            ]
        );
        let mut up = crate::config::Snapshot::default().router_upstream();
        let today = |r: &ChatRequest, up: &crate::config::Upstream| {
            serde_json::to_string(&crate::egress::llama_cpp::chat_body(
                r,
                "m",
                &Params::default(),
                false,
                up,
            ))
            .unwrap()
        };
        let todays = today(&req, &up);
        up.llama = Some(std::sync::Arc::new(crate::config::LlamaRoute {
            facts: std::sync::Arc::new(crate::egress::llama_cpp::props::LlamaFacts {
                vision: Some(true),
                ..Default::default()
            }),
            tool_images: crate::config::ToolImages::Allowed,
        }));
        assert_ne!(today(&req, &up), todays, "the route sends the image");
        assert_eq!(today(&stripped, &up), todays);
    }

    #[test]
    fn flatten_for_count_skips_a_tool_without_description_line() {
        let text = flatten_for_count(&ir(json!({
            "model": "m",
            "tools": [{"name": "ping", "input_schema": {}}],
            "messages": [{"role": "user", "content": "hi"}]
        })));
        assert_eq!(text, "ping\n{}\n\nuser: hi");
    }
}
