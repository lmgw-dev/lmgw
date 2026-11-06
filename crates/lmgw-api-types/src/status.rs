//! Status / overview (mirrors of `ops::status` + telemetry types; the server
//! serializes its own structs, these must stay field-compatible)

use serde::{Deserialize, Serialize};

/// Uniform error body for every non-2xx `/api` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiError {
    /// Stable machine-readable code, e.g. `not_found`, `invalid_input`.
    pub code: String,
    /// Human-readable description, shown verbatim in the UI.
    pub message: String,
    /// Who holds the Chat thread a realtime bind asked for, named as the
    /// Chat feed's `voice.bound.by` names a client ("device 'desktop'", "the
    /// dashboard"): what `lmgw-client`'s `read_refusal` reads from the
    /// realtime handshake's `error.held_by` ([`OpenAiErrorBody::held_by`])
    /// on its 409 `chat_thread_bound`. No `/api` route sends it, so the
    /// dashboard plane's schema leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub held_by: Option<String>,
}

/// The OpenAI-shaped error envelope `/v1` answers failures in (and the lab
/// routes that relay or imitate it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenAiError {
    pub error: OpenAiErrorBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenAiErrorBody {
    pub message: String,
    /// e.g. `invalid_request_error`, `api_error`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Always null: lmgw does not attribute an error to one request field.
    pub param: Option<String>,
    /// lmgw's own machine-readable code, not the HTTP status.
    pub code: String,
    /// Who holds the Chat thread a realtime bind asked for, named as the
    /// Chat feed's `voice.bound.by` names a client ("device 'desktop'",
    /// "the dashboard"): on the realtime handshake's 409
    /// `chat_thread_bound` only, and absent everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
}

impl OpenAiError {
    pub fn new(message: impl Into<String>, kind: &str, code: &str) -> Self {
        Self {
            error: OpenAiErrorBody {
                message: message.into(),
                kind: kind.into(),
                param: None,
                code: code.into(),
                held_by: None,
            },
        }
    }
}

/// `GET /api/version` — build identity, also serves as the liveness probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VersionInfo {
    pub version: String,
}

/// `GET /api/status` and the `lmgw__status` tool — everything the Overview
/// needs in one call.
///
/// `R`, `V` and `M` are the shapes of a runtime row, of the GPU ledger and of
/// a southbound MCP server's connection: the gateway fills in its own views,
/// a reader (and the API document) uses [`RuntimeStatus`], [`VramStatus`] and
/// [`McpStatus`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "GatewayStatus"))]
pub struct GatewayStatus<R = RuntimeStatus, V = VramStatus, M = McpStatus> {
    pub version: String,
    pub uptime_seconds: u64,
    pub bind_addr: String,
    pub auth_enabled: bool,
    /// The gateway's self-admin level: what the `lmgw__*` tools may do.
    pub self_admin: crate::AdminLevel,
    pub requests: RequestStats,
    /// One row per model lmgw believes is running (a container is per model, not per class).
    #[serde(default)]
    pub runtime: Vec<R>,
    /// The GPU those containers share: what is resident, what it is measured
    /// to cost, and anything queued behind it. Reported even when admission
    /// is inactive; `inactive_reason` says why.
    pub vram: V,
    #[serde(default)]
    pub mcp_servers: Vec<M>,
    pub counts: ObjectCounts,
    /// One entry per candidate alias, by name. Absent while none is
    /// configured, so an install without any reads as it did before the key
    /// existed: no empty array, no key at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub candidate_aliases: Option<Vec<crate::CandidateAliasStatus>>,
    /// The device link: lmgw's device CA and the server certificates issued
    /// from it (device link design §9.2).
    #[serde(default)]
    pub devices: crate::device_link::DevicesStatus,
    /// The UDP listener for WebRTC calls: its socket groups, one per HTTP
    /// listener (realtime calls design §9).
    #[serde(default)]
    pub webrtc: crate::realtime::WebRtcStatus,
    /// The upstreams lmgw remembers out of quota (openai egress design
    /// §14.6); empty while none is.
    #[serde(default)]
    pub upstreams_out_of_quota: Vec<crate::UpstreamQuotaStatus>,
    /// The machine check's verdict (machine-check design §4.1): whether this
    /// computer can run local models. Absent until the check at boot has
    /// finished; `POST /api/op/machine_check` has the whole check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub machine: Option<crate::machine::MachineSummary>,
}

/// One model container's runtime state.
// Mirror of `runtime::registry::RuntimeView`.
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
    /// Non-fatal problems this model's start found.
    /// Empty for a clean one, and absent from a frame older than the class.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// What an image container reported about its loaded pipeline. `None` for
    /// every other class, and for an image container whose capabilities route
    /// answered anything but a 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub image_capabilities: Option<ImageCapabilities>,
    /// What a llama-server container said about itself in `GET /props`.
    /// `None` for every other engine, and when the
    /// read failed — `warnings` then says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub llama_props: Option<LlamaProps>,
    /// The rung this container runs. `None` for a row
    /// without a ladder, and `#[serde(default)]` so a frame from before this
    /// field existed still decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub rung: Option<RungStatus>,
    /// A climb in progress: to which rung, why, and what it is doing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub climbing: Option<ClimbStatus>,
    /// Sends in flight as the ladder gate counts them — what a climb drains.
    /// Only ladder sends are counted, so this is zero on every other row.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sends: u32,
    /// Whom the container runs for:
    /// `"background"` when a background candidate alias started it and
    /// foreground traffic has not claimed it since. `None` — absent from the
    /// frame — is foreground: every request that is not a background candidate
    /// alias's, every warm, boot or operator start, and lmgw's own in-process
    /// callers. That is every container of an install without background
    /// traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub owner: Option<String>,
    /// A foreground admission is waiting for room, and background traffic takes
    /// no new work on this model until it is over. Absent = false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draining_for_owner: bool,
    /// `"cpu"` for an audio container started on the CPU (the per-row CPU
    /// switch): no VRAM, not stopped by the GPU hold. `None` — absent from
    /// the frame — is the GPU, every other container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub placement: Option<String>,
}

/// The facts a llama.cpp server reports about its loaded model, read once per start,
/// climb and adoption. Every field is optional,
/// and `None` is unknown — the server did not say — never `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LlamaProps {
    /// `modalities.vision`: the server loaded a projector that sees.
    pub vision: Option<bool>,
    /// `modalities.audio`: the server loaded a projector that hears.
    pub audio: Option<bool>,
    /// `modalities.video` — official builds only.
    pub video: Option<bool>,
    /// `chat_template_caps`, the template's own booleans; empty on
    /// ik_llama.cpp, which sends none of them.
    pub caps: Option<std::collections::BTreeMap<String, bool>>,
    /// One slot's context in tokens (`default_generation_settings.n_ctx`).
    pub n_ctx_slot: Option<u64>,
    /// `b<number>-<commit>` on official builds; ik_llama.cpp sends none.
    pub build_info: Option<String>,
}

/// The rung a container runs, 1-based (the base is rung 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungStatus {
    pub rung: usize,
    pub of: usize,
    /// The weights' file name.
    pub gguf: String,
}

/// A climb in progress, and why — "climbing to 3/3 — prompt 41,210 + 8,192 >
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

/// One sd-server's own answer about the pipeline it loaded.
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
    /// The preview modes the build offers (`none`, `proj`, `tae`, `vae`);
    /// empty for a build without previews.
    pub preview_modes: Vec<String>,
    /// The build can upscale with a model under its upscalers directory.
    pub upscale: bool,
    /// The request defaults per mode, as the server serializes its own
    /// request struct (`img_gen` → `{width, height, sample_params, …}`).
    pub defaults_by_mode: serde_json::Map<String, serde_json::Value>,
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
    pub max_upscale_width: Option<i64>,
    pub max_upscale_height: Option<i64>,
}

/// Which optional inputs the loaded pipeline admits to accepting. Not an edit
/// gate — the row's `edit` column is.
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
    /// The job API draws previews of a running generation.
    pub preview: bool,
}

/// A named LoRA or upscaler. Objects (`{"name": "Latent"}`) and bare strings
/// both parse, same tolerance the server-side type has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImageAsset {
    pub name: String,
    /// An upscaler `POST /sdcpp/v1/upscale` can run (a model under the
    /// upscalers directory); `false` for the built-in resize modes.
    pub image_upscale: bool,
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
                #[serde(default)]
                image_upscale: bool,
            },
        }
        Ok(match Shape::deserialize(d)? {
            Shape::Name(name) => ImageAsset {
                name,
                image_upscale: false,
            },
            Shape::Object {
                name,
                image_upscale,
            } => ImageAsset {
                name,
                image_upscale,
            },
        })
    }
}

/// Hand-written rather than derived: the reader accepts a bare string as well
/// as `{name}` (the `Deserialize` impl above), but the gateway only ever
/// sends the object, and this schema describes what is sent.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for ImageAsset {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ImageAsset".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "image_upscale": { "type": "boolean" },
            },
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

/// `GET /api/vram` and the `vram` SSE frame — measured figures and
/// estimated ones are separate fields on
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
    /// What `free_bytes` was lowered by on top of the memory in use, by
    /// reason. With a measured free figure, `free_bytes` is the capacity's
    /// measured free memory — the devices' free memory, or with
    /// `vram.budget_mb` the budget less the devices' used memory — less
    /// these (never below zero). The devices' total less their used and free
    /// memory is in neither: memory the probe reports as not available
    /// (NVML: the driver's own reserve; an AMD APU: GTT the host's RAM
    /// cannot back).
    pub kept_free: VramKeptFree,
    pub estimated_resident_bytes: u64,
    pub devices: Vec<VramDevice>,
    pub resident: Vec<VramResident>,
    pub queue: Vec<VramWaiter>,
    /// Starts whose model's engine image is still downloading (machine-check
    /// design §5), oldest first: each `phase` is `downloading` with the bytes
    /// so far. They hold no place in `queue` and nothing on the card; once
    /// its download is done a start is admitted like any other, and enters
    /// `queue` if it has to wait for room. Absent from an older gateway's
    /// frame.
    pub downloading: Vec<VramWaiter>,
    pub note: String,
    /// The `hold.active` setting — hold is a switch on
    /// the resolve/start paths, orthogonal to `enabled`/`active` above.
    pub hold_active: bool,
    /// Resident model ids that are busy or still starting while hold is
    /// active — visible continuously, not only in the one-shot `hold_set`
    /// response. Empty whenever hold is not active.
    pub draining: Vec<String>,
    /// The token ledger of every guarded unified-KV chat model with a request
    /// in flight or waiting — `vram::VramView`'s
    /// `kv_pools`. Absent from an older gateway's frame, hence the default.
    pub kv_pools: Vec<VramKvPool>,
    /// lmgw's own share of the pooled devices' used memory, measured per
    /// process. `None` when it
    /// cannot be measured — see `external_trigger_reason`.
    pub lmgw_share_bytes: Option<u64>,
    /// Used memory that is not lmgw's — games, the desktop, other apps.
    /// `None` exactly when `lmgw_share_bytes` is.
    pub outside_share_bytes: Option<u64>,
    /// Whether the outside-VRAM fallback can fire right now.
    pub external_trigger_active: bool,
    /// Why it cannot, when it cannot (the switch, admission, the hold, no
    /// telemetry, an unattributable lmgw model).
    pub external_trigger_reason: Option<String>,
    /// The benchmark run holding the card, if one does: no local model is admitted until it ends.
    pub benchmark: Option<VramBenchmark>,
}

/// What admission keeps free on top of the memory in use: memory the driver
/// cannot show yet. Each part is zero when nothing of that kind is kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramKeptFree {
    /// Containers still starting, and starts admitted whose container is not
    /// up yet: weights on their way onto the card.
    pub starting_bytes: u64,
    /// What resident image pipelines take above their idle residency while
    /// they draw (the residents' `peak_extra_bytes`).
    pub image_peak_bytes: u64,
    /// What ready audio containers will still load on their first request
    /// (the residents' `pending_bytes`).
    pub pending_load_bytes: u64,
}

/// A benchmark run's GPU lease.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramBenchmark {
    pub run_id: i64,
    pub model_id: String,
}

/// One guarded model's shared KV pool.
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

/// One request waiting for room in a pool.
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
    /// a start the scheduler has admitted whose container is not up yet.
    pub state: String,
    pub estimated_bytes: u64,
    pub in_flight: usize,
    pub idle_seconds: Option<u64>,
    /// What one generation of a resident image pipeline has been measured to
    /// need *above* its idle residency, and what admission keeps free for it. `None` for every
    /// other class, and for an image
    /// model that has not generated anything yet — `note` says which.
    pub peak_extra_bytes: Option<u64>,
    /// What a `ready` audio container will still load on its first request,
    /// kept free by admission on top of the measured use: its expected
    /// residency less what its processes hold now, else less what it was
    /// read holding at rest, or all of it where neither could be read.
    /// `None` once loaded (lmgw routed a request that loaded it, or its
    /// processes hold at least that much now), and for every other class.
    pub pending_bytes: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VramWaiter {
    /// The 1-based place in the admission queue; 0 for an entry of
    /// `downloading`, which holds no place in it.
    pub position: usize,
    pub alias: String,
    pub model: String,
    pub container: String,
    /// Footprint plus headroom; 0 for an entry of `downloading`, whose
    /// footprint is read at admission, after the download.
    pub needs_bytes: u64,
    pub waiting_ms: u64,
    pub stage: String,
    /// `stage` typed: what admission is doing for this request now. Absent
    /// from an older gateway's frame.
    pub phase: Option<WaitPhase>,
}

/// What admission is doing for a request that waits for room on the GPU
/// (chat-media-tools design §5.3), told apart by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WaitPhase {
    /// The model's engine image is not on this machine yet and is being
    /// downloaded (machine-check design §5), before admission: nothing is
    /// evicted or reserved for it meanwhile, and the queue's time budget
    /// starts after it. `done_bytes` of `total_bytes` compressed layer bytes
    /// as Podman reports them; `total_bytes` is absent when the size is not
    /// known (no registry manifest, or Podman's API was not reachable and
    /// the `podman pull` command reports no byte counts — `done_bytes` then
    /// stays 0), and `size_note` then says why.
    Downloading {
        image: String,
        done_bytes: u64,
        total_bytes: Option<u64>,
        /// Why the size is not known, when it is not: the pull job's own
        /// "size not known — …" (`ImagePullJobDetail::size_note`). Absent
        /// while nothing has said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
        size_note: Option<String>,
    },
    /// Waiting for the admission gate: another request is being admitted.
    Gate,
    /// Measuring the GPU's memory.
    Measuring,
    /// Stopping an idle model to make room: `<class>/<model>`
    /// (`chat/qwen3-30b`).
    Evicting { victim: String },
    /// Nothing idle is left to stop: waiting for a busy model to finish, or
    /// for memory held outside lmgw.
    WaitingForRoom { needs_bytes: u64, free_bytes: u64 },
    /// Room is there: the container is starting.
    Starting,
}

/// One registered MCP server's connection state: `status` is `connecting` |
/// `ready` | `stopped` | `error`, `tool_count` the tools it offers, and
/// `detail` says why on an error, or that a stopped server was idle-reaped
/// and reconnects on next use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpStatus {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub tool_count: i64,
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

/// Aggregate request statistics — the `stats` SSE frame.
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

/// One request's summary — the `request` SSE frame and the
/// row shape of `GET /api/logs` (log rows additionally carry `mcp_tool` and
/// `pricing`).
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
    /// guarded unified-KV pool. `None` = not clamped — either the row is unguarded, or the
    /// client's value already fit.
    #[serde(default)]
    pub max_tokens_clamped: Option<i64>,
    /// Why a fallback answered instead of the requested local model: `hold`
    /// (the GPU hold), `external_vram` (VRAM outside lmgw's control was
    /// short), `background` (a background candidate alias), `unavailable`
    /// (a candidate alias with no model it may use), `benchmark`, `quota`,
    /// `unreachable`, `disabled`, `gpu_setup` (the graphics card is not
    /// ready on this computer) or `engine_download` (a local model's engine
    /// could not be downloaded). `None` = no fallback; the fallback itself
    /// is `upstream_name` / `upstream_model`.
    #[serde(default)]
    pub fallback_reason: Option<String>,
    /// The rung a ladder row answered from — or, for a refusal, the rung it
    /// was judged on — 1-based. `None` for a row without
    /// a ladder, and for a request its fallback answered.
    #[serde(default)]
    pub rung: Option<i64>,
    /// What the request's content lost on its way to a model that lacks a
    /// capability: "fallback 'x' lacks vision: 3 images sent as
    /// placeholders", "'m' lacks audio: transcript sent", several joined by
    /// "; ". `None` = nothing was degraded.
    #[serde(default)]
    pub degraded: Option<String>,
    /// What the request processed besides tokens, each only when it was
    /// measured or the provider reported it: input audio in milliseconds,
    /// input characters as sent, generated images. `None` = not measured,
    /// which is not the same as zero — render a dash.
    #[serde(default)]
    pub audio_in_ms: Option<i64>,
    #[serde(default)]
    pub chars_in: Option<i64>,
    #[serde(default)]
    pub images_out: Option<i64>,
    /// The `service_tier` an OpenAI platform answer reported, verbatim
    /// (`default`, `flex`, `priority`, …): the tier the row is priced at.
    /// `None` on every other upstream, and when the answer named none.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Why the row's cost is what it is, when that is not plain: "served at
    /// service tier flex; no flex price on file for 'm', a standard one is",
    /// "stream ended before the upstream reported usage". `None` otherwise.
    #[serde(default)]
    pub price_note: Option<String>,
    // Billable-units design §3.4, §8.4.
    /// How the row was priced: the rates it recorded when it was written,
    /// so each priced part can be shown as quantity × rate even after the
    /// price has changed. On `GET /api/logs` rows only; absent on SSE
    /// frames, which carry the cost and the quantities alone.
    #[serde(default)]
    pub pricing: Option<RowPricing>,
}

/// The rates a request row was priced with and its cost's parts, each as
/// the row snapshotted it. `None` = that part or rate did not apply, or
/// (for the costs) was unknown — never 0.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RowPricing {
    /// The gateway's configured currency, which every amount here is in.
    pub currency: String,
    /// `catalog`, `manual`, `free_local` or `unknown`.
    pub source: Option<String>,
    /// Whether this row is counted in the unpriced remainder the usage
    /// totals state beside their cost (`cost_unknown_requests`): its cost is
    /// unknown, it is no tool call, and it was answered or spent something
    /// (tokens or a measured quantity). `false` for a priced row, a tool
    /// call, and a refusal or failure that spent nothing: nothing billed is
    /// missing from a total.
    pub in_remainder: bool,
    /// The token rates, per 1M tokens.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// The cost's token parts and its part in every other unit, in currency
    /// micro-units.
    pub cost_in_micro: Option<i64>,
    pub cost_out_micro: Option<i64>,
    pub cost_units_micro: Option<i64>,
    /// The other units' rates: per minute of input audio, per 1M input
    /// characters, per generated image, per answered request.
    pub price_per_audio_minute: Option<f64>,
    pub price_per_mchar: Option<f64>,
    pub price_per_image: Option<f64>,
    pub price_per_request: Option<f64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use serde_json::json;

    fn status(candidate_aliases: Option<Vec<crate::CandidateAliasStatus>>) -> GatewayStatus {
        GatewayStatus {
            version: "1".into(),
            uptime_seconds: 2,
            bind_addr: "127.0.0.1:8787".into(),
            auth_enabled: false,
            self_admin: crate::AdminLevel::ReadOnly,
            requests: RequestStats {
                total: 1,
                errors: 0,
                active: -1,
                last_minute: 1,
                errors_last_minute: 0,
                prompt_tokens: 3,
                completion_tokens: 4,
            },
            runtime: vec![],
            vram: VramStatus::default(),
            mcp_servers: vec![],
            counts: ObjectCounts {
                upstreams: 0,
                aliases: 0,
                local_models: 0,
                mcp_servers: 0,
                api_keys: 1,
            },
            candidate_aliases,
            devices: Default::default(),
            webrtc: Default::default(),
            upstreams_out_of_quota: vec![],
            machine: None,
        }
    }

    /// The key is absent while no candidate alias is configured, and an empty
    /// list is never sent in its place.
    #[test]
    fn candidate_aliases_are_absent_until_one_is_configured() {
        let none = serde_json::to_value(status(None)).unwrap();
        assert!(none.get("candidate_aliases").is_none(), "{none}");
        // The ledger and the lists are always there.
        assert_eq!(none["runtime"], json!([]));
        assert_eq!(none["mcp_servers"], json!([]));
        assert!(none["vram"].is_object());

        let one = serde_json::to_value(status(Some(vec![crate::CandidateAliasStatus {
            name: "job".into(),
            mode: "background".into(),
            primary: None,
            ..Default::default()
        }])))
        .unwrap();
        assert_eq!(
            one["candidate_aliases"],
            json!([{
                "name": "job",
                "mode": "background",
                "primary": null,
                "alternates": [],
                "enabled_facets": [],
                "problems": [],
                "deferrals_24h": 0,
            }])
        );
        let back: GatewayStatus = serde_json::from_value(one).unwrap();
        assert_eq!(back.candidate_aliases.map(|a| a.len()), Some(1));
    }
}
