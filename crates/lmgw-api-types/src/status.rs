//! Status / overview (mirrors of `ops::status` + telemetry types; the server
//! serializes its own structs, these must stay field-compatible)

use serde::{Deserialize, Serialize};

/// Uniform error body for every non-2xx `/api` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiError {
    /// Stable machine-readable code, e.g. `not_found`, `invalid_input`.
    pub code: String,
    /// Human-readable description, shown verbatim in the UI.
    pub message: String,
}

/// `GET /api/version` — build identity, also serves as the liveness probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VersionInfo {
    pub version: String,
}

/// `GET /api/status` — everything the Overview needs in one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub bind_addr: String,
    pub auth_enabled: bool,
    pub self_admin: String,
    pub requests: RequestStats,
    /// One row per model lmgw believes is running (per-model-containers
    /// design §3.2/§8) — replaces the old fixed three-container `containers`
    /// list now that a "container" is a per-model, not per-class, concept.
    #[serde(default)]
    pub runtime: Vec<RuntimeStatus>,
    #[serde(default)]
    pub mcp_servers: Vec<McpStatus>,
    pub counts: ObjectCounts,
}

/// Mirror of `runtime::registry::RuntimeView` (per-model-containers §3.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeStatus {
    /// `chat | aux | audio | image`.
    pub class: String,
    pub model_id: String,
    pub container_name: String,
    /// `0` while the entry is `starting` — no port has been allocated yet.
    pub port: u16,
    /// `starting | ready | stopping`.
    pub state: String,
    pub in_flight: u32,
    pub started_at_age_seconds: u64,
    pub last_used_age_seconds: u64,
    /// Non-fatal problems this model's start found (image-generation §3).
    /// Empty for a clean one, and absent from a frame older than the class.
    pub warnings: Vec<String>,
    /// What an image container reported about its loaded pipeline. `None` for
    /// every other class, and for an image container whose capabilities route
    /// answered anything but a 200.
    pub image_capabilities: Option<ImageCapabilities>,
    /// The rung this container runs (ladder design §6). `None` for a row
    /// without a ladder, and `#[serde(default)]` so a frame from before this
    /// field existed still decodes.
    #[serde(default)]
    pub rung: Option<RungStatus>,
    /// A climb in progress: to which rung, why, and what it is doing now.
    #[serde(default)]
    pub climbing: Option<ClimbStatus>,
    /// Sends in flight as the ladder gate counts them — what a climb drains.
    /// Only ladder sends are counted, so this is zero on every other row.
    #[serde(default)]
    pub sends: u32,
    /// Whom the container runs for (candidate-aliases design §4.4):
    /// `"background"` when a background candidate alias started it and the
    /// owner has not claimed it since. `None` — absent from the frame — is
    /// the owner's, which is every container of an install without
    /// background traffic.
    #[serde(default)]
    pub owner: Option<String>,
    /// An owner admission is waiting for room, and background traffic takes
    /// no new work on this model until it is over (§4.5). Absent = false.
    #[serde(default)]
    pub draining_for_owner: bool,
}

/// Mirror of `runtime::registry::climb::RungStatus` (ladder design §6): the
/// rung a container runs, 1-based (the base is rung 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungStatus {
    pub rung: usize,
    pub of: usize,
    /// The weights' file name.
    pub gguf: String,
}

/// Mirror of `runtime::registry::climb::ClimbStatus` (ladder design §6): a
/// climb in progress, and why — "climbing to 3/3 — prompt 41,210 + 8,192 >
/// 30,000".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClimbStatus {
    /// The rung it climbs to, 1-based.
    pub to: usize,
    pub of: usize,
    /// Why — the numbers of the request that did not fit.
    pub reason: String,
    /// What it is doing right now, in words.
    pub stage: String,
    /// Since the mark, in whole seconds.
    pub seconds: u64,
}

/// Mirror of `runtime::image::ImageCapabilities` — one sd-server's own answer
/// about the pipeline it loaded (image-generation §3).
///
/// Every field defaults, because this is the one payload in the class that
/// comes from a moving container tag: a build that stops reporting a number
/// must leave the rest readable rather than fail the frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageCapabilities {
    /// `["img_gen"]`, `["img_gen","vid_gen"]` — what the loaded pipeline can
    /// actually do, as opposed to what the row claims.
    pub supported_modes: Vec<String>,
    pub current_mode: String,
    pub limits: ImageLimits,
    pub features: ImageFeatures,
    pub samplers: Vec<String>,
    pub schedulers: Vec<String>,
    pub output_formats: Vec<String>,
    pub loras: Vec<ImageAsset>,
    pub upscalers: Vec<ImageAsset>,
}

/// Bounds the server enforces on a request — **the server's real numbers**;
/// lmgw never substitutes one of its own, so a `None` is shown as unknown
/// rather than filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageLimits {
    pub min_width: Option<i64>,
    pub max_width: Option<i64>,
    pub min_height: Option<i64>,
    pub max_height: Option<i64>,
    pub max_batch_count: Option<i64>,
    pub max_queue_size: Option<i64>,
}

/// Which optional inputs the loaded pipeline admits to accepting. Not an edit
/// gate — the row's `edit` column is (design §12.8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageFeatures {
    pub init_image: bool,
    pub mask_image: bool,
    pub ref_images: bool,
    pub control_image: bool,
    pub ip_adapter_image: bool,
    pub lora: bool,
    pub hires: bool,
    pub vae_tiling: bool,
    pub cache: bool,
    pub cancel_queued: bool,
    pub cancel_generating: bool,
}

/// A named LoRA or upscaler. Objects (`{"name": "Latent"}`) and bare strings
/// both parse, same tolerance the server-side type has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImageAsset {
    pub name: String,
}

impl<'de> Deserialize<'de> for ImageAsset {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Name(String),
            Object {
                #[serde(default)]
                name: String,
            },
        }
        Ok(match Shape::deserialize(d)? {
            Shape::Name(name) | Shape::Object { name } => ImageAsset { name },
        })
    }
}

/// Hand-written rather than derived (api-docs design §3.1): the wire shape is
/// the `Deserialize` impl above's tolerance, a bare string *or* `{name}`,
/// which `#[derive(JsonSchema)]` on the struct's one field could not state —
/// it only ever sees the object half.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for ImageAsset {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ImageAsset".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                },
            ],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestStats {
    pub total: u64,
    pub errors: u64,
    pub active: i64,
    pub last_minute: u64,
    pub errors_last_minute: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// `GET /api/vram` and the `vram` SSE frame — mirror of `vram::VramView`
/// (quickdoc §9b). Measured figures and estimated ones are separate fields on
/// purpose: the UI must be able to say which is which.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramStatus {
    pub enabled: bool,
    /// Whether admission is actually arbitrating; false is explained by
    /// `inactive_reason`.
    pub active: bool,
    pub inactive_reason: Option<String>,
    /// Where the measured numbers come from, verbatim.
    pub telemetry: String,
    pub telemetry_ok: bool,
    pub headroom_bytes: u64,
    pub capacity_bytes: u64,
    pub free_bytes: Option<u64>,
    /// False when `free_bytes` was derived from the estimate rather than read
    /// from the driver.
    pub free_measured: bool,
    pub estimated_resident_bytes: u64,
    pub devices: Vec<VramDevice>,
    pub resident: Vec<VramResident>,
    pub queue: Vec<VramWaiter>,
    pub note: String,
    /// The `hold.active` setting (gpu-hold design §6) — hold is a switch on
    /// the resolve/start paths, orthogonal to `enabled`/`active` above.
    pub hold_active: bool,
    /// `settings.hold.fallback_alias`, for the titlebar pill and the
    /// settings page to show what a held chat request falls back to.
    pub hold_fallback_alias: Option<String>,
    /// Resident model ids that are busy or still starting while hold is
    /// active — visible continuously, not only in the one-shot `hold_set`
    /// response. Empty whenever hold is not active.
    pub draining: Vec<String>,
    /// The token ledger of every guarded unified-KV chat model with a request
    /// in flight or waiting (unified-KV design §3.3) — `vram::VramView`'s
    /// `kv_pools`. Absent from an older gateway's frame, hence the default.
    pub kv_pools: Vec<VramKvPool>,
    /// lmgw's own share of the pooled devices' used memory, measured per
    /// process (candidate-aliases design §4.7, §12.26). `None` when it
    /// cannot be measured — see `external_trigger_reason`.
    pub lmgw_share_bytes: Option<u64>,
    /// Used memory that is not lmgw's — games, the desktop, other apps.
    /// `None` exactly when `lmgw_share_bytes` is.
    pub outside_share_bytes: Option<u64>,
    /// Whether the outside-VRAM fallback (§4.7) can fire right now.
    pub external_trigger_active: bool,
    /// Why it cannot, when it cannot (the switch, admission, the hold, no
    /// telemetry, an unattributable lmgw model).
    pub external_trigger_reason: Option<String>,
    /// The benchmark run holding the card, if one does (benchmark design
    /// §3.2): no local model is admitted until it ends.
    pub benchmark: Option<VramBenchmark>,
}

/// A benchmark run's GPU lease — mirror of `vram::VramBenchmarkView`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramBenchmark {
    pub run_id: i64,
    pub model_id: String,
}

/// One guarded model's shared KV pool — mirror of `gate::pool::KvPoolView`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramKvPool {
    pub model: String,
    /// The pool's size in tokens, as the running container was started
    /// with it.
    pub capacity_tokens: u64,
    /// Σ (prompt + max output) of the requests in flight and releasing.
    pub reserved_tokens: u64,
    pub in_flight: usize,
    /// Requests that ended without a normal completion and keep their
    /// reservation until llama-server lets go of their slots.
    pub releasing: usize,
    /// Requests waiting for room, head first.
    pub queue: Vec<VramKvPoolWaiter>,
}

/// One request waiting for room in a pool — mirror of
/// `gate::pool::KvPoolWaiterView`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramKvPoolWaiter {
    pub position: usize,
    pub alias: String,
    pub needs_tokens: u64,
    pub waiting_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramDevice {
    pub index: u32,
    pub name: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramResident {
    /// `chat | aux | audio`.
    pub container: String,
    pub model: String,
    /// `starting | ready` as the runtime registry holds it, or `reserved` for
    /// a start the scheduler has admitted whose container is not up yet
    /// (per-model-containers §3.2/§4).
    pub state: String,
    pub estimated_bytes: u64,
    pub in_flight: usize,
    pub idle_seconds: Option<u64>,
    /// What one generation of a resident image pipeline has been measured to
    /// need *above* its idle residency, and what admission keeps free for it
    /// (image-generation §9). `None` for every other class, and for an image
    /// model that has not generated anything yet — `note` says which.
    pub peak_extra_bytes: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramWaiter {
    pub position: usize,
    pub alias: String,
    pub model: String,
    pub container: String,
    pub needs_bytes: u64,
    pub waiting_ms: u64,
    pub stage: String,
}

/// Mirror of `mcp::McpStatusView`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpStatus {
    pub id: i64,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub tool_count: Option<i64>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObjectCounts {
    pub upstreams: u64,
    pub aliases: u64,
    pub local_models: u64,
    pub mcp_servers: u64,
    pub api_keys: u64,
}

/// Mirror of `telemetry::StatsView` — the `stats` SSE frame.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StatsView {
    pub total_requests: u64,
    pub total_errors: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub req_last_minute: u64,
    pub err_last_minute: u64,
    pub active_requests: i64,
    /// Output tokens per second across every in-flight stream, over a short
    /// trailing window; `0.0` when nothing is streaming. Approximate for
    /// upstreams that pack several tokens into one SSE chunk — show it with a
    /// "~". `default` so an older gateway's frame still decodes.
    #[serde(default)]
    pub tok_s: f64,
}

/// `GET /api/logs` — newest first; `before_id` pages further back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LogsResponse {
    pub logs: Vec<RequestRow>,
}

/// Mirror of `telemetry::RequestSummary` — the `request` SSE frame and the
/// row shape of `GET /api/logs` (log rows additionally carry `mcp_tool`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestRow {
    pub log_id: i64,
    pub ts: String,
    /// Name of the gateway API key that made the call (never the key itself);
    /// absent when auth is disabled or the call was made in-process.
    #[serde(default)]
    pub client_key: Option<String>,
    pub ingress_proto: String,
    pub requested_alias: String,
    #[serde(default)]
    pub upstream_name: Option<String>,
    #[serde(default)]
    pub upstream_model: Option<String>,
    #[serde(default)]
    pub egress_proto: Option<String>,
    pub status: u16,
    #[serde(default)]
    pub ttfb_ms: Option<i64>,
    #[serde(default)]
    pub total_ms: Option<i64>,
    /// **Total** input for the call, cache included.
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub completion_tokens: Option<i64>,
    /// The cache subsets of `prompt_tokens`, when the provider reported them —
    /// without these a row that cost 8x its neighbour looks identical to it.
    /// `None` = the provider said nothing, which is not the same as zero.
    #[serde(default)]
    pub cached_in_tokens: Option<i64>,
    #[serde(default)]
    pub cache_write_tokens: Option<i64>,
    #[serde(default)]
    pub streamed: bool,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub error_msg: Option<String>,
    /// MCP tool name for `ingress_proto = "mcp"` rows; absent on SSE frames.
    #[serde(default)]
    pub mcp_tool: Option<String>,
    /// Currency micro-units. `None` = **unpriced**, which is not the same as
    /// free — render a dash, never a 0.
    #[serde(default)]
    pub cost_micro: Option<i64>,
    /// `chat` | `aux` | `audio` | `tool`.
    #[serde(default)]
    pub class: String,
    /// `api_keys.id` behind `client_key` — the id is what a key filter matches
    /// on, and a name match breaks the moment a key is renamed.
    #[serde(default)]
    pub key_id: Option<i64>,
    /// `max_tokens` lmgw lowered a client-set value to, on a ladder rung or a
    /// guarded unified-KV pool (ladder design §3.2, unified-KV design §3.3
    /// step 1). `None` = not clamped — either the row is unguarded, or the
    /// client's value already fit.
    #[serde(default)]
    pub max_tokens_clamped: Option<i64>,
    /// Why a fallback answered instead of the requested local model: `hold`
    /// (the GPU hold), `external_vram` (VRAM outside lmgw's control was
    /// short), `background` (a background candidate alias), `unavailable`
    /// (a candidate alias with no model it may use). `None` = no
    /// fallback; the fallback itself is `upstream_name` / `upstream_model`.
    #[serde(default)]
    pub fallback_reason: Option<String>,
    /// The rung a ladder row answered from — or, for a refusal, the rung it
    /// was judged on — 1-based (ladder design §6). `None` for a row without
    /// a ladder, and for a request its fallback answered.
    #[serde(default)]
    pub rung: Option<i64>,
}
