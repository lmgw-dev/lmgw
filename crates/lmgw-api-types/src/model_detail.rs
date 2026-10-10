//! `GET /api/local-model`: one local model's full read, by class. The
//! chat arm is [`LocalModelDetail`]; the other classes have their own
//! shape. The MCP tool `lmgw__local_model_get` answers the same types as
//! JSON text.

use serde::{Deserialize, Serialize};

use super::{DownloadedFrom, LearnedResidency, LocalModelDetail};

/// One local model, with `class` naming which arm it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
// One answer value built and serialised at once; boxing the chat arm would ripple
// through every reader for nothing.
#[allow(clippy::large_enum_variant)]
pub enum LocalModelRead {
    /// A llama-server chat model.
    Chat(LocalModelDetail),
    /// An embedding or rerank model.
    Aux(AuxModelDetail),
    /// A stable-diffusion.cpp pipeline.
    Image(ImageModelDetail),
    /// An audio.cpp model.
    Audio(AudioModelDetail),
}

/// An aux (embedding or rerank) model read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxModelDetail {
    pub id: i64,
    pub model_id: String,
    /// The name clients call it by, with the aux prefix.
    pub public_name: String,
    /// `embed` or `rerank`.
    pub kind: String,
    /// The route that serves it: `/v1/embeddings` or `/v1/rerank`.
    pub endpoint: String,
    pub pooling: Option<String>,
    pub ctx_size: Option<i64>,
    pub gguf_path: String,
    pub gguf_present: bool,
    /// `hf` or `manual`.
    pub source: String,
    pub downloaded_from: Option<DownloadedFrom>,
    /// One llama-server option per line.
    pub extra_args: String,
    pub idle_seconds: i64,
    pub enabled: bool,
    /// Container image override; null inherits the aux class's.
    pub image: Option<String>,
    /// `podman run` args override, one per line; null inherits.
    pub extra_run_args: Option<String>,
    pub warm_start: bool,
    /// `inherit`, `none` or `alias`.
    pub hold_fallback_mode: String,
    pub hold_fallback: Option<String>,
    /// The `podman run …` command line this model renders to.
    pub command_line: String,
    /// Static findings; empty is not proof it loads.
    pub problems: Vec<String>,
}

/// An image-generation pipeline read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageModelDetail {
    pub id: i64,
    pub model_id: String,
    /// The name clients call it by, with the image prefix.
    pub public_name: String,
    /// Flag key to path under the image models dir.
    pub files: serde_json::Map<String, serde_json::Value>,
    /// For each key of `files`, whether the file (or directory) exists.
    pub files_present: std::collections::BTreeMap<String, bool>,
    /// Runtime and default-generation flags.
    pub args: serde_json::Map<String, serde_json::Value>,
    pub modes: Vec<String>,
    pub edit: bool,
    /// The routes that serve it.
    pub endpoints: Vec<String>,
    pub enabled: bool,
    pub image: Option<String>,
    /// `podman run` args override, one per line; null inherits.
    pub extra_run_args: Option<String>,
    pub warm_start: bool,
    pub idle_seconds: i64,
    pub hold_fallback_mode: String,
    pub hold_fallback: Option<String>,
    pub capabilities_override: Option<serde_json::Value>,
    /// Bytes one generation was measured to need above idle residency;
    /// null until one has run.
    pub peak_extra_bytes: Option<u64>,
    pub peak_learned_at: Option<String>,
    /// `peak_extra_bytes` in a sentence.
    pub peak: String,
    pub command_line: String,
    pub problems: Vec<String>,
}

/// An audio.cpp model read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioModelDetail {
    pub id: i64,
    pub model_id: String,
    /// The name clients call it by, with the audio prefix.
    pub public_name: String,
    /// The audio.cpp model-loader family.
    pub family: String,
    /// The model directory, relative to the audio models dir.
    pub path: String,
    /// Whether that path exists.
    pub path_present: bool,
    pub task: String,
    pub mode: String,
    pub lazy: Option<bool>,
    pub busy_timeout_ms: Option<i64>,
    /// `cpu` when the row is switched to the CPU; null inherits the class.
    pub backend: Option<String>,
    pub threads: Option<i64>,
    pub default_request_options: serde_json::Map<String, serde_json::Value>,
    pub model_spec_override: Option<String>,
    pub config_id: Option<String>,
    pub weight_id: Option<String>,
    pub load_options: serde_json::Map<String, serde_json::Value>,
    pub session_options: serde_json::Map<String, serde_json::Value>,
    pub voice_presets: serde_json::Map<String, serde_json::Value>,
    pub default_voice_preset: Option<serde_json::Value>,
    pub enabled: bool,
    /// Container image override; null inherits the audio class's.
    pub image: Option<String>,
    /// `podman run` args override, one per line; null inherits.
    pub extra_run_args: Option<String>,
    pub warm_start: bool,
    pub hold_fallback_mode: String,
    pub hold_fallback: Option<String>,
    /// What the container was measured to hold on the GPU.
    pub residency: Option<LearnedResidency>,
    /// The image the container starts from (the row's, else the class's).
    pub effective_image: String,
    /// The `podman run` args in effect, one per line.
    pub effective_extra_run_args: String,
    /// `gpu` or `cpu`.
    pub runs_on: String,
    pub threads_in_effect: i64,
    /// `row`, `class`, `cores` or `logical_cpus`.
    pub threads_source: String,
    /// The `server.json` entry this row renders to.
    pub server_json: serde_json::Value,
    /// The container's state, when one is running.
    pub running: Option<AudioRunning>,
    /// What the row is charged on the GPU, in a sentence.
    pub residency_note: String,
    /// The learned residency admission charges, when it belongs to the
    /// current configuration.
    pub residency_charged_bytes: Option<u64>,
    pub problems: Vec<String>,
    /// How to prove the model works.
    pub next_step: String,
}

/// A running audio container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioRunning {
    pub state: String,
    pub in_flight: u64,
}
