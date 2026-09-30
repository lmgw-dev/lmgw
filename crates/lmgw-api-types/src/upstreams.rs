//! Upstreams

use serde::{Deserialize, Serialize};

/// `GET /api/upstreams`. Secrets never round-trip: `has_api_key` says whether
/// one is stored; header values arrive redacted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamsResponse {
    pub upstreams: Vec<UpstreamView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamView {
    pub id: i64,
    pub name: String,
    /// `openai | anthropic | gemini`.
    pub protocol: String,
    /// `generic | llama_server | audio_cpp`.
    pub kind: String,
    pub base_url: String,
    pub has_api_key: bool,
    /// Names only; values are redacted server-side.
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub enabled: bool,
    pub expose_all: bool,
    pub expose_prefix: String,
    pub supports_responses: bool,
}

/// `GET /api/upstream-models?id=` — live catalog of one upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamModelsResponse {
    pub models: Vec<String>,
    /// The same models, with what the catalog states about each. Defaulted,
    /// so a gateway older than the field still parses (ids only).
    pub entries: Vec<UpstreamModelEntry>,
}

/// One entry of an upstream's live catalog (`catalog::ModelInfo`), including
/// the entries hidden from `/v1/models` — which is why the dashboard reads
/// this rather than `/v1/models`. Every field the catalog does not publish is
/// `None`: unknown, never a default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamModelEntry {
    /// Bare, as the upstream names it (no expose prefix).
    pub id: String,
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// USD per token, the upstream's verbatim decimal string.
    pub price_prompt: Option<String>,
    pub price_completion: Option<String>,
    /// Unix seconds.
    pub created: Option<i64>,
    pub input_modalities: Option<Vec<String>>,
    /// `chat` | `embedding`.
    pub task: Option<String>,
    pub tools: Option<bool>,
    /// `levels` | `toggle` | `fixed`.
    pub reasoning: Option<String>,
    /// Whether it thinks by default, where the catalog says — what tells a
    /// `fixed` model that always reasons from one that never does.
    pub reasoning_enabled: Option<bool>,
}
