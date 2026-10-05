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
    /// What the last refresh could not do — a spec file that did not load, a
    /// package repo Hugging Face could not list (offline, rate-limited,
    /// gated) — or that it has not checked the package files yet. Stays until
    /// the next refresh.
    pub warnings: Vec<String>,
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
    /// An enabled audio model of this family loads one of its packages
    /// (some package's `served`). A row that points at the family's files
    /// without saying which package is the packages' `serving_unclear`; one
    /// that loads nothing of the family is `serving_note`.
    pub served: bool,
    /// One sentence per enabled row of this family that matches none of its
    /// packages — an empty or missing root, a `weight_id` that picks no file,
    /// GGUFs no package ships — joined by "; ". Empty when there is none.
    pub serving_note: String,
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
    /// Spec files the package lacks on this machine, once any of it was
    /// downloaded: no finished download, or the file is gone from disk. A
    /// spec that grew after the package was installed (a new built-in
    /// voice) shows up here.
    pub missing_files: Vec<String>,
    /// Downloaded once and short of `missing_files` now, with nothing on
    /// its way: a download ("complete install") fetches those — and, while
    /// a pin is followed, the package's installed files from another commit
    /// than the pin, so the package is one commit again.
    pub incomplete: bool,
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
    /// The revision the spec names for the package's repo (`main`, or a
    /// commit), as written; empty when it names none.
    pub revision: String,
    /// The commit the spec pins the package to — its `revision` when that is
    /// a full commit hash; empty when it pins none.
    pub pinned_commit: String,
    /// Whether a download takes `pinned_commit`: true under
    /// `audio.catalog_revision = pinned`, false under `latest`, which takes
    /// `main` even though the spec pins one. False when nothing is pinned.
    pub pin_followed: bool,
    /// Which commit the downloaded files came from, as a sentence — the
    /// commit and the revision the download asked for, or "unknown" for files
    /// downloaded before lmgw recorded it. Empty when nothing is downloaded.
    pub downloaded_from: String,
    /// Spec files the repo does not publish, as of the refresh that listed
    /// it at the revision a download takes (the pin, when followed): the spec
    /// is ahead of the weights, and a download would be refused.
    pub unpublished_files: Vec<String>,
    /// What is known about whether the repo publishes this package's files,
    /// when it is not simply "all of them": the missing ones with the date of
    /// the listing, or why the repo could not be listed. Empty otherwise.
    pub availability_note: String,
    /// An enabled audio model loads this package's weights: the weights file
    /// lmgw takes the row to load (the GGUFs under its root, narrowed by its
    /// `weight_id`) belongs to this package and no other — or, for a package
    /// without GGUFs, the package is downloaded and the only one under the
    /// row's root. Implies the weights are on disk.
    pub served: bool,
    /// The `model_id`s of the rows that serve it.
    pub served_by: Vec<String>,
    /// Non-empty when a row points at this package's files without telling
    /// it apart from another package (its root holds several and no
    /// `weight_id` picks one): what it is and how to settle it, one sentence
    /// per such row.
    pub serving_unclear: String,
    /// Tracked download rows for this package's files — the UI joins them
    /// against `GET /api/hf/downloads` for live progress.
    pub download_ids: Vec<i64>,
    pub suggested_model_id: String,
    pub suggested_path: String,
    pub suggested_task: String,
    pub suggested_mode: String,
}
