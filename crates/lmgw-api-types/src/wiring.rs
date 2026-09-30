//! Wiring — the signal path per model

use super::*;
use serde::{Deserialize, Serialize};

/// `GET /api/wiring` — every link of the chain
/// (source → GGUF on disk → exposure → llama-server), joined
/// server-side. The client only picks colors and fix-links from these facts;
/// it never cross-references the models/aliases/downloads lists itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WiringView {
    pub locals: Vec<LocalChain>,
    /// GGUFs on disk no local model references (as weights, projector or
    /// drafter).
    pub orphans: Vec<OrphanGguf>,
    /// Remote upstreams only (llama-server-kind ones are the local chains).
    pub upstreams: Vec<UpstreamChain>,
    /// Chat models dir, empty when unset — then nothing can be scanned.
    pub models_dir: String,
    pub models_dir_missing: bool,
    /// Gateway base URL without `/v1`.
    pub base_url: String,
}

/// One local (chat) model's full path from download to container.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalChain {
    pub id: i64,
    pub model_id: String,
    /// `owner/repo` when the GGUF is a tracked HF download, else empty.
    pub hf_repo: String,
    /// HF row status (`done | queued | downloading | failed |
    /// update_available`), empty when the file was not downloaded by lmgw.
    pub hf_status: String,
    pub gguf_path: String,
    /// The GGUF was found by the models-dir scan.
    pub file_exists: bool,
    pub enabled: bool,
    pub public: bool,
    /// Client-facing name (prefix applied), meaningful only when `public`.
    pub public_name: String,
    /// Enabled explicit aliases routing here through a llama-server upstream.
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OrphanGguf {
    pub gguf_path: String,
    /// Model id the create flow proposes for it.
    pub suggested_id: String,
    /// `weights | mmproj | drafter | imatrix`, guessed from the filename.
    /// Only weights can be wired up as a model; the rest are companions a
    /// model's editor attaches (or leftovers). Empty from an older gateway.
    pub role_guess: String,
}

/// One remote upstream's exposure path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpstreamChain {
    pub id: i64,
    pub name: String,
    /// `openai | anthropic | gemini`.
    pub protocol: String,
    pub enabled: bool,
    /// Whole catalog passed through under `prefix`.
    pub expose_all: bool,
    pub prefix: String,
    /// Enabled explicit aliases pointing at this upstream.
    pub aliases: Vec<String>,
}

/// `GET /api/local-model?id=|model_id=` — full stored record plus derived
/// health for the editor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalModelDetail {
    pub id: i64,
    pub model_id: String,
    pub gguf_path: String,
    pub gguf_present: bool,
    /// `hf | manual`.
    pub source: String,
    pub mmproj_present: Option<bool>,
    pub draft_present: Option<bool>,
    pub params: LlamaParams,
    /// One llama-server option per line (round-trips with the patch field).
    pub extra_args: String,
    pub idle_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    /// Per-model container image override (per-model-containers design §3.1);
    /// `None` inherits the chat class settings' image.
    pub image: Option<String>,
    /// Per-model `podman run` args override (§3.1), one per line — same
    /// round-trip convention as `extra_args`; `None` inherits the chat class
    /// settings' `extra_run_args`.
    pub extra_run_args: Option<String>,
    /// Start this model's own container at app launch (§3.1, §3.4).
    pub warm_start: bool,
    /// GPU-hold fallback mode and alias (gpu-hold design §2/§3.2); mirror of
    /// `ops::local_model_get`'s `hold_fallback_mode`/`hold_fallback` fields.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object with optional keys
    /// `capabilities`, `max_output_tokens`, `notes`. `None` = no override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities_override: Option<serde_json::Value>,
    /// Ladder rungs above the base (ladder design §4.1); empty = not a
    /// ladder. Mirror of `config::LocalModel::ladder`, added to this view by
    /// `ops::local_model_get` (WP5 — the editor's ladder table reads it).
    #[serde(default)]
    pub ladder: Vec<Rung>,
    /// The exact `podman run …` command line this model renders to (§3.6);
    /// the published host port is a placeholder, since it is allocated per
    /// start (§3.5).
    pub command_line: String,
    /// Static findings; empty means "nothing obviously wrong", not "loads".
    pub problems: Vec<String>,
}

/// `GET /api/gguf-files` — GGUFs under the chat models dir.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GgufFiles {
    pub models_dir: String,
    pub files: Vec<GgufFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GgufFile {
    /// Relative to models_dir — exactly what the path fields take.
    pub path: String,
    pub size_bytes: u64,
    pub size: String,
    pub used_by: Vec<String>,
    /// `weights | mmproj | drafter` (filename guess).
    pub role_guess: String,
}

/// `GET /api/local-model-plan?path=` — ready-to-apply parameter set derived
/// from GGUF metadata. `params` stays dynamic (it is LocalModelPatch-shaped
/// and merges straight into a `local_model_set` create call).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanResult {
    pub model_id: String,
    pub params: serde_json::Map<String, serde_json::Value>,
    pub rationale: serde_json::Map<String, serde_json::Value>,
    pub warnings: Vec<String>,
    pub configured_as: Vec<serde_json::Value>,
    pub next_step: String,
}

/// `GET /api/ladder-rung-plan` — one ladder rung's footprint, MTP flag and
/// trained context (ladder design §4.2, §6): what the editor's rung table
/// cannot compute itself. Per-slot context and switchover are plain
/// arithmetic the UI does over fields it already holds, capped at
/// `trained_context` the way llama-server caps every slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungPlan {
    pub footprint: RungFootprint,
    /// Whether the GGUF header carries MTP tensors (§4.3 rule 6).
    pub has_mtp_layers: bool,
    /// The weights' trained context (`<arch>.context_length`): llama-server
    /// caps every slot there, and §4.3 refuses a rung whose per-slot context
    /// is above it. `None` when the header does not say.
    #[serde(default)]
    pub trained_context: Option<u64>,
}

/// Mirror of `vram::plan::Footprint` — a model's estimated GPU cost, a
/// documented lower bound (compute/graph buffers and allocator slack are not
/// in it; see that module's docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungFootprint {
    pub weights_bytes: u64,
    pub kv_cache_bytes: u64,
    /// `weights_bytes + kv_cache_bytes`.
    pub total_bytes: u64,
    /// Context the KV figure was computed for; `None` when it could not be
    /// derived (also why `kv_cache_bytes` would be 0).
    pub ctx_tokens: Option<u64>,
    /// Why a term is missing or approximate, when it is.
    pub note: Option<String>,
}
