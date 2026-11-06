//! Get it (find-a-model design §6, §9.1, WP5): what `model_get` answers,
//! what its job reports while it waits, plans, fits and creates, what it
//! ends with, and the model editor's "Fit again" (D26).
//!
//! Every field is `#[serde(default)]`, as the DTOs are.

use serde::{Deserialize, Serialize};

use crate::model_find::{FileFit, FitMeasure, FitNote};
use crate::{EngineDownload, HfCompanion, QueuedDownload};

/// `model_get`'s arguments (the op's and `lmgw__model_get`'s).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetArgs {
    /// The Hugging Face repo, `owner/name`.
    pub repo: String,
    /// The weights file's path in the repo (a split file's first part).
    pub file: String,
    /// `chat` or `aux`; absent, the class the file's header says.
    pub target: Option<String>,
    /// The measure the card showed (D120): `beside` (the default) or
    /// `whole_card`.
    pub measure: Option<FitMeasure>,
    /// The model's id; absent, the repo's name in lower case without a
    /// trailing `-GGUF` (D28). A taken one gets `-2`, `-3` and so on.
    pub model_id: Option<String>,
}

/// The models directory's free space against what the files need (D125):
/// a warning, never a block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiskRoom {
    /// The bytes still to download: every queued file's listed size less
    /// what its kept `.part` already holds. A file already on disk counts
    /// nothing.
    pub needs_bytes: u64,
    /// The free bytes of the filesystem the models directory is on, read
    /// with `statvfs` now; `None` when it couldn't be read.
    pub free_bytes: Option<u64>,
    /// The files need more than is free.
    pub short: bool,
    /// "Needs 15.6 GiB on the disk, 8.5 GiB is free", when short; else none.
    pub sentence: Option<String>,
    /// What the check leaves out, each a sentence.
    pub notes: Vec<String>,
}

/// `model_get`'s answer: what was queued and the job that sets it up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetStarted {
    /// The `model_get` job (kind `model_get`), on the jobs feed; `None`
    /// when a row already serves the file (`row_id`).
    pub job_id: Option<i64>,
    /// The get (`model_get_status` reads it); `None` when a row already
    /// serves the file.
    pub get_id: Option<i64>,
    /// A row of the class that already serves the file (D87), by its id in
    /// its class's table: that row is the answer, nothing was queued and no
    /// get runs. `model_id` and `public_name` are its own.
    pub row_id: Option<i64>,
    /// The id the row will have (D28); the job may still suffix it when
    /// another row takes the name before it is created, and
    /// `model_get_status` then says the one it got.
    pub model_id: String,
    /// The name it will answer to on `/v1` (D27): the chat class's public
    /// name, or the aux prefix's.
    pub public_name: String,
    /// `chat` or `aux`.
    pub target: String,
    pub measure: FitMeasure,
    pub repo: String,
    pub file: String,
    /// Every file of the get with its download row, the weights' parts and
    /// the companions alike; a file already on disk is listed with its row
    /// as `done` and isn't fetched again.
    pub downloads: Vec<QueuedDownload>,
    /// The projector (D121) and, for chat, the drafter.
    pub companions: Vec<HfCompanion>,
    /// The image the class's models run in, when it isn't on this machine
    /// (machine check §5); `null` when it is here.
    pub engine: Option<EngineDownload>,
    /// The free-disk check (D125).
    pub disk: DiskRoom,
    /// A get of this file was already under way, and this answer is that
    /// one's: nothing new was queued.
    pub joined: bool,
    pub message: String,
}

/// Where a get is (§6.2), as its job's progress detail says it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ModelGetStage {
    /// The files are downloading.
    #[default]
    Downloading,
    /// No file is transferring and one or more wait: paused on a metered
    /// connection, or cut and not started again. `waiting` says which and
    /// why.
    Paused,
    /// The files are here: planning, fitting and creating the row.
    SettingUp,
    /// The row is there.
    Ready,
}

/// A `model_get` job's progress detail (`done`/`total` are the files'
/// bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetDetail {
    pub get_id: i64,
    pub repo: String,
    pub file: String,
    pub target: String,
    pub model_id: String,
    pub stage: ModelGetStage,
    /// The download rows it waits for.
    pub downloads: Vec<i64>,
    /// Each file that waits with nothing transferring it, and why; empty
    /// when none waits.
    pub waiting: Vec<GetWaiting>,
}

/// Why a file of a get waits with nothing transferring it (WP7, D108),
/// read from its download row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WaitReason {
    /// Paused on a metered connection (D122): it goes on once the
    /// connection isn't metered, or with Resume on the Downloads page.
    Metered,
    /// Queued, or cut by a shutdown, and nothing transfers it yet.
    #[default]
    NotStarted,
    /// The download row keeps another note: `note` says it.
    Other,
}

/// One file of a get that waits (WP7, D108).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GetWaiting {
    /// The file's path in the repo.
    pub file: String,
    pub reason: WaitReason,
    /// The download row's own note, as the Downloads page shows it; `None`
    /// when it has none (`not_started`).
    pub note: Option<String>,
}

/// A `model_get` job's result: the row it set up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetDone {
    pub get_id: i64,
    pub model_id: String,
    pub public_name: String,
    pub target: String,
    /// The row's id in its class's table.
    pub row_id: i64,
    /// `false` when a row already served this file and the get answered
    /// that one (§6.2 step 4).
    pub created: bool,
    /// The context the row was created with.
    pub ctx_size: Option<u64>,
    /// The fit the context comes from, by the get's measure; `None` when a
    /// row already served the file.
    pub fit: Option<FileFit>,
    /// The note the row keeps when its context is a fit's shorter one
    /// (D26).
    pub note: Option<FitNote>,
    /// The plan's and the create's warnings, each a sentence.
    pub warnings: Vec<String>,
    pub next_step: String,
}

/// "Fit again"'s arguments (`model_fit_again`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelFitAgainArgs {
    /// `chat` or `aux`.
    pub target: String,
    /// The row's id in its class (`model_id`, not the public name).
    pub model_id: String,
    /// Save the changes the answer without it listed, as they were shown.
    /// The fit is taken again first; when it gives other changes nothing
    /// is written and the answer lists the new ones to confirm.
    pub confirm: Option<Vec<FitChange>>,
}

/// One parameter "Fit again" changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FitChange {
    /// `ctx_size`, `ubatch_size` or `batch_size`.
    pub key: String,
    pub from: Option<u64>,
    pub to: Option<u64>,
}

/// "Fit again"'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelFitAgain {
    pub model_id: String,
    pub target: String,
    /// The measure fitted to: the note's, else beside what runs (D120).
    pub measure: FitMeasure,
    /// The row's note as it is now (after a save, the new one); `None`
    /// when its context isn't a fit's shorter one.
    pub note: Option<FitNote>,
    /// The fit of the row's own parameters on the card now, the row left
    /// out of what is beside it (D55).
    pub fit: FileFit,
    /// What a save changes; empty when the row is as the fit says.
    pub changes: Vec<FitChange>,
    /// The changes were written.
    pub saved: bool,
    /// The confirmed changes weren't the fit's any more: nothing was
    /// written, and `changes` are the new ones.
    pub outdated: bool,
    /// What the person reads.
    pub sentence: String,
}

/// `model_get_status`'s arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetStatusArgs {
    /// The get, as `model_get` answered it (`get_id`).
    pub get_id: i64,
}

/// How a get stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ModelGetState {
    /// Waiting for its files, or setting up the row (`stage` says which).
    #[default]
    Running,
    /// The row is there (`row_id`, `result`).
    Done,
    /// It stopped; `error` says why and what to do.
    Failed,
    /// Cancelled, its own downloads with it.
    Canceled,
}

/// `model_get_status`'s answer: one get as it stands now, read from its
/// row and its job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelGetStatus {
    pub get_id: i64,
    pub repo: String,
    pub file: String,
    /// `chat` or `aux`.
    pub target: String,
    /// The id the row has, or will have: settled again when it is created
    /// (D28).
    pub model_id: String,
    /// The name it answers to on `/v1`.
    pub public_name: String,
    pub state: ModelGetState,
    /// Where a running get is; `None` once it ended, or before its job
    /// reported.
    pub stage: Option<ModelGetStage>,
    /// Each file that waits with nothing transferring it, and why (a
    /// running get's); empty when none waits.
    pub waiting: Vec<GetWaiting>,
    /// The bytes of its files here so far, and their total when every
    /// size is known (a running get's).
    pub done_bytes: u64,
    pub total_bytes: Option<u64>,
    /// Why a failed get stopped, and what to do.
    pub error: Option<String>,
    /// The row it set up or found, in its class's table.
    pub row_id: Option<i64>,
    /// The job that runs or ran it.
    pub job_id: Option<i64>,
    /// Every file of the get with its download row as it is now.
    pub downloads: Vec<QueuedDownload>,
    /// A finished get's result.
    pub result: Option<ModelGetDone>,
}
