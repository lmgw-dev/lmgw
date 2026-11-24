//! Settings › Documents' model files (document-conversion design §3, WP8):
//! the models that run on the processor through ONNX Runtime to read a
//! document's layout. A model file is not shipped and never downloaded
//! unasked: [`DocCaps::models`](super::DocCaps::models) says for each
//! whether its file is here, what it adds, its size, licence and origin,
//! and whether it is switched on; `POST /api/op/doc_model_add` queues its
//! download through lmgw's download queue ([`DocModelAddResult`]).

use serde::{Deserialize, Serialize};

/// What is at a model file's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocModelState {
    /// The file is there (its size is the model's; its SHA-256 is checked
    /// before it is first loaded).
    Found,
    /// Nothing is there.
    Missing,
    /// Its download is queued or running ([`DocModelRow::download_id`]).
    Downloading,
    /// Something else is at its place (`message` says what): not used.
    WrongFile,
    /// The folder model files go into is not set (`message` says where to
    /// set it).
    NoFolder,
}

/// The loaded model, while it is in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocModelLoaded {
    /// The threads its runs use.
    pub threads: u32,
    /// Seconds since a page last used it; `None` while a page is using it.
    pub idle_secs: Option<u64>,
}

/// One model file lmgw can use for converting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocModelRow {
    /// `layout`: what `doc_model_add` takes.
    pub id: String,
    /// Its name in plain words: "Layout model (docling heron)".
    pub label: String,
    /// What it adds, in plain words.
    pub adds: String,
    /// The kinds of file ([`FormatRow::label`](super::FormatRow::label))
    /// it is used for.
    pub used_for: Vec<String>,
    pub state: DocModelState,
    /// Switched on ("Use the layout model", `settings_set`'s
    /// `documents_onnx_layout`). Off until someone switches it on.
    pub chosen: bool,
    /// Conversions use it now: its file is found and it is switched on.
    pub in_use: bool,
    /// Why it cannot be used, or where its folder is set.
    pub message: Option<String>,
    /// The file's size in bytes.
    pub size_bytes: u64,
    /// Its licence, as an SPDX expression.
    pub licence: String,
    /// The Hugging Face repository it comes from.
    pub repo: String,
    /// The commit it is taken at.
    pub revision: String,
    /// The file's name in the repository.
    pub file: String,
    /// The file's SHA-256, lowercase hex.
    pub sha256: String,
    /// The page it can be read about on.
    pub url: String,
    /// Where its file is, or would be; `None` while the folder is not set.
    pub path: Option<String>,
    /// Its download's id on the Downloads page, when it was queued here.
    pub download_id: Option<i64>,
    /// About what it holds in memory while loaded, in bytes.
    pub memory_bytes: u64,
    /// The most threads one of its runs uses; each is a slot of the tool
    /// pool while the run lasts.
    pub threads: u32,
    /// Seconds after its last page until it is dropped from memory.
    pub idle_unload_secs: u64,
    /// `None` while it is not in memory.
    pub loaded: Option<DocModelLoaded>,
}

/// `POST /api/op/doc_model_add {id}`: the model file queued into the folder
/// model files go into, through the download queue (it shows on the
/// Downloads page). A file already there, or already on its way, is not
/// queued again. Nothing is loaded or switched on by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocModelAddResult {
    pub ok: bool,
    /// [`DocModelRow::id`].
    pub id: String,
    /// The queued download's id (`hf_downloads` with `ids` follows it).
    pub download_id: Option<i64>,
    pub already_present: bool,
    pub already_queued: bool,
    /// What happened and what comes next, in a sentence.
    pub message: String,
}
