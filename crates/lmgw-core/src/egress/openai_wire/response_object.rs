//! What lmgw reads off an OpenAI Responses `Response` object (openai egress
//! design §5.2): the whole body of a `/v1/responses` answer, or the
//! `response` its terminal stream event carries (`response.completed`,
//! `response.incomplete`, `response.failed`). Native passthrough reads it to
//! write its row; the Responses wire for the platform's chat (WP7,
//! `egress/openai/responses/decode.rs`) reads the same object through the
//! same function.

use serde_json::Value;

use super::responses_items::event;
use crate::ir::{UpstreamItem, Usage};
use crate::pricing::HostedCalls;

/// The stream events whose `response` is the finished Response object.
pub(crate) const TERMINAL_EVENTS: [&str; 3] = [event::COMPLETED, event::INCOMPLETE, event::FAILED];

/// What one Response object reported.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ResponseReport {
    /// Its `id`: the upstream's own, by which it can be fetched again.
    pub id: Option<String>,
    /// Its `usage`, in the IR's meanings.
    pub usage: Usage,
    /// The message of the response's `error`, when its `status` is
    /// `failed`: the request reached the model and failed there.
    pub failed: Option<String>,
    /// Its `service_tier`, verbatim: the tier OpenAI says served the request
    /// (§5.2). Whether it prices the row is the route's question
    /// ([`crate::pricing::ServedTier::of`]): a compatible provider's has no
    /// tier dimension.
    pub service_tier: Option<String>,
    /// Its `output`'s billable hosted-tool calls ([`hosted_calls`]): fees
    /// no `usage` carries.
    pub hosted_calls: HostedCalls,
    /// Its `output` items: the ids the upstream stores them under, which a
    /// later input item may name (`responses/references.rs`), each with the
    /// hash of its encrypted content where it has one (WP8 review S-R3).
    pub items: Vec<UpstreamItem>,
}

/// The output item types of a hosted tool that bills beyond its tokens, per
/// OpenAI's pricing page (<https://developers.openai.com/api/docs/pricing.md>,
/// checked 2026-10-24): web search ("$10.00 / 1k calls", $25 for the preview
/// on non-reasoning models), file search ("$2.50 / 1k calls"), Code
/// Interpreter and Hosted Shell (per 20-minute container session), and image
/// generation, billed at the image model's own token rates, which the
/// Response's `usage` does not include (its `usage` has only the input,
/// output, cache and reasoning counts:
/// <https://developers.openai.com/api/reference/resources/responses/methods/create.md>).
///
/// Not billed per call, so not here: `mcp_call` (remote MCP servers and
/// connectors: "No additional fees apply per tool call",
/// <https://developers.openai.com/api/docs/guides/tools-connectors-mcp.md>),
/// and the calls the client runs itself (`function_call`, `computer_call`,
/// `local_shell_call`, `custom_tool_call`, `apply_patch_call`), which cost
/// the model's tokens only. A `shell_call` is hosted unless its environment
/// says `local` ([`is_billable`]).
pub(crate) const BILLABLE_CALLS: [&str; 5] = [
    "web_search_call",
    "file_search_call",
    "code_interpreter_call",
    "image_generation_call",
    "shell_call",
];

/// Whether `item` is a hosted call that bills beyond its tokens
/// ([`BILLABLE_CALLS`]). A `shell_call` ran in a hosted container unless its
/// `environment` is `local`; one that names none counts as hosted, so an
/// unknown fee is surfaced rather than assumed away.
pub(crate) fn is_billable(item: &Value) -> Option<&str> {
    let ty = item.get("type").and_then(Value::as_str)?;
    if !BILLABLE_CALLS.contains(&ty) {
        return None;
    }
    if ty == "shell_call"
        && item.pointer("/environment/type").and_then(Value::as_str) == Some("local")
    {
        return None;
    }
    Some(ty)
}

/// The billable hosted-tool calls in a Response object's `output`.
pub(crate) fn hosted_calls(response: &Value) -> HostedCalls {
    let mut calls = HostedCalls::default();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(ty) = is_billable(item) {
            calls.add(ty, 1);
        }
    }
    calls
}

/// Read `response`, a Responses API `Response` object.
pub(crate) fn read_response(response: &Value) -> ResponseReport {
    let failed = (response.get("status").and_then(Value::as_str) == Some("failed")).then(|| {
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("the response failed upstream")
            .to_string()
    });
    ResponseReport {
        id: response.get("id").and_then(Value::as_str).map(String::from),
        usage: parse_responses_usage(response.get("usage")),
        failed,
        service_tier: response
            .get("service_tier")
            .and_then(Value::as_str)
            .map(String::from),
        hosted_calls: hosted_calls(response),
        items: response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(UpstreamItem::of)
            .collect(),
    }
}

/// A Responses `usage` object as the IR's [`Usage`].
///
/// The meanings match the chat reader's (`decode::parse_usage`):
/// `input_tokens` is the total prompt, both cache counts already inside it
/// (`input_tokens_details.cached_tokens`, `.cache_write_tokens`), and
/// `output_tokens` the total output, reasoning inside it
/// (`output_tokens_details.reasoning_tokens`). Nothing is added up, so a
/// cached prompt is not counted twice; `total_tokens` is not read, since
/// it is the sum of the two.
pub(crate) fn parse_responses_usage(v: Option<&Value>) -> Usage {
    let Some(u) = v.filter(|u| u.is_object()) else {
        return Usage::default();
    };
    let detail = |k: &str, f: &str| u.get(k).and_then(|d| d.get(f)).and_then(Value::as_u64);
    Usage {
        prompt_tokens: u.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: u.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: detail("input_tokens_details", "cached_tokens"),
        cache_write_tokens: detail("input_tokens_details", "cache_write_tokens"),
        reasoning_tokens: detail("output_tokens_details", "reasoning_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn usage_keeps_the_totals_and_their_details() {
        let u = parse_responses_usage(Some(&json!({
            "input_tokens": 120,
            "input_tokens_details": {"cached_tokens": 100, "cache_write_tokens": 8},
            "output_tokens": 40,
            "output_tokens_details": {"reasoning_tokens": 30},
            "total_tokens": 160,
        })));
        assert_eq!(
            u,
            Usage {
                prompt_tokens: Some(120),
                completion_tokens: Some(40),
                cached_input_tokens: Some(100),
                cache_write_tokens: Some(8),
                reasoning_tokens: Some(30),
            }
        );
    }

    #[test]
    fn missing_or_null_usage_is_unknown_not_zero() {
        assert_eq!(parse_responses_usage(None), Usage::default());
        assert_eq!(parse_responses_usage(Some(&Value::Null)), Usage::default());
        let r = read_response(&json!({"status": "in_progress", "usage": null}));
        assert_eq!(r, ResponseReport::default());
    }

    #[test]
    fn a_failed_response_says_why() {
        let r = read_response(&json!({
            "status": "failed",
            "error": {"code": "server_error", "message": "the model crashed"},
            "usage": {"input_tokens": 5, "output_tokens": 0},
        }));
        assert_eq!(r.failed.as_deref(), Some("the model crashed"));
        assert_eq!(r.usage.prompt_tokens, Some(5));
        assert_eq!(read_response(&json!({"status": "incomplete"})).failed, None);
    }

    #[test]
    fn billable_hosted_calls_are_counted_and_the_rest_are_not() {
        let r = read_response(&json!({"status": "completed", "output": [
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "message", "id": "msg_1", "role": "assistant", "content": []},
            {"type": "web_search_call", "id": "ws_2", "status": "completed"},
            {"type": "code_interpreter_call", "id": "ci_1"},
            {"type": "mcp_call", "id": "mcp_1"},
            {"type": "computer_call", "id": "cu_1"},
            {"type": "function_call", "id": "fc_1"},
            {"type": "shell_call", "id": "sh_1", "environment": {"type": "local"}},
            {"type": "shell_call", "id": "sh_2",
             "environment": {"type": "container_reference", "container_id": "cntr_1"}},
        ]}));
        let calls: Vec<(&str, u64)> = r.hosted_calls.iter().collect();
        assert_eq!(
            calls,
            [
                ("web_search_call", 2),
                ("code_interpreter_call", 1),
                ("shell_call", 1)
            ]
        );
        assert!(read_response(&json!({"status": "completed"}))
            .hosted_calls
            .is_empty());
    }

    #[test]
    fn the_service_tier_is_read_verbatim_and_absent_stays_absent() {
        let r = read_response(&json!({"status": "completed", "service_tier": "priority"}));
        assert_eq!(r.service_tier.as_deref(), Some("priority"));
        assert_eq!(
            read_response(&json!({"status": "completed"})).service_tier,
            None
        );
    }
}
