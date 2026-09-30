//! Models domain (mirrors of config.rs types; keep field-compatible — the
//! server serializes its own structs)

use super::*;
use serde::{Deserialize, Serialize};

/// `GET /api/models/full` — every configured model, all five kinds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelsFull {
    pub aliases: Vec<AliasView>,
    /// Candidate aliases (candidate-aliases design §4.1). Defaulted, like
    /// `image`, so a UI bundle newer than the gateway it is talking to still
    /// parses the payload.
    #[serde(default)]
    pub candidate_aliases: Vec<CandidateAliasView>,
    pub local: Vec<LocalModelView>,
    pub aux: Vec<AuxModelView>,
    pub audio: Vec<AudioModelView>,
    /// The stable-diffusion.cpp class (image-generation design §4). Defaulted
    /// rather than required, so a UI bundle newer than the gateway it is
    /// talking to still parses the payload.
    #[serde(default)]
    pub image: Vec<ImageModelView>,
    /// Enabled `expose_all` upstreams: (upstream name, request prefix).
    #[serde(default)]
    pub passthrough: Vec<PassthroughUpstream>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PassthroughUpstream {
    pub upstream: String,
    pub id: i64,
    pub prefix: String,
    /// Bare (unprefixed) model ids hidden from `/v1/models` and the client
    /// catalog for this upstream — still reachable directly by name, just
    /// not advertised. `POST /api/op/model_visibility` toggles membership.
    #[serde(default)]
    pub hidden: Vec<String>,
}

/// Mirror of `config::ModelAlias` + resolved upstream name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AliasView {
    pub id: i64,
    pub alias: String,
    pub upstream_id: i64,
    #[serde(default)]
    pub upstream_name: Option<String>,
    pub upstream_model_id: String,
    #[serde(default)]
    pub param_overrides: Params,
    pub enabled: bool,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object with optional keys
    /// `capabilities`, `max_output_tokens`, `notes`. `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
}

/// Mirror of `ir::ReasoningControl` (per-request reasoning control: on/off,
/// effort level, thinking-token budget).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReasoningControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i64>,
}

/// Mirror of `ir::Params` (alias-level request overrides).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Params {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningControl>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

/// Mirror of `config::LocalModel` plus its client-facing public name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalModelView {
    pub public_name: String,
    pub model: LocalModel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalModel {
    pub id: i64,
    pub model_id: String,
    /// Relative to the models dir — never an in-container `/models/…` path.
    pub gguf_path: String,
    #[serde(default)]
    pub params: LlamaParams,
    #[serde(default)]
    pub args: Vec<String>,
    pub idle_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    /// Per-model container image override (per-model-containers design §3.1);
    /// `None` inherits the chat class settings' image.
    #[serde(default)]
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits the chat
    /// class settings' `extra_run_args`.
    #[serde(default)]
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    #[serde(default)]
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2): `inherit` | `none` |
    /// `alias`. Mirror of `config::HoldFallbackMode::as_str()`.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    /// The alias to route to while the GPU is held, meaningful only when
    /// `hold_fallback_mode` is `alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object with optional keys
    /// `capabilities`, `max_output_tokens`, `notes`. `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
    /// Ladder rungs above the base (ladder design §4.1). Mirror of
    /// `config::LocalModel::ladder`; empty = not a ladder. `#[serde(default)]`
    /// so a frame from before this field existed still decodes.
    #[serde(default)]
    pub ladder: Vec<Rung>,
}

/// Mirror of lmgw-core's `ladder::Rung` (ladder design §4.1): one rung above
/// the base — its own weights and its own context. The base rung is never
/// one of these; it stays `LocalModel::gguf_path` / `LlamaParams::ctx_size`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Rung {
    pub gguf_path: String,
    pub ctx_size: i64,
}

/// `config::HoldFallbackMode`'s `Default` (`Inherit`), as the wire spelling.
/// `pub(crate)`: `wiring.rs` names it in a `#[serde(default = "…")]` too.
pub(crate) fn default_hold_fallback_mode() -> String {
    "inherit".into()
}

/// Mirror of `config::LlamaParams` — the full llama-server param surface.
/// `None`/`false` = flag omitted from the rendered preset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LlamaParams {
    pub ctx_size: Option<i64>,
    /// `--n-predict`: the most tokens one response may generate. `None` =
    /// llama-server's own default (-1, unbounded). This is the number
    /// `/v1/models` reports as `max_output_tokens`.
    pub n_predict: Option<i64>,
    pub n_gpu_layers: Option<i64>,
    pub threads: Option<i64>,
    pub batch_size: Option<i64>,
    pub ubatch_size: Option<i64>,
    pub parallel: Option<i64>,
    /// `--kv-unified` (true) / `--no-kv-unified` (false); `None` leaves
    /// llama-server's default (unified exactly when `parallel` is unset).
    pub kv_unified: Option<bool>,
    /// `--kv-unified-per-slot`: per-request context cap on a unified row.
    pub kv_unified_per_slot: Option<i64>,
    pub flash_attn: Option<String>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    pub cache_ram: Option<i64>,
    pub jinja: bool,
    pub chat_template_file: Option<String>,
    pub reasoning_format: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_budget: Option<i64>,
    pub reasoning_preserve: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub chat_template_kwargs: serde_json::Map<String, serde_json::Value>,
    pub temp: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub mmproj_path: Option<String>,
    pub no_mmproj: bool,
    pub draft_gguf_path: Option<String>,
    pub spec_type: Option<String>,
    pub spec_draft_n_max: Option<i64>,
    pub spec_draft_n_min: Option<i64>,
    pub spec_draft_ngl: Option<String>,
    pub fit: Option<String>,
    pub fit_ctx: Option<i64>,
}

fn default_aux_kind() -> String {
    "embed".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxModelView {
    pub public_name: String,
    pub model: AuxModel,
}

/// Mirror of `config::AuxModel` — the aux container's models, embedders and
/// rerankers alike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxModel {
    pub id: i64,
    pub model_id: String,
    pub gguf_path: String,
    /// `embed` | `rerank`.
    #[serde(default = "default_aux_kind")]
    pub kind: String,
    #[serde(default)]
    pub pooling: Option<String>,
    #[serde(default)]
    pub ctx_size: Option<i64>,
    #[serde(default)]
    pub args: Vec<String>,
    pub idle_seconds: i64,
    pub enabled: bool,
    /// Per-model container image override (per-model-containers design §3.1);
    /// `None` inherits the aux class settings' image.
    #[serde(default)]
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits the aux
    /// class settings' `extra_run_args`.
    #[serde(default)]
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    #[serde(default)]
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2): `inherit` | `none` |
    /// `alias`. `inherit` means "no fallback" for this class — aux models
    /// never inherit the global chat alias.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    /// The alias to route to while the GPU is held, meaningful only when
    /// `hold_fallback_mode` is `alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioModelView {
    pub public_name: String,
    pub model: AudioModel,
}

/// Mirror of `config::AudioModel` (JSON-map fields stay dynamic).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioModel {
    pub id: i64,
    pub model_id: String,
    pub family: String,
    pub path: String,
    pub task: String,
    pub mode: String,
    /// Per-model override of the class's `lazy_load`; `None` inherits.
    #[serde(default)]
    pub lazy: Option<bool>,
    /// Per-model `busy_timeout_ms` and the ceiling a request's own value is
    /// clamped to, in ms; `None` inherits the class.
    #[serde(default)]
    pub busy_timeout_ms: Option<i64>,
    #[serde(default)]
    pub load_options: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub session_options: serde_json::Map<String, serde_json::Value>,
    /// Request-option defaults applied to every call to this model.
    #[serde(default)]
    pub default_request_options: serde_json::Map<String, serde_json::Value>,
    /// A `<family>.json` spec (or a directory of them) under the audio models
    /// dir that replaces the container image's own catalog for this row.
    #[serde(default)]
    pub model_spec_override: Option<String>,
    /// Named config/weights asset ids, for a model directory holding several.
    #[serde(default)]
    pub config_id: Option<String>,
    #[serde(default)]
    pub weight_id: Option<String>,
    #[serde(default)]
    pub voice_presets: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub default_voice_preset: Option<serde_json::Value>,
    pub enabled: bool,
    /// Per-model container image override (per-model-containers design §3.1);
    /// `None` inherits the audio class settings' image.
    #[serde(default)]
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1); `None` inherits the audio
    /// class settings' `extra_run_args`.
    #[serde(default)]
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    #[serde(default)]
    pub warm_start: bool,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2): `inherit` | `none` |
    /// `alias`. `inherit` means "no fallback" for this class — audio models
    /// never inherit the global chat alias.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    /// The alias to route to while the GPU is held, meaningful only when
    /// `hold_fallback_mode` is `alias`.
    #[serde(default)]
    pub hold_fallback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageModelView {
    pub public_name: String,
    pub model: ImageModel,
}

/// Mirror of `config::ImageModel` — one stable-diffusion.cpp pipeline
/// (image-generation design §4).
///
/// The two JSON-map fields stay dynamic on purpose, exactly as the server
/// types them: sd.cpp adds a model family and its flag every few weeks, and
/// the keys are validated against the container image's own `--help` rather
/// than against any enum on either side of this wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageModel {
    pub id: i64,
    pub model_id: String,
    /// Flag key → path relative to the image models dir. Exactly one of
    /// `model` / `diffusion_model` is present on a valid row.
    #[serde(default)]
    pub files: serde_json::Map<String, serde_json::Value>,
    /// Runtime + default-generation flags, same key convention. A `true`
    /// renders as a bare switch.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
    /// `img_gen` / `vid_gen`; empty means the default (`img_gen`).
    #[serde(default)]
    pub modes: Vec<String>,
    /// The pipeline takes reference images and may serve `/v1/images/edits`.
    #[serde(default)]
    pub edit: bool,
    pub enabled: bool,
    /// Per-model container image override; `None` inherits the image class
    /// settings' image.
    #[serde(default)]
    pub image: Option<String>,
    /// Per-model `podman run` args override; `None` inherits the class's.
    #[serde(default)]
    pub extra_run_args: Option<Vec<String>>,
    #[serde(default)]
    pub warm_start: bool,
    /// Seconds idle before the container is stopped; `0` never reaps.
    #[serde(default)]
    pub idle_seconds: i64,
    /// `inherit` | `none` | `alias`. `inherit` means "no fallback" for this
    /// class — an image row never inherits the global chat alias.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
    /// The learned transient peak: what one generation was measured to need
    /// above this pipeline's idle residency (image-generation §9). Written by
    /// the gateway, not by the editor — a `files` or `args` change resets it
    /// to `None`, because a different pipeline has a different peak.
    #[serde(default)]
    pub peak_extra_bytes: Option<u64>,
    /// When that figure was observed; `None` whenever the peak is.
    #[serde(default)]
    pub peak_learned_at: Option<String>,
}

/// Result of a mutating `/api/op/{name}` call when it succeeds without a
/// domain-specific body: `{ ok, message?, id? }` — matches the ops plane's
/// habit of returning these keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpOutcome {
    pub ok: bool,
    pub message: Option<String>,
    pub id: Option<i64>,
}
