//! What lmgw's own prompt estimates count of the parts a send carries but
//! no local counter reads whole (gateway design §6.1, "Counting", changed
//! 2026-11-06): a verbatim Anthropic block's text, a text block's
//! citations (without what Anthropic does not count, [`citation_text`]),
//! and the names of what is left out.
//!
//! A stopped or unanswered call's prompt estimate (`proxy::stop`) and the
//! budgets' floor count what the request sent. On an Anthropic-protocol
//! route that is what the row at its scope is sent ([`AnthropicSent`], the
//! egress's own predicate): its verbatim blocks (a `document`'s plain-text
//! source, a `search_result`'s content, a provider tool's input and
//! result), and nothing another scope signed.
//! Their strings are counted; what no local counter can read (a base64 PDF
//! or image, a Files API reference, encrypted content, a thinking block's
//! signature, which holds the whole thinking Anthropic bills as input) is
//! left out, and the estimate's note names it ([`uncounted`]): no
//! undercount goes unsaid.

use serde_json::Value;

use crate::config::{Protocol, Upstream};
use crate::ir::{has_unread, text_source, url_source};
use crate::ir::{AnthropicBlock, AnthropicSent, ChatRequest, ContentPart, Signer, ToolResultBlock};

/// The keys whose strings no local counter reads as text: the block's
/// scaffolding (`type`, `media_type`, `cache_control`) and what is opaque
/// (base64 `data`, a Files API `file_id`, encrypted content, signatures).
const OPAQUE: &[&str] = &[
    "type",
    "media_type",
    "cache_control",
    "data",
    "file_id",
    "encrypted_content",
    "encrypted_index",
    "signature",
];

/// Every string of `v` an estimate counts (in the map's key order): each
/// string value but those under [`OPAQUE`] keys, and a plain-text source's
/// `data`.
pub(super) fn countable(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| countable(x, out)),
        Value::Object(o) => {
            let text = text_source(o);
            let url = url_source(o);
            for (k, x) in o {
                if (!OPAQUE.contains(&k.as_str()) || (text && k == "data")) && !(url && k == "url")
                {
                    countable(x, out);
                }
            }
        }
        _ => {}
    }
}

/// A verbatim block's countable text, joined.
pub(super) fn block_text(b: &AnthropicBlock) -> String {
    let mut out = Vec::new();
    countable(&b.block, &mut out);
    out.join("\n")
}

/// The strings of one citation an estimate counts: [`countable`]'s,
/// without the `cited_text`, which Anthropic does not count ("When passed
/// back in subsequent conversation turns, `cited_text` is also not counted
/// toward input tokens",
/// <https://platform.claude.com/docs/en/build-with-claude/citations>, "Token
/// costs"), and for a web search's citation without its `title` and `url`
/// too ("The web search citation fields `cited_text`, `title`, and `url` do
/// not count toward input or output token usage",
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>,
/// "Citations").
pub(super) fn citation_text(c: &Value, out: &mut Vec<String>) {
    let Some(o) = c.as_object() else {
        return countable(c, out);
    };
    let web = o.get("type").and_then(Value::as_str) == Some("web_search_result_location");
    for (k, x) in o {
        let free = k == "cited_text" || (web && matches!(k.as_str(), "title" | "url"));
        if !free && !OPAQUE.contains(&k.as_str()) {
            countable(x, out);
        }
    }
}

/// The scope an estimate counts for on `up`: the row's own on an
/// Anthropic-protocol route, where what the row is sent depends on it
/// ([`AnthropicSent`]); `None` on every other route.
pub(crate) fn anthropic_row(up: &Upstream) -> Option<Signer> {
    (up.protocol == Protocol::Anthropic).then(|| Signer::of(up))
}

/// The names of the parts `ir` sends that the estimate leaves out, in order
/// and each once, as `x-lmgw-params-ignored` spells a part
/// (`content.image`, `content.audio`, `content.document`,
/// `content.thinking` for a signed thinking block's encrypted thinking):
/// images and audio everywhere, and on an Anthropic route (`anthropic`, the
/// row's scope, [`anthropic_row`]) what a verbatim block or a signature it
/// is sent holds that no local counter reads. What the row is not sent (a
/// verbatim block off an Anthropic route, a block another scope signed,
/// [`AnthropicSent`]) is neither counted nor named.
pub(crate) fn uncounted(ir: &ChatRequest, anthropic: Option<&Signer>) -> Vec<String> {
    let ir = &*crate::ir::compaction::folded(ir);
    let mut out: Vec<String> = Vec::new();
    let mut add = |name: String| {
        if !out.contains(&name) {
            out.push(name);
        }
    };
    for p in ir.non_system_messages().flat_map(|m| &m.content) {
        let sent = anthropic.map(|row| AnthropicSent::of(p, row)) == Some(AnthropicSent::Whole);
        match p {
            ContentPart::Image { .. } => add("content.image".into()),
            ContentPart::Audio { .. } => add("content.audio".into()),
            ContentPart::Reasoning {
                signature: Some(_), ..
            } if sent => add("content.thinking".into()),
            ContentPart::AnthropicBlock(b) if sent && has_unread(&b.block) => {
                for name in crate::ir::ignored_names("content", [p]) {
                    add(name);
                }
            }
            // A tool result's verbatim block, which an Anthropic row is
            // sent as written: what of it no local counter reads (a PDF,
            // a Files API reference). Every other row is sent its text
            // form, which is counted ([`crate::ir::AnthropicBlock::tool_text`]).
            ContentPart::ToolResult { content, .. } if anthropic.is_some() => {
                for t in content {
                    if let ToolResultBlock::AnthropicBlock(b) = t {
                        if has_unread(&b.block) {
                            add(b.header_name("content"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The clause an estimate's note ends with when [`uncounted`] named parts:
/// empty when it named none.
pub(crate) fn left_out_clause(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!(
        "; the estimate leaves out what lmgw cannot count locally ({})",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_block_counts_its_strings_and_leaves_the_opaque_ones_out() {
        // A plain-text source's `data` is the document's text.
        let doc = AnthropicBlock::from_client(json!({
            "type": "document", "title": "Notes",
            "source": {"type": "text", "media_type": "text/plain", "data": "The word is KIWI."},
            "context": "the team's notes",
        }));
        assert_eq!(
            block_text(&doc),
            "the team's notes\nThe word is KIWI.\nNotes",
            "the map's key order"
        );
        assert!(!has_unread(&doc.block));
        let pdf = AnthropicBlock::from_client(json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"},
        }));
        assert_eq!(block_text(&pdf), "");
        assert!(has_unread(&pdf.block));
        let file = AnthropicBlock::from_client(json!({
            "type": "image", "source": {"type": "file", "file_id": "file_1"},
        }));
        assert!(has_unread(&file.block));
        let search = AnthropicBlock::from_client(json!({
            "type": "search_result", "source": "https://example.com", "title": "T",
            "content": [{"type": "text", "text": "The word is KIWI."}],
        }));
        assert_eq!(
            block_text(&search),
            "The word is KIWI.\nhttps://example.com\nT"
        );
        assert!(!has_unread(&search.block));
    }

    #[test]
    fn what_is_left_out_is_named_where_it_is_sent() {
        let ir = crate::ingress::anthropic::parse_messages_request(&json!({
            "model": "m", "max_tokens": 9,
            "messages": [
                {"role": "user", "content": [
                    {"type": "document", "source": {"type": "file", "file_id": "file_1"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                 "data": "iVBORw0KGgo="}},
                    {"type": "search_result", "source": "s", "title": "t",
                     "content": [{"type": "text", "text": "x"}]},
                    {"type": "text", "text": "Read them."},
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig"},
                    {"type": "text", "text": "Done."},
                ]},
                {"role": "user", "content": "Thanks."},
            ],
        }))
        .unwrap();
        let row = Signer::new("https://api.anthropic.com");
        assert_eq!(
            uncounted(&ir, Some(&row)),
            ["content.document", "content.image", "content.thinking"]
        );
        // Not sent off an Anthropic route: only the image is.
        assert_eq!(uncounted(&ir, None), ["content.image"]);
        assert_eq!(left_out_clause(&[]), "");
    }

    /// A by-URL document or image (also nested in a custom-content
    /// document) is a reference: the URL is not counted and the block is
    /// named (changed 2026-11-06).
    #[test]
    fn a_url_source_is_named_and_its_url_not_counted() {
        let ir = crate::ingress::anthropic::parse_messages_request(&json!({
            "model": "m", "max_tokens": 9,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                    "content": [
                        {"type": "document", "title": "Paper",
                         "source": {"type": "url", "url": "https://e.example/PAPER.pdf"}},
                        {"type": "document", "source": {"type": "content", "content": [
                            {"type": "image", "source": {"type": "url",
                                                         "url": "https://e.example/IMG.png"}}]}},
                    ]}]},
            ],
        }))
        .unwrap();
        let row = Signer::new("https://api.anthropic.com");
        assert_eq!(uncounted(&ir, Some(&row)), ["content.document"]);
        let text = super::super::estimate_text(&ir, Some(&row));
        assert!(!text.contains("e.example"), "{text}");
        let img = AnthropicBlock::from_client(json!({
            "type": "image", "source": {"type": "url", "url": "https://e.example/a.png"},
        }));
        assert!(has_unread(&img.block));
    }

    /// A tool result's verbatim blocks: an Anthropic row is sent them as
    /// written, so their strings count and a PDF's or a Files API
    /// reference's content is named; every other row is sent their text
    /// form, which counts whole and leaves nothing out (changed
    /// 2026-11-06).
    #[test]
    fn a_tool_result_s_blocks_count_as_each_row_is_sent_them() {
        let ir = crate::ingress::anthropic::parse_messages_request(&json!({
            "model": "m", "max_tokens": 9,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                    "content": [
                        {"type": "document", "title": "Notes",
                         "source": {"type": "text", "media_type": "text/plain",
                                    "data": "The word is KIWI."}},
                        {"type": "search_result", "source": "kb://1", "title": "T",
                         "content": [{"type": "text", "text": "x"}]},
                        {"type": "document", "source": {"type": "file", "file_id": "file_1"}},
                    ]}]},
            ],
        }))
        .unwrap();
        let row = Signer::new("https://api.anthropic.com");
        assert_eq!(uncounted(&ir, Some(&row)), ["content.document"]);
        assert_eq!(
            super::super::estimate_text(&ir, Some(&row)),
            "assistant: lookup({})\n\ntool: The word is KIWI.\nNotes\nx\nkb://1\nT"
        );
        assert!(uncounted(&ir, None).is_empty());
        assert_eq!(
            super::super::estimate_text(&ir, None),
            "assistant: lookup({})\n\ntool: Notes\n\nThe word is KIWI.\n\nT\nkb://1\n\nx"
        );
    }

    /// A citation's `cited_text` (and a web search citation's `title` and
    /// `url`) is free on Anthropic, so the estimate leaves it out.
    #[test]
    fn a_citation_counts_without_what_anthropic_does_not_bill() {
        let mut out = Vec::new();
        citation_text(
            &json!({"type": "char_location", "cited_text": "The word is KIWI.",
                    "document_index": 0, "document_title": "Notes",
                    "start_char_index": 0, "end_char_index": 17}),
            &mut out,
        );
        assert_eq!(out, ["Notes"]);
        out.clear();
        citation_text(
            &json!({"type": "web_search_result_location", "url": "https://example.com",
                    "title": "Kiwi", "encrypted_index": "Eo8B", "cited_text": "KIWI."}),
            &mut out,
        );
        assert!(out.is_empty(), "{out:?}");
    }

    /// What another scope signed is not sent to this row, so neither
    /// counted nor named ([`AnthropicSent`]).
    #[test]
    fn what_another_scope_signed_is_not_named() {
        let there = Signer::new("https://gateway.ai.cloudflare.com/v1/a/g/anthropic");
        let here = Signer::new("https://api.anthropic.com");
        let ir = ChatRequest {
            messages: vec![crate::ir::Message {
                role: crate::ir::Role::Assistant,
                content: vec![
                    ContentPart::signed_reasoning("hmm", "sig", Some(there.clone())),
                    ContentPart::AnthropicBlock(AnthropicBlock::answered(
                        json!({"type": "server_tool_use", "name": "web_search",
                               "input": {"query": "kiwi facts"}}),
                        &there,
                    )),
                    ContentPart::AnthropicBlock(AnthropicBlock::answered(
                        json!({"type": "redacted_thinking", "data": "enc"}),
                        &there,
                    )),
                    ContentPart::cited(
                        "The word is KIWI.",
                        vec![json!({"type": "search_result_location", "source": "notes",
                                    "cited_text": "KIWI quoted"})],
                        Some(there.clone()),
                    ),
                ],
            }],
            ..crate::ingress::anthropic::parse_messages_request(&json!({
                "model": "m", "max_tokens": 9, "messages": [],
            }))
            .unwrap()
        };
        assert_eq!(
            uncounted(&ir, Some(&there)),
            ["content.thinking", "content.redacted_thinking"]
        );
        assert!(uncounted(&ir, Some(&here)).is_empty());
        let at_there = super::super::estimate_text(&ir, Some(&there));
        assert_eq!(
            at_there,
            "assistant: hmm\nkiwi facts\nweb_search\nThe word is KIWI.\nnotes"
        );
        assert_eq!(
            super::super::estimate_text(&ir, Some(&here)),
            "assistant: The word is KIWI.",
            "only the text, as the egress sends it"
        );
    }
}
