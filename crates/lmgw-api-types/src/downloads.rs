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
    /// With `fit` (find-a-model design §3.2, D120): the file this list
    /// recommends, the pick beside what runs on the graphics card now, else
    /// the whole card's. `None` without `fit`, or when no file qualifies.
    pub recommended: Option<String>,
    /// With `fit`: the pick by each measure, with its "Why this file" line.
    pub picks: Option<RepoPicks>,
    /// With `fit`: the card the marks were measured against, one reading for
    /// every file; `None` without one (D9), when every mark is `no_card`. A
    /// file's own `fit.card` is left out where it is this one.
    pub card: Option<crate::model_find::CardMeasure>,
    /// With `fit`: the file header bytes this answer received from Hugging
    /// Face, every file's, a read that failed after it began included; a
    /// header lmgw had read before adds nothing (D123).
    pub header_bytes_read: u64,
    /// With `fit`: what holds for the whole list, each a sentence, said
    /// here once rather than in every file's notes: the GPU hold's line,
    /// admission off, what other programs use not being known, where the
    /// estimates came from, why the header reads stopped, why no file was
    /// measured (no card to measure against, a class whose files aren't
    /// measured).
    pub fit_notes: Vec<String>,
    /// With `fit=unless_metered` on a metered connection (D123): why no
    /// header was read. Every mark is then by size and nothing is
    /// recommended; `fit=true` reads the headers.
    pub metered: Option<String>,
}

/// What `GET /api/hf/repo`'s `fit` asks for (find-a-model design §3.3, D68).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum RepoFit {
    /// No marks, and no header read.
    #[default]
    #[serde(rename = "false")]
    No,
    /// Marks from the files' headers, read on a metered connection too: the
    /// caller asked for them (D59).
    #[serde(rename = "true")]
    Headers,
    /// Marks from the headers unless the connection is metered; then every
    /// mark is by size and `metered` says why (D123): what the dashboard
    /// asks by itself.
    #[serde(rename = "unless_metered")]
    UnlessMetered,
}

/// The file a repo's list recommends by each measure (find-a-model design
/// §3.2, D120).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoPicks {
    pub whole_card: crate::model_find::FilePick,
    pub beside: crate::model_find::FilePick,
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
    /// With `fit`, for a `weights` file of the chat or aux class: its fit on
    /// the graphics card by both measures (find-a-model design §3.3), what
    /// `local_model_plan` with `repo` + `file` answers as its `fit`, less
    /// what the list says once (D63): `card` is `None` where it is the
    /// list's `card`, and the notes leave out the list's `fit_notes`.
    /// `None` without `fit` and for the other roles.
    pub fit: Option<crate::model_find::PlanFit>,
    /// The file whose header this file's fit was estimated from (§3.3):
    /// never smaller than the real use, so its "fits" is true, but its width
    /// isn't known and it is never the recommended file. `None` when the
    /// file's own header was read, or it is marked by its size alone.
    pub estimated_from: Option<String>,
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
    /// The image `target`'s models run in, when it is not on this machine
    /// or podman could not say: the download fetches a missing one too, a
    /// multi-gigabyte download of its own, and starts nothing
    /// ([`EngineDownload`]). `null` when it is here.
    pub engine: Option<EngineDownload>,
}

/// What a model download did about the image the models of its class run
/// in (machine-check design §5, D16): that image was not on this machine,
/// so the download fetches it too — through the `image_pull` job a start
/// of the model would use, keyed by the reference, so a start or an
/// Engines-page pull of the same image joins it. Nothing is started: the
/// first request then only loads the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EngineDownload {
    /// The image reference, as the class's Image setting names it.
    pub image: String,
    /// What became of it: downloading, refused, or not known whether it
    /// is needed.
    pub state: EngineFetch,
    /// The `image_pull` job fetching it: started now, or the one already
    /// pulling it. Its bytes are on the jobs feed (`done`/`total`, and
    /// `size_note` in its detail when the size is not known), and
    /// `container_image_pull_status` reads how it ended. `null` unless
    /// `state` is `downloading`; `note` says why.
    pub job_id: Option<i64>,
    /// Why nothing is downloading: why it could not begin (an image built
    /// on this machine is never pulled — "run its build again on the
    /// Engines page" — and a reference no registry can give is refused),
    /// or why podman could not say whether the image is here.
    pub note: Option<String>,
}

/// What became of a model download's fetch of its class's image
/// ([`EngineDownload::state`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum EngineFetch {
    /// The image is missing and its `image_pull` job runs (`job_id`).
    #[default]
    Downloading,
    /// The image is missing and its download could not begin (`note`).
    Refused,
    /// Podman failed to say whether the image is on this machine, or did
    /// not answer in time (`note`): nothing was fetched, and the model's
    /// first start looks again.
    NotKnown,
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
    /// The bytes a failed, cut or paused transfer left in its `.part`, which
    /// Retry or Resume goes on from (find-a-model design D135). `None` while
    /// a transfer runs, and when nothing is kept.
    pub kept_bytes: Option<u64>,
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
