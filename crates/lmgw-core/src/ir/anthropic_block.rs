//! An Anthropic content block the IR does not model, carried verbatim
//! (gateway design §6.1, changed 2026-11-05): `redacted_thinking`, the
//! provider's own tool blocks (`server_tool_use`, `web_search_tool_result`,
//! …) and any block type Anthropic adds next. The block is kept as
//! Anthropic sent it, so a Messages client gets it where it stood and an
//! Anthropic-protocol upstream gets it back as it came. Every other egress
//! leaves it out, and the response names its type in
//! `x-lmgw-params-ignored` ([`ignored_names`]).
//!
//! A block lmgw read from an answer has the [`Signer`] of the row that
//! answered, its scope: lmgw's own records (the agent loop's turns, the
//! Chat's tool turns) replay it to a row at that scope only, whatever its
//! key or model, because what it holds (`redacted_thinking`'s `data`) is
//! encrypted by the provider there, which drops what the model asked cannot
//! read ([`super::signer`]). A block a Messages client sent has none: it is
//! the client's own, and goes to every Anthropic-protocol upstream as the
//! client wrote it. Lmgw's pages, devices and exports never show one
//! ([`super::strip_seals`]).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{ContentPart, Message, Signer, ToolResultBlock};
use crate::config::{Protocol, Upstream};

mod sent;
mod tool_text;
pub use sent::AnthropicSent;
pub use tool_text::ToolText;
pub(crate) use tool_text::{has_unread, text_source, url_source};

/// One block (module doc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicBlock {
    /// The block as Anthropic or the client wrote it, its `type` included.
    pub block: Value,
    /// The scope whose answer carried it; `None` for a client's block
    /// (module doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Signer>,
    /// The `partial_json` an `input_json_delta` streamed so far, read into
    /// the block's `input` whenever it parses ([`Self::apply_delta`]).
    #[serde(skip)]
    partial_json: String,
}

/// Two blocks are equal when they hold the same block for the same scope;
/// what a stream had not folded in yet is no part of it.
impl PartialEq for AnthropicBlock {
    fn eq(&self, other: &Self) -> bool {
        self.block == other.block && self.scope == other.scope
    }
}

impl AnthropicBlock {
    /// A block a Messages client sent.
    pub fn from_client(block: Value) -> Self {
        Self {
            block,
            scope: None,
            partial_json: String::new(),
        }
    }

    /// A block the scope `scope` names answered with.
    pub fn answered(block: Value, scope: &Signer) -> Self {
        Self {
            block,
            scope: Some(scope.clone()),
            partial_json: String::new(),
        }
    }

    /// The block's `type`, as it came.
    pub fn kind(&self) -> &str {
        self.block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// What an `image` block is, as a placeholder names it
    /// ([`super::image_note`]'s wording, which names an image by URL
    /// without the URL; this names no file id): one whose source lmgw does
    /// not read (a Files API `file`), which vision checks count as an image
    /// where the row is sent it ([`Self::image_sent_to`]); `None` for every
    /// other block.
    pub fn image_note(&self) -> Option<String> {
        if self.kind() != "image" {
            return None;
        }
        let ty = self
            .block
            .get("source")
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        Some(match ty {
            "file" => "image by Files API reference".to_string(),
            other => format!("image with a {other:?} source"),
        })
    }

    /// Whether the row on `up` is sent the block as an image (gateway
    /// design §6.1, changed 2026-11-06): an `image` block
    /// ([`Self::image_note`]) to an Anthropic-protocol row it goes to
    /// ([`AnthropicSent`]). Every other row is sent none of it, and the
    /// request names it `content.image` ([`ignored_names`]): there it is no
    /// image, costs nothing, meets no per-image bound and calls for no
    /// vision (`gate::media_parts`, `gate::fallback_images`).
    pub fn image_sent_to(&self, up: &Upstream) -> bool {
        self.image_note().is_some()
            && up.protocol == Protocol::Anthropic
            && self.goes_to(&Signer::of(up))
    }

    /// Whether the block goes to the row whose scope `row` names: a
    /// client's block goes to every Anthropic-protocol row, an answer's only
    /// to a row at the scope that answered with it (module doc).
    pub fn goes_to(&self, row: &Signer) -> bool {
        super::signer::goes_to(self.scope.as_ref(), row)
    }

    /// One `content_block_delta` at the block folded in, so a streamed
    /// block is recorded whole. The delta types Anthropic documents fold as
    /// its own SDKs fold them: `input_json_delta`'s `partial_json` is the
    /// block's `input` once it parses, `citations_delta`'s `citation` joins
    /// its `citations`. Any other delta's fields (a `text_delta`'s `text`,
    /// a `thinking_delta`'s `thinking`, a `signature_delta`'s `signature`,
    /// and a type Anthropic adds next) extend the block's string of the same
    /// name, join its array of that name, or are set on it.
    pub fn apply_delta(&mut self, delta: &Value) {
        let Some(d) = delta.as_object() else {
            return;
        };
        let Some(block) = self.block.as_object_mut() else {
            return;
        };
        match d.get("type").and_then(Value::as_str) {
            Some("input_json_delta") => {
                if let Some(frag) = d.get("partial_json").and_then(Value::as_str) {
                    self.partial_json.push_str(frag);
                    if let Ok(input) = serde_json::from_str::<Value>(&self.partial_json) {
                        block.insert("input".into(), input);
                    }
                }
            }
            Some("citations_delta") => {
                if let Some(c) = d.get("citation") {
                    push(block, "citations", c.clone());
                }
            }
            _ => {
                for (k, v) in d {
                    if k == "type" {
                        continue;
                    }
                    match (block.get_mut(k), v) {
                        (Some(Value::String(s)), Value::String(more)) => s.push_str(more),
                        (Some(Value::Array(a)), v) => a.push(v.clone()),
                        _ => {
                            block.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
    }
}

fn push(block: &mut Map<String, Value>, key: &str, v: Value) {
    match block.get_mut(key) {
        Some(Value::Array(a)) => a.push(v),
        _ => {
            block.insert(key.into(), Value::Array(vec![v]));
        }
    }
}

/// A verbatim block's stream events, as an Anthropic stream sends them: its
/// `content_block_start`, every `content_block_delta` at it, and its
/// `content_block_stop` when the upstream's came (a `server_tool_use` block
/// stays open while the provider's tool runs, a web search for seconds, and
/// its stop says the call is whole). Only the Anthropic egress emits them;
/// the Messages wire relays them as they came, every other wire leaves them
/// out.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockEvent {
    /// The block as its `content_block_start` began it.
    Start(AnthropicBlock),
    /// One delta object at the block, verbatim.
    Delta(Value),
    /// Its `content_block_stop`: the block is whole.
    Stop,
}

/// The names `x-lmgw-params-ignored` gives the verbatim blocks among
/// `parts` that a wire left out, in order and each once: `<prefix>.<type>`,
/// `content.redacted_thinking` for a request's block an upstream was not
/// sent, `answer.redacted_thinking` for an answer's block a client was not
/// given. A type that is no plain token (not ASCII letters, digits, `_`, `-`
/// or `.`), which a header cannot carry as it is, is named `<prefix>.block`.
/// A text part's Anthropic citations, which only the Messages wire and an
/// Anthropic-protocol egress carry, are `<prefix>.citations`; a tool
/// result's verbatim block ([`ToolResultBlock::AnthropicBlock`]) is named
/// by its type as a message's is where some of it has no text form, which
/// is all a row off an Anthropic route is sent of it
/// ([`AnthropicBlock::tool_text`], changed 2026-11-06).
pub fn ignored_names<'a>(
    prefix: &str,
    parts: impl IntoIterator<Item = &'a ContentPart>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let b = match p {
            ContentPart::Text { citations, .. } if !citations.is_empty() => {
                let name = format!("{prefix}.citations");
                if !out.contains(&name) {
                    out.push(name);
                }
                continue;
            }
            ContentPart::AnthropicBlock(b) => b,
            // A tool result's verbatim block, where it loses something as
            // text (a PDF, a Files API image).
            ContentPart::ToolResult { content, .. } => {
                for t in content {
                    if let ToolResultBlock::AnthropicBlock(b) = t {
                        if b.lost() {
                            push_name(&mut out, prefix, b);
                        }
                        if b.wants_citations() {
                            let name = format!("{prefix}.citations");
                            if !out.contains(&name) {
                                out.push(name);
                            }
                        }
                    }
                }
                continue;
            }
            _ => continue,
        };
        push_name(&mut out, prefix, b);
    }
    out
}

/// `<prefix>.<type>` of `b` onto `out`, once ([`ignored_names`]).
fn push_name(out: &mut Vec<String>, prefix: &str, b: &AnthropicBlock) {
    let name = b.header_name(prefix);
    if !out.contains(&name) {
        out.push(name);
    }
}

impl AnthropicBlock {
    /// `<prefix>.<type>`, as [`ignored_names`] names the block: a type that
    /// is no plain token is `<prefix>.block`.
    pub fn header_name(&self, prefix: &str) -> String {
        let kind = self.kind();
        let token = !kind.is_empty()
            && kind
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'));
        format!("{prefix}.{}", if token { kind } else { "block" })
    }
}

/// [`ignored_names`] of every message's parts.
pub fn ignored_in_messages(prefix: &str, messages: &[Message]) -> Vec<String> {
    ignored_names(prefix, messages.iter().flat_map(|m| &m.content))
}

/// The names `x-lmgw-params-ignored` gives what an Anthropic-protocol row
/// at `row`'s scope is not sent of `messages` ([`AnthropicSent`]: what
/// another scope signed, a citation without text; the egress logs the
/// same, `egress/anthropic/left_out.rs`), in order and each once:
/// `content.thinking` for a thinking block, `content.citations` for a
/// text's citations, `content.<type>` for a verbatim block
/// ([`ignored_names`]' spelling).
pub fn left_out_for(messages: &[Message], row: &Signer) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in messages.iter().flat_map(|m| &m.content) {
        if let Some(name) = AnthropicSent::left_out_name(p, row) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_streamed_block_folds_into_the_block_it_streamed() {
        let mut b = AnthropicBlock::answered(
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                   "input": {}}),
            &Signer::new("https://api.anthropic.com"),
        );
        b.apply_delta(&json!({"type": "input_json_delta", "partial_json": "{\"query\": "}));
        assert_eq!(b.block["input"], json!({}), "not yet whole");
        b.apply_delta(&json!({"type": "input_json_delta", "partial_json": "\"rust\"}"}));
        assert_eq!(b.block["input"], json!({"query": "rust"}));

        let mut f = AnthropicBlock::from_client(json!({"type": "future_block", "note": "a"}));
        f.apply_delta(&json!({"type": "future_delta", "note": "b", "extra": 1}));
        f.apply_delta(&json!({"type": "citations_delta", "citation": {"n": 1}}));
        assert_eq!(
            f.block,
            json!({"type": "future_block", "note": "ab", "extra": 1, "citations": [{"n": 1}]})
        );
    }

    #[test]
    fn an_answered_block_goes_to_its_scope_and_a_client_block_everywhere() {
        let row = Signer::new("https://api.anthropic.com");
        let answered = AnthropicBlock::answered(json!({"type": "redacted_thinking"}), &row);
        assert!(answered.goes_to(&row));
        assert!(!answered.goes_to(&Signer::new("https://api.example.com")));
        let client = AnthropicBlock::from_client(json!({"type": "redacted_thinking"}));
        assert!(client.goes_to(&Signer::new("https://api.example.com")));
    }

    #[test]
    fn what_another_scope_signed_is_named_once() {
        let here = Signer::new("https://api.anthropic.com");
        let there = Signer::new("https://api.example.com");
        let messages = [Message {
            role: super::super::Role::Assistant,
            content: vec![
                ContentPart::signed_reasoning("", "sig", Some(there.clone())),
                ContentPart::signed_reasoning("", "sig", Some(here.clone())),
                ContentPart::signed_reasoning("", "client", None),
                ContentPart::AnthropicBlock(AnthropicBlock::answered(
                    json!({"type": "redacted_thinking", "data": "x"}),
                    &there,
                )),
                ContentPart::AnthropicBlock(AnthropicBlock::from_client(
                    json!({"type": "server_tool_use"}),
                )),
            ],
        }];
        assert_eq!(
            left_out_for(&messages, &here),
            ["content.thinking", "content.redacted_thinking"]
        );
        assert!(
            left_out_for(&messages, &there).len() == 1,
            "only the one signed here"
        );
    }

    #[test]
    fn ignored_blocks_are_named_by_type_once() {
        let parts = [
            ContentPart::AnthropicBlock(AnthropicBlock::from_client(
                json!({"type": "redacted_thinking", "data": "x"}),
            )),
            ContentPart::text("hi"),
            ContentPart::AnthropicBlock(AnthropicBlock::from_client(
                json!({"type": "redacted_thinking", "data": "y"}),
            )),
            ContentPart::AnthropicBlock(AnthropicBlock::from_client(
                json!({"type": "füture block"}),
            )),
        ];
        assert_eq!(
            ignored_names("content", &parts),
            ["content.redacted_thinking", "content.block"]
        );
    }

    /// A record keeps the block and its scope; the scope of a client's
    /// block is left out.
    #[test]
    fn a_block_round_trips_through_a_record() {
        let b = ContentPart::AnthropicBlock(AnthropicBlock::answered(
            json!({"type": "redacted_thinking", "data": "enc"}),
            &Signer::new("https://api.anthropic.com"),
        ));
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(
            v,
            json!({"type": "anthropic_block", "block": {"type": "redacted_thinking", "data": "enc"},
                   "scope": "https://api.anthropic.com"})
        );
        assert_eq!(serde_json::from_value::<ContentPart>(v).unwrap(), b);
    }
}
