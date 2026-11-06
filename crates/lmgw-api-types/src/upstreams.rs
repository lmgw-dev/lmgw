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
    /// `openai | openai_compatible | anthropic | gemini | llama_cpp`.
    /// `openai` is the OpenAI platform (api.openai.com) only and always kind
    /// `generic`; `openai_compatible` is every other OpenAI-shaped provider.
    /// `llama_cpp` is llama.cpp's own server (llama-server, ik_llama.cpp) and
    /// is always kind `llama_server`.
    pub protocol: String,
    /// `generic | llama_server | audio_cpp`. `llama_server` goes with
    /// protocol `llama_cpp` or `anthropic`; `audio_cpp` (an external
    /// audio.cpp server) with `openai_compatible`.
    pub kind: String,
    pub base_url: String,
    pub has_api_key: bool,
    /// Names only; values are redacted server-side.
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub enabled: bool,
    pub expose_all: bool,
    pub expose_prefix: String,
    /// `/v1/responses` is forwarded to it natively rather than synthesized
    /// from `/v1/chat/completions`. A choice on `openai_compatible` rows only:
    /// implied on `openai`, and `false` on every other protocol.
    pub supports_responses: bool,
    /// The catalog request asks for every output modality
    /// (`GET …/models?output_modalities=all`), so OpenRouter lists its
    /// decision, embedding, rerank, speech, transcription, image and video
    /// models too. Only an `openai_compatible` row of kind `generic` sends
    /// it; on by default, off for an endpoint that refuses it (Gemini's
    /// OpenAI-compatible one).
    pub catalog_all_modalities: bool,
    /// Set on an `openai` row whose host is not api.openai.com or a
    /// regional `*.api.openai.com` one: lmgw sends it the OpenAI platform's
    /// own dialect, which another provider may refuse. The same note the
    /// write and the Test answer with. `None` on every other row.
    pub host_note: Option<String>,
    /// A `llama_cpp` row's server: what `GET /props` said, when, or why it
    /// is unknown — one entry for the server, and for a router one more per
    /// model (its `model`). Empty on every other protocol,
    /// and on a `llama_cpp` row until a chat request or its Test makes lmgw
    /// ask in the background.
    pub llama_facts: Vec<UpstreamLlamaFacts>,
    /// Set while lmgw remembers the row out of quota (openai egress design
    /// §14.6): its account answered `insufficient_quota`, so models and
    /// aliases on it that have a fallback answer with the fallback until
    /// the next check. `None` otherwise.
    pub quota: Option<UpstreamQuota>,
}

/// What lmgw remembers of an upstream's exhausted quota (openai egress
/// design §14.6). In memory: a restart forgets it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamQuota {
    /// Unix seconds of the first quota answer since it was last clear.
    pub since: i64,
    /// The provider's code (`insufficient_quota`,
    /// `credit_balance_exhausted`, …), when it sent one.
    pub code: Option<String>,
    /// The provider's message, verbatim.
    pub message: String,
    /// Unix seconds of the next check: the next request after it goes to
    /// the upstream again. The upstream's `Retry-After` when it sent one,
    /// otherwise `quota_recheck_seconds` after the answer.
    pub next_check: i64,
}

/// An upstream lmgw remembers out of quota, as `GET /api/status` lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamQuotaStatus {
    pub id: i64,
    pub name: String,
    pub quota: UpstreamQuota,
}

/// What an external llama.cpp server said about itself, or a router about
/// one model (`GET /props`). Asked in the
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

/// `POST /api/op/upstream_test`: the upstream answered its model list. A failed
/// connection is an error answer instead, so this one only ever says it worked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamTested {
    /// Always `true`; a failed test is an error answer instead.
    pub ok: bool,
    /// The row's `host_note` ([`UpstreamView::host_note`]): it answered, and
    /// it is an `openai` row on a host that is not OpenAI's.
    pub note: Option<String>,
}

/// `POST /api/op/model_visibility`: models of an `expose_all` upstream's live
/// catalog were hidden from, or restored to, the client-facing model list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelVisibilitySaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// What was done, naming the model or how many, for a person to read.
    pub message: String,
    /// How many distinct models the call named: `model_id` and `model_ids`
    /// together, trimmed, empty entries and repeats dropped.
    pub count: usize,
}

impl ModelVisibilitySaved {
    /// `{"ok": true, "message": message, "count": count}`.
    pub fn new(message: impl Into<String>, count: usize) -> Self {
        Self {
            ok: true,
            message: message.into(),
            count,
        }
    }
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
