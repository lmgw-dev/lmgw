//! audio.cpp spec catalog (`model_specs/*.json`)

use serde::{Deserialize, Serialize};

/// `GET /api/audio/catalog` — the *cached* spec catalog. Never fetches: the
/// snapshot comes from memory or the kv store, and `POST /api/op/audio_catalog`
/// (`action: "refresh"`) is the only thing that goes to the network.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioCatalog {
    /// RFC3339 fetch time of the snapshot; empty when nothing is cached yet.
    pub fetched_at: String,
    /// Families with an installed package or a served model come first.
    pub families: Vec<AudioFamily>,
}

/// One model family (`model_specs/<family>.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioFamily {
    pub family: String,
    pub display_name: String,
    /// Free-form spec category (`tts`, `asr`, …) — a badge, not an enum.
    pub category: String,
    pub description: String,
    pub tasks: Vec<String>,
    pub languages: Vec<String>,
    /// `supported` | `community` | `experimental` | `testing` | `wip` — how
    /// finished upstream calls this family. Empty when the spec is older than
    /// the field.
    pub status: String,
    /// `ui.tags` — short labels from the project's own WebUI.
    pub tags: Vec<String>,
    /// `ui.docs` — paths into the audio.cpp repo, rendered as links.
    pub docs: Vec<String>,
    /// One extra line the spec offers beside the description.
    pub summary: String,
    /// Voices the family ships, and which it uses by default — enough to
    /// configure a TTS row before its container has ever run.
    pub builtin_voices: Vec<String>,
    pub default_voice: String,
    /// Per-task capability tags, as the spec writes them.
    pub capabilities: serde_json::Map<String, serde_json::Value>,
    /// The typed options this family accepts, by group.
    pub options: AudioFamilyOptions,
    /// At least one package fully downloaded.
    pub any_installed: bool,
    /// An audio model of this family is already configured.
    pub served: bool,
    pub packages: Vec<AudioPackage>,
}

/// A family's declared options, split the way they are configured: `load` and
/// `session` are the two JSON objects on the model row, `request` is what a
/// call may carry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioFamilyOptions {
    pub request: Vec<AudioFamilyOption>,
    pub load: Vec<AudioFamilyOption>,
    pub session: Vec<AudioFamilyOption>,
}

/// One declared option: name, type, and whatever bounds the spec states.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioFamilyOption {
    pub name: String,
    /// `string` | `int` | `float` | `bool` | `enum` | `path` | … — the spec's
    /// own vocabulary, not an enum on this side of the wire.
    pub kind: String,
    pub description: String,
    pub required: bool,
    /// The engine's default, as JSON; absent when the spec states none.
    pub default: Option<serde_json::Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Accepted values of an enum option (a named preset already expanded).
    pub values: Vec<String>,
}

/// One installable package (a file set of a family) plus everything the UI
/// needs to show its state and to prefill the audio-model editor from it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioPackage {
    pub id: String,
    pub display_name: String,
    /// `gguf` / `safetensors`.
    pub format: String,
    /// `q8_0`, `f16`, … — empty when the spec names none.
    pub precision: String,
    pub file_count: u32,
    /// Human size of the downloaded files, empty until they are on disk.
    pub size: String,
    /// audio.cpp's recommended package for the family.
    pub recommended: bool,
    /// Every file downloaded — the package can be served.
    pub installed: bool,
    /// Some (not all) files tracked/downloaded.
    pub partial: bool,
    /// What this package is, when the spec says so (a size, a trade).
    pub description: String,
    /// Hugging Face repo the files come from; empty = no download source in
    /// the spec, so the package cannot be installed from here.
    pub repo: String,
    /// The repo needs an accepted licence and a token: downloading it without
    /// one fails with 401s, so the UI says so before the click.
    pub gated: bool,
    /// Why this package cannot be installed from here, in upstream's words —
    /// a licence that forbids redistribution, a GGUF build not published yet.
    /// Empty when it can.
    pub unavailable_reason: String,
    /// Branch/tag the spec pins, when it pins one.
    pub revision: String,
    /// An audio model already serves this package's path.
    pub served: bool,
    /// Tracked download rows for this package's files — the UI joins them
    /// against `GET /api/hf/downloads` for live progress.
    pub download_ids: Vec<i64>,
    pub suggested_model_id: String,
    pub suggested_path: String,
    pub suggested_task: String,
    pub suggested_mode: String,
}
