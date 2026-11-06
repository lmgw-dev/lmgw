//! Anthropic's `compaction` content block in the IR (gateway design §6.1).
//!
//! Anthropic answers a compaction request (the top-level `compaction`, beta
//! `compact-2026-09-04`, or a `compact_*` edit in `context_management`, beta
//! `compact-2026-01-12`) with a `compaction` block: the summary as
//! `content`, plus fields the client must send back exactly as they came
//! (`signature`, `encrypted_content`, `tool_changes`, a `cache_control` the
//! client added, and whatever Anthropic adds next). A [`Compaction`] is the
//! summary as text and every other field verbatim, so the block goes back to
//! Anthropic as it came and a route without the block reads its summary.
//!
//! On a route without compaction blocks (every egress but Anthropic's) the
//! conversation is what Anthropic itself makes of it ([`folded`]): every
//! content block before the block is dropped, and the summary opens the kept
//! conversation as a user's text.
//!
//! A block lmgw read from an answer has the [`Signer`] of the row that
//! answered, its scope (gateway design §6.1, changed 2026-11-06), as a
//! signed thinking block has: its `signature` and `encrypted_content` are
//! that provider's to read. lmgw's own records send the block to an
//! Anthropic-protocol row at that scope only; a row at another scope gets
//! the conversation folded as every other egress gets it ([`folded_for`]),
//! and the request names `content.compaction`
//! ([`super::AnthropicSent::left_out_name`]). A block a Messages client sent
//! has no scope: it is the client's own, and goes to every Anthropic row as
//! the client wrote it. Lmgw's pages, devices and exports show the summary
//! without the signature, the encrypted content or the scope
//! ([`super::strip_seals`]).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{ChatRequest, ContentPart, Message, Role, Signer};

/// The block's `type`.
pub const BLOCK_TYPE: &str = "compaction";

/// A `compaction` block (module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// The summary, the block's `content`. `None` where Anthropic sent
    /// `null`: no summary was produced, and Anthropic treats the block as a
    /// no-op.
    #[serde(default)]
    pub summary: Option<String>,
    /// Every other field of the block, verbatim (`signature`,
    /// `encrypted_content`, `tool_changes`, `cache_control`, …); a `content`
    /// that is neither a string nor `null` stays here too.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
    /// The scope whose answer carried it; `None` for a client's block
    /// (module doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Signer>,
}

impl Compaction {
    /// A block as a Messages body or answer carries it.
    pub fn from_block(block: &Value) -> Self {
        let mut fields = block.as_object().cloned().unwrap_or_default();
        fields.remove("type");
        let summary = match fields.get("content") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(_) => {
                return Self {
                    summary: None,
                    fields,
                    scope: None,
                }
            }
        };
        fields.remove("content");
        Self {
            summary,
            fields,
            scope: None,
        }
    }

    /// A block the row whose scope `scope` names answered with.
    pub fn answered(block: &Value, scope: &Signer) -> Self {
        Self {
            scope: Some(scope.clone()),
            ..Self::from_block(block)
        }
    }

    /// Whether the block goes to the row whose scope `row` names: a
    /// client's block to every Anthropic-protocol row, an answer's only to a
    /// row at the scope that answered with it (module doc).
    pub fn goes_to(&self, row: &Signer) -> bool {
        super::signer::goes_to(self.scope.as_ref(), row)
    }

    /// The block in Anthropic's shape, as it came: `type`, `content` (the
    /// summary, `null` without one) and every other field.
    pub fn to_block(&self) -> Value {
        let mut block = Map::new();
        block.insert("type".into(), Value::from(BLOCK_TYPE));
        block.insert(
            "content".into(),
            self.summary.clone().map_or(Value::Null, Value::String),
        );
        block.extend(self.fields.clone());
        Value::Object(block)
    }

    /// One `content_block_delta` at the block folded in: a
    /// `compaction_delta`'s `content` extends the summary, every other key
    /// but `type` (its `encrypted_content`, a `signature_delta`'s
    /// `signature`) is set on the block as it came.
    pub fn apply_delta(&mut self, delta: &Map<String, Value>) {
        for (k, v) in delta {
            match (k.as_str(), v) {
                ("type", _) | ("content", Value::Null) => {}
                ("content", Value::String(s)) => {
                    self.summary.get_or_insert_with(String::new).push_str(s);
                }
                _ => {
                    self.fields.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

/// A compaction block's stream events, as an Anthropic stream sends them:
/// the block's `content_block_start` (an on-demand block arrives whole in
/// it), then any `content_block_delta` at it (a threshold block's one
/// `compaction_delta` with the whole summary); its `content_block_stop` is
/// the stream's own. Only the Anthropic egress emits them.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionEvent {
    /// The block as its `content_block_start` began it.
    Start(Compaction),
    /// One delta object at the block, verbatim (`compaction_delta`, or any
    /// other type Anthropic sends at the block).
    Delta(Map<String, Value>),
}

impl CompactionEvent {
    /// The summary text the event carries, for a wire without a compaction
    /// block of its own, which writes it as text.
    pub fn summary(&self) -> Option<&str> {
        match self {
            Self::Start(c) => c.summary.as_deref(),
            Self::Delta(d) => d.get("content").and_then(Value::as_str),
        }
    }
}

/// The conversation as a route without compaction blocks is sent it: what
/// Anthropic does with the history before the block (gateway design §6.1).
///
/// - The **last block with a summary** is the one in effect: every content
///   block before it is dropped, in earlier messages and earlier in its own
///   ("the API automatically drops all content blocks prior to the
///   `compaction` block"; "the last compaction block … replac[es] content
///   prior to it").
/// - Its summary opens the kept conversation as a **user** text part: its
///   own user message, or the first part of the next message when that is a
///   user message too. Whatever followed the block inside its own message
///   stays a message of that message's role after it. So a folded
///   conversation starts with a user turn and keeps the roles' alternation
///   as the client had it, which strict chat templates and Gemini's `contents`
///   expect. No framing text is added.
/// - An empty summary (`content: ""`) is in effect all the same (the history
///   before it is dropped, as Anthropic would), but adds no part.
/// - A block without a summary (`content: null`) is a no-op to Anthropic:
///   it is left out, and nothing before it is dropped.
/// - System messages stay: Messages' `system` is not part of `messages`,
///   and the Messages ingress takes no mid-conversation system message.
///
/// A request without a block is returned as it is.
pub fn folded(ir: &ChatRequest) -> Cow<'_, ChatRequest> {
    folded_where(ir, |_| true)
}

/// The conversation as the Anthropic-protocol row whose scope `row` names
/// is sent it: each block it is not sent (one lmgw read from an answer at
/// another scope, module doc) folded as [`folded`] folds every block, each
/// block it is sent kept where it stands. A request without such a block
/// is returned as it is.
pub fn folded_for<'a>(ir: &'a ChatRequest, row: &Signer) -> Cow<'a, ChatRequest> {
    folded_where(ir, |c| !c.goes_to(row))
}

/// [`folded`] of the blocks `fold` picks; every other block stays a block.
fn folded_where(ir: &ChatRequest, fold: impl Fn(&Compaction) -> bool) -> Cow<'_, ChatRequest> {
    let fold = &fold;
    let folds = |p: &ContentPart| matches!(p, ContentPart::Compaction(c) if fold(c));
    if !ir.messages.iter().any(|m| m.content.iter().any(folds)) {
        return Cow::Borrowed(ir);
    }
    let effective = ir
        .messages
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| {
            m.content
                .iter()
                .enumerate()
                .filter_map(move |(pi, p)| match p {
                    ContentPart::Compaction(
                        c @ Compaction {
                            summary: Some(s), ..
                        },
                    ) if fold(c) => Some((mi, pi, s.as_str())),
                    _ => None,
                })
        })
        .last();
    let mut messages = Vec::with_capacity(ir.messages.len() + 1);
    // Where the summary's user turn goes: after the system messages kept
    // in front of the block.
    let mut summary_at = None;
    for (mi, m) in ir.messages.iter().enumerate() {
        if m.role == Role::System {
            messages.push(m.clone());
            continue;
        }
        if let Some((at_m, _, _)) = effective {
            if mi < at_m {
                continue;
            }
            if mi == at_m {
                summary_at = Some(messages.len());
            }
        }
        let skip = match effective {
            Some((at_m, at_p, _)) if mi == at_m => at_p + 1,
            _ => 0,
        };
        let content: Vec<ContentPart> = m
            .content
            .iter()
            .skip(skip)
            .filter(|p| !folds(p))
            .cloned()
            .collect();
        if !content.is_empty() {
            messages.push(Message {
                role: m.role,
                content,
            });
        }
    }
    if let (Some(at), Some((_, _, summary))) = (summary_at, effective) {
        if !summary.is_empty() {
            let part = ContentPart::text(summary.to_string());
            match messages.get_mut(at) {
                Some(next) if next.role == Role::User => next.content.insert(0, part),
                _ => messages.insert(
                    at,
                    Message {
                        role: Role::User,
                        content: vec![part],
                    },
                ),
            }
        }
    }
    let mut out = ir.clone();
    out.messages = messages;
    Cow::Owned(out)
}

/// An Anthropic answer's `usage.iterations`, verbatim, with which cache
/// counters the answer's top-level `usage` itself carried. The IR's usage
/// counts the compaction entries in, cache counters included, so a
/// top-level counter Anthropic left out reads as one there; the Messages
/// wire writes Anthropic's own field set again from these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageIterations {
    /// `usage.iterations`, as it came.
    pub list: Value,
    /// Whether the top level carried `cache_read_input_tokens`.
    #[serde(default)]
    pub top_cache_read: bool,
    /// Whether the top level carried `cache_creation_input_tokens`.
    #[serde(default)]
    pub top_cache_write: bool,
}

/// The tokens the `compaction` entries of an Anthropic answer's
/// `usage.iterations` spent, summed, in the IR's meaning (input with its
/// cache counters included, as [`super::Usage::prompt_tokens`] is): `None`
/// when no entry is a compaction's. Anthropic leaves them out of its
/// top-level counts and bills a request as the sum over every iteration;
/// the Anthropic egress adds them in, the Messages wire takes them out again
/// for its top-level counts.
pub fn compaction_usage(iterations: &Value) -> Option<super::Usage> {
    let mut sum: Option<super::Usage> = None;
    for e in iterations.as_array()? {
        if e.get("type").and_then(Value::as_str) != Some(BLOCK_TYPE) {
            continue;
        }
        let n = |k: &str| e.get(k).and_then(Value::as_u64);
        let read = n("cache_read_input_tokens");
        let write = n("cache_creation_input_tokens");
        let entry = super::Usage {
            prompt_tokens: Some(
                n("input_tokens").unwrap_or(0) + read.unwrap_or(0) + write.unwrap_or(0),
            ),
            completion_tokens: Some(n("output_tokens").unwrap_or(0)),
            cached_input_tokens: read,
            cache_write_tokens: write,
            reasoning_tokens: None,
        };
        sum.get_or_insert_with(super::Usage::default).add(&entry);
    }
    sum
}
