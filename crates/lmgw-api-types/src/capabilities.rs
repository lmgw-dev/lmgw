//! The `capabilities` object of a model on `GET /v1/models`, all the way down
//! (model-capabilities design `docs/design/2026-09-17-model-capabilities-design.md`
//! §2.1). Only the shapes live here; the gateway derives them (and checks an
//! owner's override against them) in lmgw-core's `capabilities` module.
//!
//! Every type is strict (`deny_unknown_fields`) on purpose: an override that
//! misspells a key is refused by parsing into [`ModelCapabilities`].

use serde::{Deserialize, Serialize};

/// The `capabilities` object of one model on `GET /v1/models`.
///
/// `task`, `endpoints` and `source` are always present — they are what makes
/// the entry actionable at all ("this id exists, here is where to send it,
/// here is how much to trust the rest"). Everything else is absent unless a
/// source stated it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    /// `chat` | `embedding` | `rerank` | `decision` (a decision model,
    /// answering on `/v1/systemone`) | `tts` | `asr` | an audio.cpp task
    /// name | an image task | `realtime` (an OpenAI Realtime model, no lmgw
    /// route). Determines `endpoints`.
    pub task: String,
    /// The lmgw routes that accept this model id.
    pub endpoints: Vec<String>,
    /// Subset of `text`, `image`, `audio`, `video`, `file`, `pdf`,
    /// `embedding`. `pdf` is a catalog's word (Anthropic's `pdf_input`,
    /// Kilo's and OpenRouter's modality).
    /// Absent = unknown, never "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    /// The same vocabulary, plus `decisions`: what a decision model answers
    /// with (llama.cpp's and OpenRouter's word). A client that checks
    /// llama.cpp's `architecture.output_modalities` for `decisions` reads
    /// this list instead. Absent = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    /// `input_modalities` contains `image`. Redundant with the list on
    /// purpose: a client can read this one boolean to decide whether to
    /// send images, e.g. whether a session gets a screenshot tool. Absent
    /// exactly when `input_modalities` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningCaps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<ToolCallCaps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutputCaps>,
    /// Anthropic's context management (`context_management` on
    /// `/v1/messages`: context editing and threshold compaction, and the
    /// top-level on-demand `compaction`), under a name of its
    /// own because the Anthropic-shaped model entry puts Anthropic's
    /// `context_management` block beside lmgw's object. Present only on an
    /// Anthropic-protocol route, the one route that sends a request's
    /// `context_management` on as written; every other route leaves it out
    /// and names it in `x-lmgw-params-ignored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_editing: Option<ContextEditingCaps>,
    /// ISO 639-1 codes (a few three-letter ones, `yue`) of the languages a
    /// local audio model works in, whatever its task: speech, recognition,
    /// alignment. A request's `language` takes them (lmgw sends each in the
    /// model's own spelling). Absent when the model's package names none
    /// as codes; see `multilingual` for a family that says only that it
    /// covers many.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// The model's family says it is multilingual without listing codes
    /// ("multilingual", "80+ languages"): it works in more languages than
    /// `languages` names, which then holds few or none. Absent otherwise;
    /// absent along with `languages` means unknown, never "one language".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multilingual: Option<bool>,
    /// A text-to-speech model's expressive controls — what `instructions`,
    /// inline tags, `language` and `stream_format` do on it
    /// (the speech capabilities object). A local audio model's is derived; a
    /// cloud alias has one only through an alias's capabilities override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<SpeechCaps>,
    /// Where the facts came from: `gguf+config`, `catalog`, `config`, or
    /// `owner` (an alias's capabilities override).
    pub source: String,
}

/// Reasoning / thinking support and how a request controls it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ReasoningCaps {
    /// `fixed` (nothing to change per request), `toggle` (on/off per
    /// request), `levels` (effort selectable per request).
    pub kind: String,
    /// The default state as this alias is configured. Absent when no source
    /// states it — "the catalog says the model supports reasoning" does not
    /// say whether it is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Accepted effort values, least → most in the canonical order. Only for
    /// `kind == "levels"`; omitted entirely when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
    /// The effort in force when the request sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Whether a request can switch thinking off entirely. On an Anthropic
    /// route: the catalog's `thinking.types.disabled.supported`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_disable: Option<bool>,
    /// The configured default thinking-token budget, when set and non-zero
    /// (a zero budget is published as `enabled: false`, not as a budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i64>,
    /// Whether replayed reasoning of earlier turns reaches the model again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_history: Option<bool>,
    /// The exact header and body fields that work on this route; omitted when
    /// empty (nothing to control on a `fixed` model).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control: Vec<String>,
    /// The Messages `thinking` types the model takes, as an Anthropic
    /// upstream's catalog publishes them (`capabilities.thinking.types`), in
    /// Anthropic's order: `adaptive`, `between_tools`, `disabled`, `enabled`
    /// (the budget form). Only on an Anthropic route; omitted when the
    /// catalog names none of them as supported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_types: Vec<String>,
}

/// Tool-calling support.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ToolCallCaps {
    /// `native` (lmgw returns structured `tool_calls`), `none` (the template
    /// renders no tools; aux/audio rows), `text` (the template renders tools
    /// in a syntax llama-server has no parser for, so calls may arrive as
    /// prose the caller must read itself).
    pub kind: String,
    /// The template can render more than one call per turn. Absent for cloud
    /// models, which publish nothing about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    /// The native syntax family the template renders (`qwen-xml`,
    /// `hermes-json`, …), `provider` for cloud, `unknown` when the template
    /// renders tools with none of the known markers. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// `response_format` support.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StructuredOutputCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_object: Option<bool>,
}

/// The `context_editing` object of a model on an Anthropic route: what
/// its provider's catalog says about Anthropic's context management
/// (`capabilities.context_management` on Anthropic's own `/v1/models`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ContextEditingCaps {
    /// The provider says whether the model takes `context_management` at
    /// all. Absent when its catalog says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<bool>,
    /// The `context_management.edits[].type` values the provider says the
    /// model takes, all of which lmgw sends on (`clear_thinking_20251015`,
    /// `clear_tool_uses_20250919`, `compact_20260112`, …), sorted. A
    /// compaction edit's `compaction` answer block comes back as Anthropic
    /// sent it. Omitted when the provider names none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<String>,
    /// The top-level `compaction.type` values (on-demand compaction) the
    /// provider says the model takes (`summarize`, …), sorted: its catalog's
    /// `capabilities.compaction`. lmgw sends the field on as written, and
    /// the `compaction` block Anthropic answers with comes back as it came.
    /// Omitted when the provider names none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction: Vec<String>,
}

/// The `speech` object of a text-to-speech model's capabilities. Absent
/// fields are unknown, as everywhere in the schema.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SpeechCaps {
    /// What `instructions` do: `none` (dropped — said in `x-lmgw-speech`),
    /// `style` (a speaking style for the voice), `voice_design` (the voice
    /// is designed from them), `passthrough` (sent; the model reads them —
    /// it declares an instruction option, or lmgw knows its engine reads
    /// one anyway — and decides what they do).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// A request without instructions is refused (`instructions_required`)
    /// unless the row has a default description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_required: Option<bool>,
    /// What inline tags in `input` do: `none` (stripped, never read out),
    /// `fixed` (the ones in `tags` are rendered, stage directions mapped
    /// onto them, the rest stripped), `free` (every `[tag]` is rendered).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_tags: Option<String>,
    /// The tags a `fixed` model renders, written `[tag]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `stream_format` (`sse` or `audio`, `response_format: pcm`) works on
    /// this row: it runs in streaming mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// The sample rate of its audio, in Hz, as its last answer through lmgw
    /// said (a streamed answer's PCM has no header of its own; the same
    /// figure is the `x-lmgw-sample-rate` header on it). Absent until the
    /// model has answered once since lmgw started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
}
