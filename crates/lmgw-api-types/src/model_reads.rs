//! The self-admin reads about local models: `GET /api/local-model-check`,
//! `GET /api/model-inspect` and `GET /api/llama-flags`. The MCP tools
//! `lmgw__local_model_check`, `lmgw__model_inspect` and `lmgw__llama_flags`
//! answer the same types as JSON text.

use serde::{Deserialize, Serialize};

/// `GET /api/local-model-check` — the static pre-flight over local models,
/// without starting any of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalModelCheck {
    /// How many models were looked at (all classes asked for).
    pub checked: usize,
    /// How many of `models` have problems (`ok` false).
    pub broken: usize,
    /// How many carry advisories; those never count as broken.
    pub with_advisories: usize,
    /// Models with something to report, or the one asked for by name.
    pub models: Vec<ModelCheck>,
    /// What the check does and does not prove.
    pub note: String,
}

/// One model's pre-flight result; `class` says which kind of model it is and
/// so which fields it has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ModelCheck {
    /// A chat-class model.
    Chat {
        model_id: String,
        enabled: bool,
        /// No problems. Advisories do not count.
        ok: bool,
        /// Why the model cannot load as configured.
        problems: Vec<String>,
        /// The model starts, but misbehaves under some input.
        advisories: Vec<String>,
    },
    /// An embedding, rerank or decision model.
    Aux {
        model_id: String,
        /// `embed`, `rerank` or `decision`.
        kind: String,
        enabled: bool,
        ok: bool,
        problems: Vec<String>,
    },
    /// An image-generation model.
    Image {
        model_id: String,
        /// The modes the row serves (`generate`, `edit`, …).
        modes: Vec<String>,
        edit: bool,
        enabled: bool,
        ok: bool,
        problems: Vec<String>,
    },
}

impl ModelCheck {
    /// Whether the model has no problems.
    pub fn ok(&self) -> bool {
        match self {
            Self::Chat { ok, .. } | Self::Aux { ok, .. } | Self::Image { ok, .. } => *ok,
        }
    }
}

/// `GET /api/llama-flags` — the flag vocabulary of one llama-server image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LlamaFlags {
    /// The image whose `--help` was read.
    pub image: String,
    /// How many flags the image has before the search filter.
    pub flag_count: usize,
    /// The flags matching the search (all of them without one).
    pub flags: Vec<String>,
    /// For flags that take a fixed set of values, the values, keyed by flag.
    pub allowed_values: std::collections::BTreeMap<String, Vec<String>>,
    /// Flags the image removed or renamed away.
    pub removed: Vec<String>,
    /// Why the vocabulary is per image.
    pub note: String,
}

/// The licence a GGUF header states (`general.license`, with the optional
/// `general.license.name` and `general.license.link`). Information only: it
/// is never a filter, a ranking or a warning. At least one field is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelLicence {
    /// `general.license`: an SPDX-like id or a name, such as `apache-2.0`
    /// or `other`.
    pub id: Option<String>,
    /// `general.license.name`.
    pub name: Option<String>,
    /// `general.license.link`.
    pub link: Option<String>,
}

impl ModelLicence {
    /// What a chip says: the name when the header gives one, else the id.
    /// `None` for a licence that only has a link.
    pub fn label(&self) -> Option<&str> {
        self.name.as_deref().or(self.id.as_deref())
    }
}

/// `GET /api/model-inspect` — what a GGUF file is, from its header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelInspect {
    /// The path under the class's models dir.
    pub path: String,
    /// The models dir class the path was resolved in: `chat` or `aux`.
    pub target: String,
    /// `weights`, `mmproj` (a projector), `drafter` or `unknown`.
    pub role: String,
    /// Which class serves this file, from its header rather than its
    /// directory: `chat` or `aux`.
    pub serve_as: String,
    /// For an aux file: `embed`, `rerank` or `decision`.
    pub aux_kind: Option<String>,
    /// `none`, `mean`, `cls`, `last` or `rank`; set on encoders.
    pub pooling_type: Option<String>,
    /// A classification head, as a cross-encoder reranker has.
    pub has_classifier_head: bool,
    /// The `spec_type` the file calls for when it is a drafter or carries
    /// MTP layers.
    pub suggested_spec_type: Option<String>,
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub size_label: Option<String>,
    /// The licence the header states; null when it states none.
    pub licence: Option<ModelLicence>,
    pub quant: Option<String>,
    /// The model's trained context, in tokens.
    pub context_length: Option<u64>,
    pub block_count: Option<u64>,
    pub head_count: Option<u64>,
    pub head_count_kv: Option<u64>,
    /// Per-layer KV heads, on models whose layers differ.
    pub head_count_kv_per_layer: Option<Vec<u64>>,
    pub sliding_window: Option<u64>,
    pub sliding_window_pattern: Option<u64>,
    /// `sliding_window_pattern` counts layers described, not a stride.
    pub sliding_window_pattern_is_array: bool,
    pub full_attention_interval: Option<u64>,
    pub has_chat_template: bool,
    /// Text heuristics over the chat template; null without a template.
    pub signals: Option<TemplateSignals>,
    pub has_mtp_layers: bool,
    /// On a projector file: its type.
    pub projector_type: Option<String>,
    pub vision_block_count: Option<u64>,
    pub has_vision_encoder: Option<bool>,
    pub has_audio_encoder: Option<bool>,
    pub file_size_bytes: u64,
    /// `file_size_bytes` for a person to read.
    pub file_size: String,
    pub vram_estimate: VramEstimate,
}

/// What the chat template's own text says about reasoning, effort selection
/// and tool-call rendering. Pure text heuristics: a template that does it
/// through indirection reads as absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplateSignals {
    /// The template renders a reasoning trace somewhere.
    pub thinking_markers: bool,
    /// The first marker that matched, verbatim.
    pub thinking_marker: Option<String>,
    /// The template reads an `enable_thinking` variable.
    pub enable_thinking_var: bool,
    /// The default of `enable_thinking`, when the template states one.
    pub enable_thinking_default: Option<bool>,
    /// The template reads a reasoning-effort variable of any name.
    pub reasoning_effort_var: bool,
    /// Which effort variables it reads.
    pub effort_var_names: Vec<String>,
    /// The effort levels it compares against, in canonical order.
    pub effort_levels: Vec<String>,
    /// The effort it falls back to, verbatim.
    pub effort_default: Option<String>,
    /// The template reads `preserve_thinking`.
    pub preserve_thinking_var: bool,
    /// `tools` is used as a variable.
    pub tools_var: bool,
    /// The template loops over `tool_calls`: several calls per turn.
    pub parallel_tool_calls: bool,
    /// The native tool-call syntax family it renders.
    pub tool_call_format: Option<String>,
}

/// The GPU cost of the weights and the KV cache at the trained context; a
/// chat model's is laid out as its row is charged, for one slot. Compute
/// buffers depend on a row's settings and are in the plan's footprint, not
/// here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramEstimate {
    pub weights_bytes: u64,
    /// KV cache for `context_length` tokens at f16; null when the header
    /// lacks what the estimate needs.
    pub kv_cache_bytes_at_full_ctx_f16: Option<u64>,
    /// The same at q8_0.
    pub kv_cache_bytes_at_full_ctx_q8_0: Option<u64>,
    /// What the figures leave out.
    pub note: String,
}
