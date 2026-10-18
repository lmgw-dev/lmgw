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
    /// `<org>/<repo>` when the GGUF is a tracked HF download, else empty.
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
    /// The licence the GGUF header states; null when it states none.
    pub licence: Option<crate::ModelLicence>,
    /// `hf | manual`.
    pub source: String,
    pub mmproj_present: Option<bool>,
    pub draft_present: Option<bool>,
    pub params: LlamaParams,
    /// One entry per llama-server argument (round-trips with the patch field).
    pub extra_args: Vec<String>,
    pub idle_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    /// Per-model container image override;
    /// `None` inherits the chat class settings' image.
    pub image: Option<String>,
    /// Per-model `podman run` args override, one entry per argument (round-trips
    /// with the patch field); `None` inherits the chat class settings'
    /// `extra_run_args`.
    pub extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch.
    pub warm_start: bool,
    /// GPU-hold fallback mode (`inherit` | `none` | `alias`) and alias.
    #[serde(default = "default_hold_fallback_mode")]
    pub hold_fallback_mode: String,
    #[serde(default)]
    pub hold_fallback: Option<String>,
    /// An alias's capabilities override for the derived `/v1/models` capability facts: a JSON
    /// object with optional keys `capabilities`, `max_output_tokens`, `notes`. `None` = no
    /// override.
    #[serde(default)]
    pub capabilities_override: Option<serde_json::Value>,
    /// Ladder rungs above the base; empty = not a ladder. The editor's ladder table reads
    /// it.
    #[serde(default)]
    pub ladder: Vec<Rung>,
    /// The exact `podman run …` command line this model renders to;
    /// the published host port is a placeholder, since it is allocated per
    /// start.
    pub command_line: String,
    /// Every rung's own command line, per-slot context and switchover;
    /// `null` on a row that is not a ladder.
    #[serde(default)]
    pub rungs: Option<Vec<RungDetail>>,
    /// Static findings; empty means "nothing obviously wrong", not "loads".
    pub problems: Vec<String>,
    /// Configuration that loads and serves but misbehaves under some input
    /// (a large image, say). Apart from `problems` because the model starts.
    #[serde(default)]
    pub advisories: Vec<String>,
    /// The download this file came from, when the downloader fetched it.
    #[serde(default)]
    pub downloaded_from: Option<DownloadedFrom>,
}

/// Where a downloaded weights file came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DownloadedFrom {
    /// The Hugging Face repository.
    pub repo: String,
    /// The file within it.
    pub file: String,
    /// The download's id.
    pub download_id: i64,
}

/// One ladder rung as the model read shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungDetail {
    /// This rung's number, from 1 (the base row).
    pub rung: usize,
    /// How many rungs the ladder has.
    pub of: usize,
    pub gguf_path: String,
    pub ctx_size: i64,
    /// The context one slot gets on this rung; null when the base row
    /// predates the rule that derives it.
    pub per_slot_ctx: Option<i64>,
    /// The prompt length at which a request moves up to the next rung.
    pub switchover: Option<i64>,
    /// The `podman run …` command line this rung renders to.
    pub command_line: String,
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
    /// The rows of the planned class already serving this GGUF, each with how
    /// it differs from the plan. A row's comparison is the plan's own: it
    /// knows what the plan's raw `params` do not (a tuning the row may take
    /// further, a projector its args already name, placement left to the
    /// owner), so an editor refills a saved row from `change`, not `params`.
    pub configured_as: Vec<ConfiguredAs>,
    pub next_step: String,
}

/// A configured row already serving the planned GGUF, and how it differs
/// from the plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConfiguredAs {
    pub model_id: String,
    /// The row's id in its class's table.
    pub id: i64,
    pub enabled: bool,
    /// Nothing the plan proposes differs from the row.
    pub in_sync: bool,
    /// Each field the plan would change on the row.
    pub change: Vec<PlanChange>,
    /// Chat rows: settings the row's files cannot support, which `apply`
    /// clears. Absent on aux rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub unset: Option<Vec<PlanUnset>>,
    /// The patch for the class's set op that brings the row in line with the
    /// plan; absent when it is in sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub apply: Option<serde_json::Map<String, serde_json::Value>>,
}

/// One field a plan would change on a configured row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanChange {
    pub field: String,
    /// The row's value now; null when unset.
    pub current: serde_json::Value,
    pub planned: serde_json::Value,
    /// The plan's reason for the planned value; null when it gives none.
    pub why: Option<String>,
}

/// One setting a plan would clear on a configured chat row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanUnset {
    pub field: String,
    pub current: serde_json::Value,
    pub why: String,
}

/// `GET /api/ladder-rung-plan` — one ladder rung's footprint, MTP flag and
/// trained context: what the editor's rung table
/// cannot compute itself. Per-slot context and switchover are plain
/// arithmetic the UI does over fields it already holds, capped at
/// `trained_context` the way llama-server caps every slot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungPlan {
    pub footprint: RungFootprint,
    /// Whether the GGUF header carries MTP tensors.
    pub has_mtp_layers: bool,
    /// The weights' trained context (`<arch>.context_length`): llama-server
    /// caps every slot there, and a rung whose per-slot context
    /// is above it is refused. `None` when the header does not say.
    pub trained_context: Option<u64>,
}

/// A model's estimated GPU cost. The CUDA context, allocator slack and a
/// projector's compute buffer are not in it; those are the headroom's.
// Mirror of `vram::plan::Footprint`; see that module's docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RungFootprint {
    pub weights_bytes: u64,
    /// The KV cache, a speculative row's MTP or draft cache and recurrent
    /// state included.
    pub kv_cache_bytes: u64,
    /// The compute buffers llama.cpp reserves on the card, derived from the
    /// header and the row's slots, batch sizes and context: for a chat row
    /// (a ladder rung) its own context's and its MTP or draft context's,
    /// summed.
    pub compute_bytes: u64,
    /// `weights_bytes + kv_cache_bytes + compute_bytes`.
    pub total_bytes: u64,
    /// Context the KV figure was computed for; `None` when it could not be
    /// derived (also why `kv_cache_bytes` would be 0).
    pub ctx_tokens: Option<u64>,
    /// Why a term is missing or approximate, when it is.
    pub note: Option<String>,
}
