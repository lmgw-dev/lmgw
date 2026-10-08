//! Routes, upstreams, model aliases and candidate aliases: the wire-facing
//! config types the gateway resolves a request against, plus the fallback
//! types routing produces.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ir::Params;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Openai,
    Anthropic,
    Gemini,
    /// llama.cpp's `llama-server` and ik_llama.cpp's: OpenAI-shaped HTTP
    /// with a dialect of its own (llama.cpp egress design, decision 10).
    /// Always kind [`UpstreamKind::LlamaServer`] (decision 11). Spelled
    /// `llama_cpp`, which `rename_all = "lowercase"` would not produce.
    #[serde(rename = "llama_cpp")]
    LlamaCpp,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::LlamaCpp => "llama_cpp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "llama_cpp" => Some(Self::LlamaCpp),
            _ => None,
        }
    }

    /// The upstream answers OpenAI's HTTP surface: its paths
    /// (`/chat/completions`, `/completions`, `/models`, `/audio/*`), its
    /// bearer auth and its error shape. True for `openai` and `llama_cpp`;
    /// what is said on that surface is the egress's business.
    pub fn speaks_openai_http(&self) -> bool {
        matches!(self, Self::Openai | Self::LlamaCpp)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    Generic,
    LlamaServer,
    /// audio.cpp `audiocpp_server` (OpenAI-shaped `/v1/audio/*` endpoints).
    AudioCpp,
    /// stable-diffusion.cpp `sd-server` (OpenAI-shaped `/v1/images/*`
    /// endpoints; image-generation design §3).
    ///
    /// **Never stored.** Unlike the three above it exists only on the
    /// synthetic image upstream ([`IMAGE_UPSTREAM_ID`]), which is built in
    /// memory on every route and never written — so `upstreams.kind`'s CHECK
    /// constraint deliberately does not list it, and
    /// [`crate::ops::parse_upstream_kind`] refuses it from an owner-supplied
    /// `upstream_set` rather than letting the INSERT fail against that CHECK.
    SdCpp,
}

impl UpstreamKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::LlamaServer => "llama_server",
            Self::AudioCpp => "audio_cpp",
            Self::SdCpp => "sd_cpp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "generic" => Some(Self::Generic),
            "llama_server" => Some(Self::LlamaServer),
            "audio_cpp" => Some(Self::AudioCpp),
            "sd_cpp" => Some(Self::SdCpp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub id: i64,
    pub name: String,
    pub protocol: Protocol,
    pub kind: UpstreamKind,
    /// Base URL up to but excluding the protocol-specific path,
    /// e.g. `https://api.openai.com/v1`, `https://api.anthropic.com`,
    /// `https://generativelanguage.googleapis.com`, `http://127.0.0.1:9292/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    /// Per-request ceiling. **0 means the maximum possible** — no deadline of
    /// lmgw's own, the request ends when the upstream ends it. Read through
    /// [`Upstream::request_timeout`], never directly, so that rule holds at
    /// every call site.
    pub timeout_ms: u64,
    pub enabled: bool,
    /// Expose the upstream's whole model catalog without per-model aliases:
    /// any requested model under [`expose_prefix`](Self::expose_prefix) is
    /// passed through verbatim (prefix stripped).
    pub expose_all: bool,
    /// Namespace for passthrough models (`groq` → clients request
    /// `groq/<model>`); empty = bare passthrough of any unknown model name.
    pub expose_prefix: String,
    /// This upstream implements OpenAI's `/v1/responses` natively (§21).
    ///
    /// When set, `/v1/responses` forwards the body verbatim rather than
    /// synthesizing the API from `/v1/chat/completions`. That matters beyond
    /// tidiness: a real Responses upstream round-trips its own reasoning items,
    /// which down-translating to chat/completions would throw away — so a
    /// synthesized call to OpenAI would be *worse* than talking to it directly.
    ///
    /// Off by default and never inferred from the protocol: `openai` protocol
    /// means "speaks the OpenAI wire shape", which llama-server, vLLM and most
    /// local servers do without implementing Responses at all. Guessing would
    /// turn every request into a 404.
    pub supports_responses: bool,
    /// What the llama-server this route goes to said about itself, and the
    /// tool-image decision made on it (llama egress design §3.2) — decided
    /// once per chat send by `gate::fit_chat`, carried unchanged by
    /// `LocalHold::point_at`. Runtime only, never stored or serialized.
    /// `None` on every non-llama route and while nothing is known, which
    /// means today's bytes (decision 14).
    #[serde(skip)]
    pub llama: Option<std::sync::Arc<LlamaRoute>>,
}

impl Upstream {
    /// Base URL with exactly no trailing slash.
    pub fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Exposure prefix without stray slashes (empty = bare passthrough).
    pub fn prefix(&self) -> &str {
        self.expose_prefix.trim_matches('/')
    }

    /// The per-request ceiling as a deadline, or `None` for **no deadline of
    /// lmgw's own** — the maximum possible, which is what `timeout_ms = 0`
    /// means everywhere: on a stored upstream, and on the four synthetic ones
    /// whose value comes from their class's `request_timeout_seconds`.
    ///
    /// The single reading of this field. Before it, every call site spelled
    /// `Duration::from_millis(timeout_ms.max(1))`, which turned a 0 into a
    /// one-millisecond deadline — the opposite of what an owner who clears
    /// the field means, and a hidden floor besides (house rule: no invented
    /// bound). `None` reaches `send_local` and the reqwest builders as "do not
    /// set one", so the only limits left in play are the client's and the
    /// upstream's own.
    pub fn request_timeout(&self) -> Option<Duration> {
        (self.timeout_ms > 0).then(|| Duration::from_millis(self.timeout_ms))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    pub id: i64,
    pub alias: String,
    pub upstream_id: i64,
    pub upstream_model_id: String,
    pub param_overrides: Params,
    pub enabled: bool,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object with optional keys
    /// `capabilities` (deep-merged over what the row/catalog derives),
    /// `max_output_tokens`, `notes` (appended). Applied by
    /// `capabilities::apply_owner_override`, which sets `source: "owner"` on
    /// the result. `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
}

/// A candidate alias (candidate-aliases design §4.1): an alias whose target
/// is a **primary** local chat model plus an ordered list of **alternates**,
/// used only when already loaded. Resolution — which of `candidates`
/// actually answers a given request, background vs. not — is entirely the
/// gate worker's job (phase 4 packages C1/C2); nothing on this struct or on
/// [`Snapshot`] decides that. What lives here is the saved contract: the
/// list, the background flag, the one whole-alias fallback, and the
/// explicit capability set §4.6 describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateAlias {
    pub id: i64,
    pub alias: String,
    /// Local chat model ids (`LocalModel::model_id` — a row's own id, not a
    /// prefixed/public name), first = primary, rest = alternates in
    /// preference order (§4.1). Refused empty at save time; never consulted
    /// for *what* they are here — a candidate that stopped existing, was
    /// disabled, or lost a facet is still listed, and shows up as a problem
    /// instead (`candidates::derive`).
    #[serde(default)]
    pub candidates: Vec<String>,
    /// Background traffic is a GPU guest and never disturbs the owner
    /// (§4.3–§4.5); the gate worker reads this to pick which resolution
    /// order (§4.2 vs §4.3) applies to a request against this alias.
    #[serde(default)]
    pub background: bool,
    /// Same three modes as a row's GPU-hold fallback, and the same reason for
    /// two columns rather than one nullable string — see
    /// [`HoldFallbackMode`]'s own doc comment. A candidate's own row
    /// fallback is never used through the alias (§4.1): this is the only
    /// fallback that answers for it.
    #[serde(default)]
    pub fallback_mode: HoldFallbackMode,
    /// Meaningful only when `fallback_mode` is `Alias`. Validated at save
    /// time and again at use (must resolve, must not be local, must not be
    /// another candidate alias — [`Snapshot::usable_fallback`]).
    #[serde(default)]
    pub fallback: Option<String>,
    /// The owner's explicit "turn this common facet off" list (§4.6): wire
    /// facet names ([`crate::candidates::Facet::as_str`]). Together with
    /// what every candidate currently supports this decides the *next*
    /// save's [`Self::capabilities_enabled`] — see that field's doc comment
    /// for why the two are not the same thing.
    #[serde(default)]
    pub capabilities_disabled: Vec<String>,
    /// The enabled facet set **as computed at the last save** (§12 entry 49):
    /// every facet every candidate supported at that time, minus
    /// `capabilities_disabled`. Recomputed at every save — never at read
    /// time — because a candidate row edited afterwards to drop a facet must
    /// not silently shrink the published contract (§4.6: "the contract never
    /// shrinks silently"); instead that candidate leaves `routable` and the
    /// alias reports a problem, until the owner resolves it explicitly by
    /// saving the alias again. Routing, `/v1/models` and `lmgw__models` all
    /// read this stored set, never a live recomputation.
    #[serde(default)]
    pub capabilities_enabled: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub notes: String,
}

impl CandidateAlias {
    /// `candidates[0]` — the only entry the alias itself ever starts (§4.1:
    /// "the alias only ever loads the primary"). `None` only for a row that
    /// somehow saved with an empty list, which `ops::candidate_alias` refuses
    /// at write time.
    pub fn primary(&self) -> Option<&str> {
        self.candidates.first().map(String::as_str)
    }
}

/// Result of routing an alias (§4): which upstream, which concrete model,
/// and the merged params.
#[derive(Debug, Clone)]
pub struct Route {
    pub upstream: Upstream,
    pub upstream_model: String,
    /// Alias-level parameter defaults (client params win on conflict).
    pub param_defaults: Params,
    /// The fallback alias this route stands in as, when it is a configured
    /// fallback answering for a local model (the GPU hold's or a benchmark
    /// lease's swap, §4.7's outside-VRAM swap, a ladder climb's, a candidate
    /// alias's): its name, as `x-lmgw-fallback` says it. Set where every
    /// fallback's route is born ([`Snapshot::usable_fallback`]), so every
    /// send on it knows, in-process turns included — what
    /// `gate::fallback_images` reads. `None` on a route the client named.
    /// Runtime only.
    pub fallback: Option<std::sync::Arc<str>>,
}

impl Route {
    /// The alias whose price rows apply to a call on this route: the fallback
    /// that answered, when this route stands in for one, else `requested`
    /// (billable-units design §12 Q2). Its alias-scoped rows, manual or
    /// catalog, are that alias's, and the upstream and model beside it are
    /// already the fallback's. Only the price lookup reads it: the row's
    /// `requested_alias` stays the name the client asked for.
    pub fn priced_alias<'a>(&'a self, requested: &'a str) -> &'a str {
        self.fallback.as_deref().unwrap_or(requested)
    }
}

/// A routed request's destination, and how it got there — the answer of
/// [`Snapshot::resolve_for_request`] (gpu-hold design §4).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub route: Route,
    /// `Some(alias)` when the GPU hold re-routed a local model to this
    /// fallback. Carried out to every HTTP handler, which stamps it on the
    /// response as `x-lmgw-fallback` — without it a client sees a normal 200
    /// and has no way to know it was answered by a different model than the
    /// one it asked for (the body's `model` stays the requested alias, §2).
    pub fallback: Option<String>,
}

/// A local model's fallback — [`Snapshot::fallback_route`]'s answer.
// Built once per lookup and matched at once; boxing the route would buy
// nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum FallbackRoute {
    /// None configured: the row's mode is `none`, an `alias` row names
    /// nothing, a non-chat row inherits (only chat inherits the global), no
    /// global is set, or no row describes the model.
    None,
    /// Configured, resolves, and is not itself local: it can answer.
    Usable { alias: String, route: Route },
    /// Configured, and it cannot answer: `why` is the clause that says so
    /// ("does not resolve", "is itself a local model").
    Unusable { alias: String, why: &'static str },
}

/// Sentinel id of the synthetic chat upstream (local chat models route here
/// without any user-created upstream row).
///
/// The three sentinels below are negative because SQLite's `INTEGER PRIMARY
/// KEY` only ever hands out positive ids, so no stored upstream can collide
/// with one — which is what lets [`vram::classify`](crate::vram::classify) and
/// [`Snapshot::aux_model_for`] key on the id instead of on a name an owner is
/// free to reuse.
pub const ROUTER_UPSTREAM_ID: i64 = -1;

/// Sentinel id of the synthetic aux upstream (embedders + rerankers).
pub const AUX_UPSTREAM_ID: i64 = -2;

/// Sentinel id of the synthetic audio upstream (audio.cpp models).
pub const AUDIO_UPSTREAM_ID: i64 = -3;

/// Sentinel id of the synthetic image upstream (stable-diffusion.cpp models).
pub const IMAGE_UPSTREAM_ID: i64 = -4;

/// Name of the synthetic chat upstream.
///
/// The three names below were the `upstreams.name` values of the managed rows
/// per-model containers §5 deleted. They are kept verbatim as the synthetic
/// upstreams' names because they are pinned data: a quickdoc corpus records
/// the *resolved* identity it was embedded with (`embed_upstream =
/// "llama-aux"`), so changing a spelling here would orphan every corpus on
/// disk. Zero data migration is the point.
pub const ROUTER_UPSTREAM_NAME: &str = "llama-server";

/// Name of the synthetic aux upstream — see [`ROUTER_UPSTREAM_NAME`].
pub const AUX_UPSTREAM_NAME: &str = "llama-aux";

/// Name of the synthetic audio upstream — see [`ROUTER_UPSTREAM_NAME`].
pub const AUDIO_UPSTREAM_NAME: &str = "audiocpp";

/// Name of the synthetic image upstream — see [`ROUTER_UPSTREAM_NAME`].
///
/// The only one of the four that was never a stored row: the image class
/// arrived after §5 made every managed upstream synthetic, so this name has
/// nothing pinned to it yet and is simply the engine's own word.
pub const IMAGE_UPSTREAM_NAME: &str = "sdcpp";

/// The `base_url` every synthetic upstream carries until it is overwritten.
///
/// With one container per model (§3.2) there is no class-wide port left to
/// derive a URL from: a local route is only forwardable while a caller holds
/// the acquire guard [`vram::admit`](crate::vram::admit) hands back, and the
/// caller overwrites `route.upstream.base_url` with that hold's endpoint (§5).
/// So this is deliberately unconnectable — port 0 is not a listenable port —
/// rather than a plausible-looking default: any request that actually reaches
/// it is the §5 bug (a local route forwarded without a hold), and it fails
/// immediately and visibly instead of landing on whatever a dev box happens to
/// be running.
pub(super) const UNHELD_BASE_URL: &str = "http://127.0.0.1:0/v1";

/// A model name served by `GET /v1/models`, with its provenance.
#[derive(Debug, Clone)]
pub struct ExposedModel {
    pub name: String,
    /// `"alias"` | `"local"` (chat) | `"aux"` | `"audio"` | `"image"` —
    /// passthrough catalogs are appended by the handler (they need upstream
    /// HTTP calls).
    pub source: &'static str,
}
