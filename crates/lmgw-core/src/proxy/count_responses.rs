//! `POST /v1/responses/input_tokens`: the OpenAI SDKs'
//! `responses.input_tokens.count()` (api-compat design §9 finding 6).
//!
//! A thin adapter, as `/v1/messages/count_tokens` is one (api-docs design
//! §5.1): a Responses create body and the `{"object":
//! "response.input_tokens", "input_tokens": N}` answer, and the Responses
//! error dialect, outside; inside, the body read as `/v1/responses` reads it
//! — its stored conversation in front for a `previous_response_id`, its
//! function tools as the run offers them — and counted by the counter's own
//! machinery on the route the gate settles on
//! ([`super::count_messages::count_ir_route`]): exact on an Anthropic route
//! (the Messages body the send would render, on Anthropic's own counter), a
//! Gemini route and a local llama.cpp chat row; flattened text on every other
//! one, with `x-lmgw-count-approximate` saying so.
//!
//! Not yet: the OpenAI platform's own `/v1/responses/input_tokens` (an
//! `openai` route is counted flattened, with the model's tiktoken encoding),
//! and the definitions of `mcp` tools, which a run lists from their servers
//! when it starts (`tools_omitted`), and the results of the calls a stored
//! response is waiting on (`pending_calls_omitted`). What only an upstream's own Responses
//! API serves (hosted tools, item references) is refused by name.

use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::gate::Opened;
use crate::ingress::ClientProto;
use crate::state::SharedState;

use super::*;

/// The request log's label for the route: the OpenAI wire's.
const PROTO: ClientProto = ClientProto::OpenaiChat;

/// The answer's `object`.
pub const INPUT_TOKENS_OBJECT: &str = "response.input_tokens";

/// An error in the Responses dialect, as `/v1/responses` answers it.
fn refused(e: &GatewayError) -> Response {
    let mut resp = (
        e.http_status(),
        axum::Json(crate::ingress::responses::serialize_error(e)),
    )
        .into_response();
    e.stamp_rate_hints(&mut resp);
    resp
}

/// `POST /v1/responses/input_tokens` (module doc). Written to the request
/// log as the other counters are: a failed count, and one that brought its
/// model up; a cheap one is not. No budget applies (api-docs design §12
/// entry 15), and nothing is reserved or clamped: a count is not a send.
pub async fn handle_responses_input_tokens(
    state: SharedState,
    ctx: RequestCtx,
    body: Value,
) -> Response {
    let mut row = counter_row(&state, PROTO, &ctx, &body);
    let Some(alias) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let e = GatewayError::BadRequest("missing 'model'".into());
        row.failed(&e).await;
        return refused(&e);
    };
    if let Some(e) = counter_policy(&state, &ctx, &alias) {
        row.failed(&e).await;
        return refused(&e);
    }
    let mut req = match crate::ingress::responses::parse_request(&body) {
        Ok(r) => r,
        Err(e) => {
            row.failed(&e).await;
            return refused(&e);
        }
    };
    if let Some(n) = req.native_only.first() {
        let e = GatewayError::Unsupported(format!(
            "{}: /v1/responses/input_tokens does not count what only an upstream's own \
             Responses API serves yet",
            n.what
        ));
        row.failed(&e).await;
        return refused(&e);
    }
    req.ir.anthropic_beta = ctx.anthropic_beta.clone();
    let pending_calls = match crate::responses::with_stored_history(&state, &ctx, &mut req).await {
        Ok(p) => p,
        Err(e) => {
            row.failed(&e).await;
            return refused(&e);
        }
    };
    // The run offers the client's function tools as the IR's tools; the mcp
    // tools it lists when it starts are not counted (module doc).
    req.ir.tools = req.client_tools.clone();
    let mcp_tools = !req.mcp_tools.is_empty();
    let ir = req.ir;

    let Opened {
        route,
        hold,
        headers,
    } = match super::count_messages::open_for_count(&state, &ctx, &alias, Ok(&ir), &mut row).await {
        Ok(o) => o,
        Err((headers, e)) => {
            row.failed(&e).await;
            return headers.stamp(refused(&e));
        }
    };
    let counted =
        super::count_messages::count_ir_route(&state, &ctx, ir, &route, hold.as_ref()).await;
    drop(hold);
    match counted {
        Ok(mut count) => {
            if mcp_tools {
                count.flag(Approx::ToolsOmitted);
            }
            if pending_calls {
                count.flag(Approx::PendingCallsOmitted);
            }
            row.answered(200, None).await;
            stamp_count(
                headers.stamp(
                    axum::Json(json!({
                        "object": INPUT_TOKENS_OBJECT,
                        "input_tokens": count.tokens,
                    }))
                    .into_response(),
                ),
                &count,
            )
        }
        Err(e) => {
            row.failed(&e).await;
            headers.stamp(refused(&e))
        }
    }
}
