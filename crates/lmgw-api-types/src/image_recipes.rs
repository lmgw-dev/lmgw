//! Image recipes (image-generation design §7.2)

use super::*;
use serde::{Deserialize, Serialize};

/// `POST /api/op/image_recipes` — the shipped pipeline list, joined against
/// what is on disk and what is downloading.
///
/// The audio catalog's sibling with a static source: there is no upstream
/// catalog for stable-diffusion.cpp pipelines, and a pipeline spans repos, so
/// the list is compiled into the binary and every `(repo, file, size, gated)`
/// in it was checked against the hub's public metadata API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipes {
    pub recipes: Vec<ImageRecipe>,
    /// The image class's models dir; empty means the class is unconfigured and
    /// nothing can be downloaded or found.
    pub models_dir: String,
    /// True when `models_dir` is empty — the one refusal the UI can show
    /// before the owner clicks anything.
    pub models_dir_missing: bool,
    /// A Hugging Face token is configured, so a gated component is reachable.
    pub hf_token_set: bool,
    /// What to call next, for a caller that has no dashboard (the tool plane's
    /// rule: a result names the tool that follows it).
    pub next_step: String,
}

/// One shipped family.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipe {
    pub key: String,
    pub display_name: String,
    pub description: String,
    pub components: Vec<ImageRecipeComponent>,
    /// Runtime + default-generation flags for the prefilled row, already in
    /// `ImageModel.args` shape.
    pub args: serde_json::Map<String, serde_json::Value>,
    pub modes: Vec<String>,
    pub edit: bool,
    /// Measured VRAM where a spike measured it, and the word "unmeasured"
    /// where none did — never a number derived from the file sizes.
    pub vram_note: String,
    /// Bytes of the default file set, on disk.
    pub total_bytes: u64,
    pub total_size: String,
    /// Every component is already on disk.
    pub installed: bool,
    /// Some but not all components are on disk or downloading.
    pub partial: bool,
    /// An image model row already names this recipe's primary file.
    pub served: bool,
    /// Suggested `model_id` for the prefilled row.
    pub suggested_model_id: String,
}

/// One file of a pipeline, under the `files` key it fills.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipeComponent {
    /// The `files` key — `diffusion_model` / `model` / `vae` / `clip_l` /
    /// `clip_g` / `t5xxl` / `llm` / `llm_vision`.
    pub role: String,
    pub repo: String,
    pub file: String,
    /// Path relative to the image models dir once downloaded — what the row's
    /// `files` value is.
    pub dest_path: String,
    pub size_bytes: u64,
    pub size: String,
    /// The hub refuses this repo without a token that accepted the licence.
    pub gated: bool,
    pub note: String,
    /// The file is on disk under the image models dir.
    pub present: bool,
    /// A tracked download exists for it and has not finished.
    pub downloading: bool,
    /// A tracked download for it finished.
    pub done: bool,
    /// Tracked download row id, when there is one — join against
    /// `GET /api/hf/downloads` for live progress.
    pub download_id: Option<i64>,
    /// Display names of the other recipes that name this exact file. A
    /// component is shared on purpose (the second recipe to want it finds it
    /// on disk), but it also means one recipe's download shows up under every
    /// neighbour — this is what lets a card say whose download it is watching.
    #[serde(default)]
    pub shared_with: Vec<String>,
    pub alternatives: Vec<ImageRecipeAlternative>,
}

/// A different file for the same component — the quantizations the repo ships.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipeAlternative {
    pub file: String,
    pub size_bytes: u64,
    pub size: String,
    pub label: String,
    /// This alternative is the file already on disk.
    pub present: bool,
}

/// `POST /api/op/image_recipe_add` — components queued, plus the row the
/// editor is about to be filled with.
///
/// It does **not** create the row (§7.2, "hands a prefilled row to the
/// editor"): the files are not on disk yet, and `image_model_set` refuses a
/// row whose `files` do not resolve — correctly, because that check is what
/// keeps a broken row out of the table. So the caller downloads, then posts
/// `row` at `image_model_set action=create`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipeAddResult {
    pub ok: bool,
    pub key: String,
    pub display_name: String,
    /// Components queued now (the ones that were not already on disk).
    pub files_queued: u32,
    pub downloads: Vec<QueuedDownload>,
    /// Components skipped because the file was already there.
    pub already_present: Vec<String>,
    /// Components skipped because a download for them is already queued or
    /// running — re-queueing one would flip its row back to `queued` under
    /// the job that is downloading it.
    #[serde(default)]
    pub already_queued: Vec<String>,
    /// The `image_model_set` patch, ready to post once the downloads are done.
    pub row: ImageRecipeRow,
    pub message: String,
}

/// The prefilled row an "add from recipe" hands to the editor — the field
/// names `POST /api/op/image_model_set` (and `lmgw__image_model_set`) take.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageRecipeRow {
    pub action: String,
    pub model_id: String,
    pub files: serde_json::Map<String, serde_json::Value>,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub modes: Vec<String>,
    pub edit: bool,
}

/// `POST /api/op/audio_catalog` with `action: "download"` — the package's
/// files go through the shared HF download queue (target `audio`), exactly
/// like a wizard download.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioCatalogInstall {
    pub ok: bool,
    pub family: String,
    pub package: String,
    pub repo: String,
    /// The revision the files are fetched at: the commit the spec pins
    /// (under `audio.catalog_revision = pinned`), else `main`.
    pub revision: String,
    pub files_queued: u32,
    pub downloads: Vec<QueuedDownload>,
    pub message: String,
}
