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
    /// `openai | anthropic | gemini | llama_cpp`. `llama_cpp` is llama.cpp's
    /// own server (llama-server, ik_llama.cpp) and is always kind
    /// `llama_server`.
    pub protocol: String,
    /// `generic | llama_server | audio_cpp`. `llama_server` goes with
    /// protocol `llama_cpp` or `anthropic`.
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
    /// A `llama_cpp` row's server: what `GET /props` said, when, or why it
    /// is unknown — one entry for the server, and for a router one more per
    /// model ([`UpstreamLlamaFacts::model`]). Empty on every other protocol,
    /// and on a `llama_cpp` row until a chat request or its Test makes lmgw
    /// ask in the background.
    pub llama_facts: Vec<UpstreamLlamaFacts>,
}

/// What an external llama.cpp server said about itself, or a router about
/// one model (`GET /props`, llama egress design §4.2). Asked in the
/// background, one probe per row at a time, and kept until the row is
/// edited, unreachable, refuses a medium or is tested.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamLlamaFacts {
    /// Empty for the server itself: a llama-server that is no router serves
    /// one model whatever a request names, so its facts hold for every model
    /// of the row. Under a router, the upstream model this entry is about,
    /// as the row's aliases (or an `expose_all` request) name it.
    pub model: String,
    /// The base URL it was asked at.
    pub base_url: String,
    /// The server is a llama-server router (only on the entry with an empty
    /// `model`): it states its build and nothing about a model, so `props`
    /// carries the build alone, and the facts are on the per-model entries.
    /// Defaulted, so a gateway older than the field still parses.
    pub router: bool,
    /// The facts, when the server stated them: its build, one slot's
    /// context and its modalities. `None` is unknown; `unknown` says why.
    pub props: Option<crate::LlamaProps>,
    /// Why the facts are unknown: the server's own answer (an old build's
    /// 404, a 401), or why the last probe found nothing.
    pub unknown: Option<String>,
    /// The answer stands until an event drops it. `false` for a probe that
    /// found nothing worth keeping (a router that has not loaded the model,
    /// a server that did not answer): it is asked again on the next use.
    pub cached: bool,
    /// Unix seconds of the answer shown; `None` while the first is on its way.
    pub read_at: Option<i64>,
    /// Being asked now, or waiting for the row's probe.
    pub probing: bool,
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
