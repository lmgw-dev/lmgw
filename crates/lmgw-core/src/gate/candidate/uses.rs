//! Which capability facets a request **uses** (candidate-aliases design
//! §4.6 "Requests", §9 "a request that uses a facet the alias does not
//! enable is refused (400), not passed through").
//!
//! A candidate alias is a contract: `/v1/models` publishes exactly its
//! enabled facets, and every candidate — and its fallback — supports each of
//! them. A request that needs one the alias does not enable is refused by
//! name before anything starts ([`crate::gate::Routed::using`]), rather than
//! handed to a model the alias never promised it to.
//!
//! **What counts, per facet.** Only what the request itself asks for — a
//! facet a client could not have meant is never read into it:
//! - **vision:** an image part anywhere in the messages
//!   ([`crate::gate::media_parts`]). Not a Files API image (changed
//!   2026-11-06): only an Anthropic-protocol row is sent one, and a
//!   candidate is a local row;
//! - **audio:** an audio input part;
//! - **tool_calls:** tool definitions the model is given. The IR's own
//!   `tools` here; a site whose loop adds tools of its own (the
//!   `/v1/responses` MCP blocks, Admin/agent chat, agent runs) adds the facet
//!   itself. Tool *history* without definitions is not counted: a model
//!   given no tools cannot call one;
//! - **structured_output:** a JSON format in the IR's field
//!   (structured-output design §4) — `response_format` of type `json_schema`
//!   or `json_object`, llama.cpp's native top-level `json_schema` (beside a
//!   `text` format too), the `/v1/responses` `text.format`, Anthropic's
//!   `output_config.format`, all read into it by their ingress. `grammar` is
//!   a llama.cpp-only sampler control, not this facet;
//! - **reasoning:** an explicit ask to **think** (§12, "narrow"): `enabled:
//!   true`, an effort level other than `none`, a budget above zero, or
//!   `chat_template_kwargs.enable_thinking: true` — from the body or from
//!   the `x-lmgw-reasoning*` headers. Asking for thinking **off**, or saying
//!   nothing, is not a use: every model can not-think, and clients that
//!   always send an effort of `none` must not be refused for it.

use serde_json::Value;

use crate::candidates::{Facet, FacetSet};
use crate::ir::{ChatRequest, ReasoningControl};

/// The facets a chat-shaped request uses (module doc): every dialect that
/// parses to the IR — `/v1/chat/completions`, `/v1/messages`,
/// `/v1/responses`, the dashboard and agent chats, MCP sampling.
///
/// `header` is the request's `x-lmgw-reasoning*` control
/// ([`crate::proxy::RequestCtx::reasoning_control`]), `None` for in-process
/// callers, which have no headers.
pub fn request_facets(ir: &ChatRequest, header: Option<&ReasoningControl>) -> FacetSet {
    // A candidate is a local row, which is sent no Files API image.
    let media = crate::gate::media_parts(ir, None);
    let mut uses = FacetSet::EMPTY;
    if media.images > 0 {
        uses = uses.insert(Facet::Vision);
    }
    if media.audio > 0 {
        uses = uses.insert(Facet::Audio);
    }
    if !ir.tools.is_empty() {
        uses = uses.insert(Facet::ToolCalls);
    }
    if ir.asks_json() {
        uses = uses.insert(Facet::StructuredOutput);
    }
    let thinks = ir.params.reasoning.as_ref().is_some_and(asks_to_think)
        || header.is_some_and(asks_to_think)
        || ir.llama_kwargs_enabled == Some(true);
    if thinks {
        uses = uses.insert(Facet::Reasoning);
    }
    uses
}

/// The facets a legacy `/v1/completions` body uses: structured output only
/// (the same two fields as [`request_facets`]). The body is forwarded as it
/// came — no chat template, so no tools and no thinking switch — and a
/// prompt that carries media is refused by the fit on a guarded row and
/// otherwise llama-server's own business.
pub fn legacy_facets(body: &Value) -> FacetSet {
    if crate::ir::structured::body_kind(body).is_some() {
        FacetSet::EMPTY.insert(Facet::StructuredOutput)
    } else {
        FacetSet::EMPTY
    }
}

/// An explicit ask to think (module doc): on, an effort other than `none`,
/// or a positive budget.
fn asks_to_think(c: &ReasoningControl) -> bool {
    c.enabled == Some(true)
        || c.effort
            .as_deref()
            .is_some_and(|e| !e.trim().eq_ignore_ascii_case("none"))
        || c.budget_tokens.is_some_and(|b| b > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ContentPart, ImageSource, Message, Params, Role, ToolDef};
    use serde_json::json;

    fn ir() -> ChatRequest {
        ChatRequest {
            model_alias: "jobs".into(),
            messages: vec![Message::text(Role::User, "hi")],
            params: Params::default(),
            tools: vec![],
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        }
    }

    fn names(s: FacetSet) -> Vec<String> {
        s.names()
    }

    #[test]
    fn a_plain_text_request_uses_nothing() {
        assert!(request_facets(&ir(), None).is_empty());
    }

    #[test]
    fn images_audio_and_tools_each_count() {
        let mut r = ir();
        r.messages[0].content.push(ContentPart::Image {
            mime: "image/png".into(),
            source: ImageSource::Base64 {
                data: "AAAA".into(),
            },
        });
        r.messages[0].content.push(ContentPart::Audio {
            mime: "audio/wav".into(),
            data: "AAAA".into(),
        });
        r.tools.push(ToolDef {
            name: "f".into(),
            description: None,
            parameters: json!({"type": "object"}),
            strict: None,
        });
        assert_eq!(
            names(request_facets(&r, None)),
            ["vision", "audio", "tool_calls"]
        );
    }

    #[test]
    fn structured_output_is_a_json_response_format_or_a_native_schema() {
        for (key, v, used) in [
            (
                "response_format",
                json!({"type": "json_schema", "json_schema": {}}),
                true,
            ),
            ("response_format", json!({"type": "json_object"}), true),
            ("response_format", json!({"type": "text"}), false),
            ("response_format", json!({"type": "regex"}), false),
            ("json_schema", json!({"type": "object"}), true),
            ("grammar", json!("root ::= \"a\""), false),
        ] {
            let body = json!({"model": "m", "messages": [], key: v.clone()});
            let r = crate::ingress::openai::parse_chat_request(&body).unwrap();
            assert_eq!(
                request_facets(&r, None).contains(Facet::StructuredOutput),
                used,
                "{key}: {v}"
            );
            assert_eq!(legacy_facets(&body).contains(Facet::StructuredOutput), used);
        }
        // A `text` format beside the native schema asks for the schema
        // (structured-output design §11.18), in both dialects' readers.
        let body = json!({"model": "m", "messages": [],
                          "response_format": {"type": "text"}, "json_schema": {"type": "object"}});
        let r = crate::ingress::openai::parse_chat_request(&body).unwrap();
        assert!(request_facets(&r, None).contains(Facet::StructuredOutput));
        assert!(legacy_facets(&body).contains(Facet::StructuredOutput));
    }

    /// Only an ask to think counts; switching it off, or an effort of
    /// `none`, is not a use (§12, "narrow").
    #[test]
    fn reasoning_counts_only_when_the_request_asks_to_think() {
        let cases = [
            (Some(true), None, None, true),
            (Some(false), None, None, false),
            (None, Some("high"), None, true),
            (None, Some("none"), None, false),
            (None, Some("None"), None, false),
            (None, None, Some(1024), true),
            (None, None, Some(0), false),
        ];
        for (enabled, effort, budget, used) in cases {
            let c = ReasoningControl {
                enabled,
                effort: effort.map(str::to_string),
                budget_tokens: budget,
                ..Default::default()
            };
            let mut r = ir();
            r.params.reasoning = Some(c.clone());
            assert_eq!(
                request_facets(&r, None).contains(Facet::Reasoning),
                used,
                "body {c:?}"
            );
            assert_eq!(
                request_facets(&ir(), Some(&c)).contains(Facet::Reasoning),
                used,
                "header {c:?}"
            );
        }
        let mut r = ir();
        r.llama_kwargs_enabled = Some(true);
        assert!(request_facets(&r, None).contains(Facet::Reasoning));
        r.llama_kwargs_enabled = Some(false);
        assert!(!request_facets(&r, None).contains(Facet::Reasoning));
    }
}
