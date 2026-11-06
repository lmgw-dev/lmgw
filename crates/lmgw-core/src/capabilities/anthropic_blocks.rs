//! Anthropic's capability blocks on an Anthropic-shaped model entry
//! (model-capabilities design §2.2.1).
//!
//! `GET /v1/models` answers in Anthropic's shape when the caller sends
//! `anthropic-version`, and Anthropic's `ModelInfo.capabilities` is an object
//! of fixed blocks (`batch`, `citations`, `code_execution`, `compaction`,
//! `context_management`, `effort`, `image_input`, `pdf_input`,
//! `server_tools`, `structured_outputs`, `thinking`), each a `supported`
//! boolean. That dialect's `capabilities` is lmgw's own object with these
//! merged in ([`anthropic_capabilities`]); the key sets do not overlap
//! (lmgw says `structured_output`, `reasoning` and `context_editing`,
//! Anthropic `structured_outputs`, `thinking` and `context_management`), and
//! Anthropic's object takes keys it does not define.
//!
//! **The rule for every block:** `supported: true` only when a
//! `/v1/messages` request that uses the feature, sent to lmgw for this model,
//! really works — lmgw translates it, or passes it through to an upstream
//! that does it. Anthropic's schema has no "unknown", so whatever lmgw does
//! not serve yet, or does not know, is `false`: the one answer lmgw can keep.
//! lmgw's own fields in the same object keep "absent means unknown".
//!
//! Pure: the blocks come from the [`ModelCapabilities`] lmgw already derived
//! and nothing else, so the two halves of the object cannot disagree. The
//! catalog reads an Anthropic upstream's blocks the other way round
//! (`catalog::parse_reasoning_anthropic` and its siblings: `thinking` →
//! `reasoning.kind`, `thinking.types` → `reasoning.thinking_types` and
//! `reasoning.can_disable`, `effort.<level>` → `reasoning.levels`, `image_input` →
//! `vision`, `structured_outputs` → `structured_output.json_schema`,
//! `context_management` → `context_editing`, `compaction` →
//! `context_editing.compaction`), and the mapping here is that reading's
//! inverse.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::ModelCapabilities;

/// One Anthropic capability: `{"supported": bool}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicSupport {
    /// `true` only when a `/v1/messages` request using this feature works on
    /// this model through lmgw; `false` when lmgw does not serve it yet or
    /// does not know.
    pub supported: bool,
}

impl AnthropicSupport {
    const fn of(supported: bool) -> Self {
        Self { supported }
    }
}

/// `effort`: whether `output_config.effort` reaches the model, and which
/// levels it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicEffort {
    /// The model has selectable effort levels (`reasoning.kind` is
    /// `levels`): a request's `output_config.effort` reaches it.
    pub supported: bool,
    /// `reasoning.levels` contains `low`.
    pub low: AnthropicSupport,
    /// `reasoning.levels` contains `medium`.
    pub medium: AnthropicSupport,
    /// `reasoning.levels` contains `high`.
    pub high: AnthropicSupport,
    /// `reasoning.levels` contains `xhigh`.
    pub xhigh: AnthropicSupport,
    /// `reasoning.levels` contains `max`.
    pub max: AnthropicSupport,
}

/// `server_tools`: Anthropic's server-run tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicServerTools {
    /// Always `false` yet: the Messages ingress refuses every tool without an
    /// `input_schema` (a server tool) before any upstream sees it.
    pub supported: bool,
    /// The code execution server tool; `false` yet, as `supported`.
    pub code_execution: AnthropicSupport,
    /// The web search server tool; `false` yet, as `supported`.
    pub web_search: AnthropicSupport,
}

/// `context_management`: whether a request's `context_management` reaches
/// the model, and which of its edit types work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicContextManagement {
    /// An Anthropic route, which sends `context_management` on as written
    /// (lmgw's `context_editing` is present), whose catalog says the model
    /// takes it (`context_editing.supported` is `true`); unpublished is
    /// `false`.
    pub supported: bool,
    /// `context_management.edits` names `clear_thinking_20251015`.
    pub clear_thinking_20251015: AnthropicSupport,
    /// `context_management.edits` names `clear_tool_uses_20250919`.
    pub clear_tool_uses_20250919: AnthropicSupport,
    /// `context_management.edits` names `compact_20260112`: the edit goes
    /// out as written and the `compaction` block Anthropic answers with
    /// comes back as it came.
    pub compact_20260112: AnthropicSupport,
}

/// `compaction`: on-demand compaction (the top-level `compaction`), and
/// which of its types work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicCompaction {
    /// An Anthropic route (lmgw's `context_editing` is present) whose
    /// catalog names an on-demand compaction type for the model
    /// (`context_editing.compaction`): the field goes out as written and
    /// the `compaction` block comes back as it came. Unpublished is `false`.
    pub supported: bool,
    /// `context_editing.compaction` names `summarize`.
    pub summarize: AnthropicSupport,
}

/// `thinking.types`: which of Messages' `thinking` types work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicThinkingTypes {
    /// `{"type": "adaptive"}` (lmgw's bare "on") makes the model think. On
    /// an Anthropic route, which sends it as thinking type adaptive: the
    /// model's catalog names the type (`reasoning.thinking_types`). Elsewhere:
    /// its default is on, or "on" reaches it (the template's
    /// `enable_thinking` on a local row).
    pub adaptive: AnthropicSupport,
    /// `{"type": "between_tools"}` (no up-front thinking) reaches a model
    /// that takes it: on an Anthropic route, which sends the type as
    /// written, the model's catalog names it (`reasoning.thinking_types`).
    /// Elsewhere `false`: those routes send their own off instead, and say
    /// so in `x-lmgw-reasoning-ignored`.
    pub between_tools: AnthropicSupport,
    /// `{"type": "disabled"}` switches thinking off
    /// (`reasoning.can_disable` is `true`; on an Anthropic route, the
    /// catalog's `thinking.types.disabled`).
    pub disabled: AnthropicSupport,
    /// `{"type": "enabled", "budget_tokens": n}`: the budget reaches the
    /// model. On an Anthropic route, which forwards the budget form as
    /// written: the model's catalog names the type
    /// (`reasoning.thinking_types`). Elsewhere: `reasoning.control` names
    /// `x-lmgw-reasoning-budget`.
    pub enabled: AnthropicSupport,
}

/// `thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicThinking {
    /// The model reasons: `reasoning.kind` is `toggle` or `levels`, or
    /// `fixed` with `enabled: true`.
    pub supported: bool,
    /// Which `thinking` types work; all `false` when `supported` is.
    pub types: AnthropicThinkingTypes,
}

/// Anthropic's capability blocks, as lmgw serves them on `/v1/messages`
/// (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AnthropicBlocks {
    /// Always `false` yet: lmgw serves no `/v1/messages/batches` yet.
    pub batch: AnthropicSupport,
    /// Always `false` yet. A `document` or `search_result` block reaches an
    /// Anthropic-protocol route as the client wrote it and an answer's
    /// citations come back to a Messages client (gateway design §6.1,
    /// "Citations", 2026-11-06), but lmgw does not read the catalog's
    /// `citations` block yet, so it cannot say per model.
    pub citations: AnthropicSupport,
    /// Always `false` yet: the code execution tool is a server tool, which
    /// the Messages ingress refuses.
    pub code_execution: AnthropicSupport,
    pub compaction: AnthropicCompaction,
    pub context_management: AnthropicContextManagement,
    pub effort: AnthropicEffort,
    /// `vision` is `true` and the model is served on `/v1/messages`, whose
    /// ingress takes `image` blocks (base64 and URL sources).
    pub image_input: AnthropicSupport,
    /// An Anthropic-protocol route (`context_editing` present) to a model
    /// whose catalog says `pdf_input` (`input_modalities` holds `pdf`): a
    /// `document` block reaches it as the client wrote it, base64, URL or
    /// Files API source (gateway design §6.1, "Anthropic blocks lmgw does
    /// not model"). `false` on every other route, which is sent no
    /// `document` block.
    pub pdf_input: AnthropicSupport,
    pub server_tools: AnthropicServerTools,
    /// `structured_output.json_schema` is `true`: the ingress reads
    /// `output_config.format` (and the older `output_format`) of type
    /// `json_schema`.
    pub structured_outputs: AnthropicSupport,
    pub thinking: AnthropicThinking,
}

/// The `/v1/messages` route: a model not served there gets `false` in every
/// block, whatever else it can do.
const MESSAGES: &str = "/v1/messages";

/// The control entry that names Messages' own `thinking` field: only an
/// Anthropic route publishes it (`ANTHROPIC_REASONING_CONTROL`), where the
/// field goes out as Anthropic's own, "on" is rendered as thinking type
/// adaptive and a budget as type enabled — so the model's published
/// `thinking_types` say which of them work.
const NATIVE_THINKING: &str = "thinking";
/// The template variable a local row's "on" and "off" reach.
const TEMPLATE_TOGGLE: &str = "chat_template_kwargs.enable_thinking";
/// The control entry saying a reasoning budget reaches the model.
const BUDGET: &str = "x-lmgw-reasoning-budget";

/// Anthropic's blocks for one model, from lmgw's own facts (module doc).
pub fn anthropic_blocks(caps: &ModelCapabilities) -> AnthropicBlocks {
    let no = AnthropicSupport::of(false);
    let serves = caps.endpoints.iter().any(|e| e == MESSAGES);
    let reasoning = caps.reasoning.as_ref().filter(|_| serves);
    let control = |c: &str| reasoning.is_some_and(|r| r.control.iter().any(|x| x == c));

    let thinks = reasoning.is_some_and(|r| match r.kind.as_str() {
        "toggle" | "levels" => true,
        "fixed" => r.enabled == Some(true),
        _ => false,
    });
    let on_by_default = reasoning.is_some_and(|r| r.enabled == Some(true));
    // An Anthropic route sends the type as Messages defines it, so the
    // model's own published types decide there; unpublished is `false`.
    let anthropic_route = control(NATIVE_THINKING);
    let takes = |t: &str| reasoning.is_some_and(|r| r.thinking_types.iter().any(|x| x == t));
    let adaptive = thinks
        && if anthropic_route {
            takes("adaptive")
        } else {
            on_by_default || control(TEMPLATE_TOGGLE)
        };
    let budget = thinks
        && if anthropic_route {
            takes("enabled")
        } else {
            control(BUDGET)
        };
    let disabled = thinks && reasoning.is_some_and(|r| r.can_disable == Some(true));
    let between_tools = thinks && anthropic_route && takes("between_tools");

    // What the catalog says, as for the thinking types: unpublished is
    // `false` (Anthropic's schema has no "unknown").
    let cm = caps
        .context_editing
        .as_ref()
        .filter(|c| serves && c.supported == Some(true));
    let edit = |e: &str| AnthropicSupport::of(cm.is_some_and(|c| c.edits.iter().any(|x| x == e)));
    // On-demand compaction is a field of its own, not gated on
    // `context_management.supported`: the catalog's own list decides.
    let compaction = caps
        .context_editing
        .as_ref()
        .filter(|_| serves)
        .map(|c| c.compaction.as_slice())
        .unwrap_or_default();
    let compacts = |t: &str| AnthropicSupport::of(compaction.iter().any(|x| x == t));

    let levels = reasoning
        .filter(|r| r.kind == "levels")
        .map(|r| r.levels.as_slice());
    let level = |l: &str| AnthropicSupport::of(levels.is_some_and(|ls| ls.iter().any(|x| x == l)));

    AnthropicBlocks {
        batch: no,
        citations: no,
        code_execution: no,
        compaction: AnthropicCompaction {
            supported: !compaction.is_empty(),
            summarize: compacts("summarize"),
        },
        context_management: AnthropicContextManagement {
            supported: cm.is_some(),
            clear_thinking_20251015: edit("clear_thinking_20251015"),
            clear_tool_uses_20250919: edit("clear_tool_uses_20250919"),
            compact_20260112: edit("compact_20260112"),
        },
        effort: AnthropicEffort {
            supported: levels.is_some(),
            low: level("low"),
            medium: level("medium"),
            high: level("high"),
            xhigh: level("xhigh"),
            max: level("max"),
        },
        image_input: AnthropicSupport::of(serves && caps.vision == Some(true)),
        pdf_input: AnthropicSupport::of(
            serves
                && caps.context_editing.is_some()
                && caps
                    .input_modalities
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|x| x == "pdf")),
        ),
        server_tools: AnthropicServerTools {
            supported: false,
            code_execution: no,
            web_search: no,
        },
        structured_outputs: AnthropicSupport::of(
            serves && caps.structured_output.as_ref().and_then(|s| s.json_schema) == Some(true),
        ),
        thinking: AnthropicThinking {
            supported: thinks,
            types: AnthropicThinkingTypes {
                adaptive: AnthropicSupport::of(adaptive),
                between_tools: AnthropicSupport::of(between_tools),
                disabled: AnthropicSupport::of(disabled),
                enabled: AnthropicSupport::of(budget),
            },
        },
    }
}

/// The Anthropic dialect's `capabilities`: lmgw's own object, unchanged,
/// with [`anthropic_blocks`] merged into it.
pub fn anthropic_capabilities(caps: &ModelCapabilities) -> Value {
    let mut out = serde_json::to_value(caps).unwrap_or(Value::Null);
    if let (Some(obj), Value::Object(blocks)) = (
        out.as_object_mut(),
        serde_json::to_value(anthropic_blocks(caps)).unwrap_or(Value::Null),
    ) {
        obj.extend(blocks);
    }
    out
}
