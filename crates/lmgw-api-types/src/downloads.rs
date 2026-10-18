//! Hugging Face downloads (tolerant mirrors of the ops-plane JSON)

use serde::{Deserialize, Serialize};

use crate::MessageAck;

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
    /// How to read `role` and what to do with the list, for a person or an
    /// agent. It differs by `target`: the image classes' roles are filename
    /// heuristics and a pipeline spans several repos, the others name the
    /// `weights` file to hand to `hf_add`, which fetches its companions. When
    /// `files` is empty (nothing in the repo matched) it says so plainly
    /// instead.
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoFile {
    pub file: String,
    /// Filename heuristic, in the vocabulary of the target that was listed:
    /// `weights | mmproj | drafter | other` for chat/aux/audio, and
    /// `diffusion | checkpoint | vae | text_encoder | lora | upscaler | other`
    /// for `image`. A string, not an enum, on
    /// both sides of the wire — the image roles are guesses the editor lets
    /// the user override, and a new one must not break an old client.
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
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub repo: String,
    /// The weights file asked for: the `file`, or the one the `quant` named.
    pub weights: String,
    /// The projector and the drafter queued beside the weights. Empty when
    /// `companions` was off, when the repo has none beside that file, and for
    /// the `image` target (a pipeline's components live in other repos).
    pub companions: Vec<HfCompanion>,
    /// How many files went into the queue: the weights, the companions, and
    /// every part of a split GGUF.
    pub files_queued: u32,
    /// The tracked rows of the file asked for, the companions and every part
    /// of a split GGUF, with the id each can be followed by in
    /// `GET /api/hf/downloads`.
    pub downloads: Vec<QueuedDownload>,
    /// What to look out for in what was queued, one sentence each; empty when
    /// nothing. Today: a shard of a sharded `.safetensors` is queued alone,
    /// and stable-diffusion.cpp cannot reassemble shards.
    pub warnings: Vec<String>,
    /// Relative GGUF path (`repo/file`) — feed to plan/create once done.
    pub gguf_path: String,
    /// `repo/`: the directory under the models dir that the repo's files land
    /// in, with the trailing slash.
    pub dest_prefix: String,
    /// The class whose models dir the files go to: `chat | aux | audio |
    /// image` (`embed` as asked is `aux` here).
    pub target: String,
    /// What was queued and what to do next, which depends on `target`.
    pub message: String,
}

/// A file `hf_add` queued beside the weights.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HfCompanion {
    /// Path inside the repo.
    pub file: String,
    /// `mmproj | drafter`.
    pub role: String,
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
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DownloadRow {
    pub id: i64,
    pub repo: String,
    pub file: String,
    pub dest_path: String,
    /// `chat | aux | audio | image`.
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
    /// The revision the download asks for: `main`, or the commit an audio
    /// catalog entry pins. `None` on a row from before it was recorded (a
    /// `main` download).
    pub requested_revision: Option<String>,
    /// The commit the file on disk came from (the hub's `X-Repo-Commit`).
    /// `None` when unknown — every row downloaded before it was recorded.
    pub resolved_commit: Option<String>,
}

impl DownloadRow {
    pub fn interrupted(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "downloading") && self.received_bytes.is_none()
    }
}

/// What `hf_set` answers, by `action`: a [`MessageAck`] for `redownload`,
/// `cancel` and `delete`, an [`HfUpdatesChecked`] for `check_updates`.
///
/// The two shapes share only `ok`, so this is the document's description of
/// the one op, not a type to read an answer into: a reader knows the action it
/// sent and reads that action's type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum HfSetAnswer {
    /// `action: redownload | cancel | delete`.
    Done(MessageAck),
    /// `action: check_updates`.
    Checked(HfUpdatesChecked),
}

/// `POST /api/op/hf_set` with `action: check_updates`: the tracked downloads of
/// one class compared with the hub.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HfUpdatesChecked {
    /// `false` when a file could not be compared (`errors` is not empty); the
    /// answer is still a success, for the files that could.
    pub ok: bool,
    /// How many finished downloads now differ from the hub's copy; each is
    /// flagged `update_available`.
    pub updates_available: u64,
    /// One sentence per file the hub could not be asked about.
    pub errors: Vec<String>,
}
