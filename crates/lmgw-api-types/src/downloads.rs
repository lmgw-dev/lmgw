//! Hugging Face downloads (tolerant mirrors of the ops-plane JSON)

use serde::{Deserialize, Serialize};

/// `GET /api/hf/repo?repo=` — the repo's weights files, split parts collapsed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoFiles {
    pub repo: String,
    /// Which class's file kinds and role vocabulary were listed. Empty on a
    /// response from before the image class existed; `chat` then.
    pub target: String,
    pub files: Vec<RepoFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoFile {
    pub file: String,
    /// Filename heuristic, in the vocabulary of the target that was listed:
    /// `weights | mmproj | drafter | other` for chat/aux/audio, and
    /// `diffusion | checkpoint | vae | text_encoder | lora | upscaler | other`
    /// for `image` (image-generation design §7.1). A string, not an enum, on
    /// both sides of the wire — the image roles are guesses the editor lets
    /// the owner override, and a new one must not break an old client.
    pub role: String,
    pub quant: Option<String>,
    /// >1 = split GGUF; selecting it downloads all parts.
    pub parts: u32,
    pub size_bytes: u64,
    /// Human-formatted size, server-rendered.
    pub size: String,
}

/// `POST /api/op/hf_add` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HfAddResult {
    pub ok: bool,
    pub repo: String,
    pub weights: String,
    pub files_queued: u32,
    pub downloads: Vec<QueuedDownload>,
    /// Relative GGUF path (`repo/file`) — feed to plan/create once done.
    pub gguf_path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QueuedDownload {
    pub id: i64,
    pub file: String,
    pub dest_path: String,
    pub status: String,
}

/// `GET /api/hf/downloads`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DownloadsView {
    pub downloads: Vec<DownloadRow>,
    /// Number of live (actually transferring) downloads.
    pub active: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DownloadRow {
    pub id: i64,
    pub repo: String,
    pub file: String,
    pub dest_path: String,
    /// `chat | aux | audio`.
    pub target: String,
    /// `queued | downloading | done | failed | update_available` — a row that
    /// claims queued/downloading with no live progress is *interrupted*
    /// (derived, never stored).
    pub status: String,
    pub error: Option<String>,
    pub size: Option<String>,
    pub downloaded_at: Option<String>,
    pub received_bytes: Option<u64>,
    pub percent: Option<u64>,
    /// Id of the `hf_download` job currently transferring this file, if any —
    /// what "Cancel" acts on. `None` means nothing is running for this row.
    pub job_id: Option<i64>,
}

impl DownloadRow {
    pub fn interrupted(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "downloading") && self.received_bytes.is_none()
    }
}
