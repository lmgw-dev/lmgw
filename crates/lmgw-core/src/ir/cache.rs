//! Anthropic's prompt-caching marks (gateway design §6.1, "Prompt
//! caching", 2026-11-12): a `cache_control` object on a tool, a `system`
//! text block or a message content block, and one at the top of a
//! Messages body, which asks Anthropic to place the breakpoint itself.
//!
//! The IR keeps each mark where the client wrote it, verbatim
//! ([`CacheControl`]): on the part that carried it (`ContentPart::Text`,
//! `::Image`, `::ToolUse`, `::ToolResult`, `ToolResultBlock::Text`,
//! `::Image`), in a tool's Anthropic fields ([`super::ToolDef`]) and in
//! [`super::ChatRequest::cache_control`]. Only an Anthropic-protocol
//! egress writes them back; every other egress never reads them, so the
//! body it builds is the one it built before marks existed, and the
//! response names `cache_control` in `x-lmgw-params-ignored`
//! ([`ignored_names`]). Marks are not translated yet for an
//! OpenAI-compatible upstream that takes `cache_control` on content parts
//! (gateways in front of Claude models), and a request from an
//! OpenAI-shaped route carries none, so nothing is cached for it on an
//! Anthropic row yet.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ChatRequest, ContentPart, Message, Role, ToolResultBlock};

/// The key every placement spells its mark with.
pub const KEY: &str = "cache_control";

/// One `cache_control` object as the client wrote it (`{"type":
/// "ephemeral"}`, a `ttl`, a field Anthropic adds next).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CacheControl(pub Value);

impl CacheControl {
    /// A block's non-null `cache_control`, verbatim; `null` is no mark.
    pub fn of(block: &Value) -> Option<Self> {
        block.get(KEY).filter(|v| !v.is_null()).cloned().map(Self)
    }

    /// `block` with this mark set on it, when there is one.
    pub fn mark(block: &mut Value, mark: Option<&Self>) {
        if let (Some(m), Some(o)) = (mark, block.as_object_mut()) {
            o.insert(KEY.into(), m.0.clone());
        }
    }
}

/// Whether a verbatim block holds a mark of its own.
fn verbatim_marked(block: &Value) -> bool {
    block.get(KEY).is_some_and(|v| !v.is_null())
}

/// Whether one content part carries a mark: its own, a tool result's
/// nested block's, a verbatim block's or a compaction block's.
fn part_marked(p: &ContentPart) -> bool {
    match p {
        ContentPart::Text { cache_control, .. }
        | ContentPart::Image { cache_control, .. }
        | ContentPart::ToolUse { cache_control, .. } => cache_control.is_some(),
        ContentPart::ToolResult {
            cache_control,
            content,
            ..
        } => cache_control.is_some() || content.iter().any(block_marked),
        ContentPart::AnthropicBlock(b) => verbatim_marked(&b.block),
        ContentPart::Compaction(c) => c.fields.get(KEY).is_some_and(|v| !v.is_null()),
        ContentPart::Audio { .. } | ContentPart::Reasoning { .. } | ContentPart::Refusal { .. } => {
            false
        }
    }
}

/// Whether one tool-result block carries a mark.
fn block_marked(b: &ToolResultBlock) -> bool {
    match b {
        ToolResultBlock::Text { cache_control, .. }
        | ToolResultBlock::Image { cache_control, .. } => cache_control.is_some(),
        ToolResultBlock::AnthropicBlock(b) => verbatim_marked(&b.block),
        ToolResultBlock::Json { .. }
        | ToolResultBlock::Audio { .. }
        | ToolResultBlock::Resource { .. } => false,
    }
}

/// Whether the request carries any mark: the top-level one, a tool's, a
/// `system` block's, a message block's or a block's nested in a tool
/// result. The one predicate [`ignored_names`] reads.
pub fn has_marks(ir: &ChatRequest) -> bool {
    ir.cache_control.is_some()
        || ir
            .tools
            .iter()
            .any(|t| t.anthropic_fields.contains_key(KEY))
        || ir.messages.iter().flat_map(|m| &m.content).any(part_marked)
}

/// What `x-lmgw-params-ignored` names of the request's Anthropic-only
/// caching and tool fields on a route that is not Anthropic's, in order
/// and each once: `cache_control` for any mark ([`has_marks`]), then
/// `tools.<key>` for every other Anthropic tool field a tool carries
/// ([`super::ToolDef::anthropic_fields`]). A key that is no plain token
/// (ASCII letters, digits, `_`, `-`, `.`), which a header cannot carry as
/// it is, is named `tools.field`.
pub fn ignored_names(ir: &ChatRequest) -> Vec<String> {
    let mut out = Vec::new();
    if has_marks(ir) {
        out.push(KEY.to_string());
    }
    for k in ir.tools.iter().flat_map(|t| t.anthropic_fields.keys()) {
        if k == KEY {
            continue;
        }
        let token = !k.is_empty()
            && k.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'));
        let name = format!("tools.{}", if token { k.as_str() } else { "field" });
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Since a Messages client's `system` blocks are a part each, a system
/// message's text parts join by a blank line ([`Message::joined_text`]);
/// the OpenAI-shaped ingresses keep their own join with this.
impl Message {
    /// A system message with its text parts joined by a line break into
    /// one, where the first stood; every other message as it is. What an
    /// OpenAI-shaped client's system parts were joined by before the
    /// system role's own rule ([`Self::joined_text`]), so its system text
    /// reaches every upstream byte for byte as before.
    pub fn with_system_text_joined(mut self) -> Self {
        let texts = self
            .content
            .iter()
            .filter(|p| matches!(p, ContentPart::Text { .. }))
            .count();
        if self.role != Role::System || texts < 2 {
            return self;
        }
        let joined = self
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut joined = Some(joined);
        self.content.retain_mut(|p| match p {
            ContentPart::Text { .. } => match joined.take() {
                Some(text) => {
                    *p = ContentPart::text(text);
                    true
                }
                None => false,
            },
            _ => true,
        });
        self
    }
}

#[cfg(test)]
mod tests;
