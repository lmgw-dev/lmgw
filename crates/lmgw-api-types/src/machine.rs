//! The machine check (machine-check design §4.1): whether this computer can
//! run local models, what is missing in plain words, and the plan of root
//! steps that gets the graphics card ready.
//!
//! `POST /api/op/machine_check` and `lmgw__machine_check` answer
//! [`MachineCheck`]; `GET /api/status` and the `machine` frame of
//! `/api/events` carry [`MachineSummary`]. The fix itself is no route: it
//! runs only from the app's own window, after a confirmation and a password
//! (§3.4, §4.3), through the shell command
//! `machine_apply`, whose events and answer are the last types here
//! ([`MachineApplyEvent`], [`MachineApplyResult`]). The one fix that needs
//! no password, switching the model settings to the ones for the card this
//! computer has (§1.4, WP12), is `POST /api/op/machine_settings_fit`
//! ([`MachineSettingsFitArgs`], answered by [`MachineSettingsFit`]); the
//! settings item offers it as its [`MachineAction`].
//!
//! Every `sentence` and `headline` is plain: no "container", no "CDI", no
//! "Podman". The technical names are in each item's `detail`.

use serde::{Deserialize, Serialize};

/// `machine_check`'s arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineCheckArgs {
    /// Run the check now (joining a run already in progress) instead of
    /// answering the last result.
    pub fresh: bool,
}

/// The whole check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineCheck {
    /// When it ran: UTC, `YYYY-MM-DD HH:MM:SS`.
    pub checked_at: String,
    pub verdict: MachineVerdict,
    /// The verdict in one plain sentence.
    pub headline: String,
    /// The items, in the order they are checked: graphics card and driver,
    /// using the card for models, running models, model settings, room, the
    /// download.
    pub items: Vec<MachineItem>,
    /// What can be done about it, when anything is to be done or offered.
    pub plan: Option<MachinePlan>,
    /// The last start of a local model on the graphics card that failed
    /// because of this computer (the check named why), whoever asked for it:
    /// a client, the Chat, a lab, the model's Test. `None` before one since
    /// lmgw started. A surface that was told "Not now" asks again once this
    /// is another failed start than the one it was told it beside
    /// (machine-check design D171, D182).
    #[serde(default)]
    pub failed_start: Option<MachineFailedStart>,
}

/// A local model's start on the graphics card that failed because of this
/// computer: `gpu_setup`'s start (machine-check design §2.5, D182).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineFailedStart {
    /// The model's id.
    pub model: String,
    /// Its class: `chat`, `aux`, `audio` or `image`.
    pub class: String,
    /// When it failed: UTC, `YYYY-MM-DD HH:MM:SS`.
    pub at: String,
    /// How many such starts failed since lmgw started, this one included:
    /// two in the same second are still two.
    pub count: u64,
}

/// The headline verdict (§1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MachineVerdict {
    /// Local models can run.
    Ready,
    /// Something is missing that lmgw can set up with a button here.
    NeedsFix,
    /// Something is missing that is added by following instructions here.
    NeedsSteps,
    /// A restart finishes a driver update or start.
    NeedsRestart,
    /// The NVIDIA card needs NVIDIA's driver.
    NeedsDriver,
    /// No graphics card: cloud models only.
    CloudOnly,
}

/// One checked thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineItem {
    pub id: MachineItemId,
    pub state: ItemState,
    /// What a newcomer reads.
    pub sentence: String,
    /// What was read and where from, with the technical names: the fold.
    pub detail: Vec<String>,
    /// The typed reason, when the item is not plainly ok.
    pub cause: Option<MachineCause>,
    /// What the person can do about it from here, without a password; only
    /// the model settings item has one yet (§1.4).
    pub action: Option<MachineAction>,
}

/// What an item offers to do (§1.4, WP12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MachineAction {
    /// Switch the model settings to the ones lmgw ships for this card:
    /// `machine_settings_fit` with this `fit` and `confirm` set to these
    /// `changes`, after a confirmation that lists them, `left` and
    /// `running`. An ordinary settings write: no password.
    SettingsFit {
        fit: SettingsFit,
        /// The button's words ("Switch to the settings for AMD cards").
        label: String,
        /// Each field the switch changes, as it is and as it would become.
        changes: Vec<SettingsFitChange>,
        /// What it leaves as it is, and why.
        left: Vec<SettingsFitLeft>,
        /// The running models whose settings the switch changes: each is
        /// stopped after the write, unless it is answering a request.
        running: Vec<SettingsFitRunning>,
    },
}

/// Which card's model settings (§1.4): the images, run arguments and audio
/// backend lmgw ships for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SettingsFit {
    /// AMD cards, through Vulkan: the llama.cpp, audio.cpp and
    /// stable-diffusion.cpp Vulkan images, given the render nodes and the
    /// user's groups.
    AmdVulkan,
    /// NVIDIA cards, through CUDA: lmgw's defaults.
    NvidiaCuda,
}

/// `machine_settings_fit`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSettingsFitArgs {
    /// Which card's settings to switch to.
    pub fit: SettingsFit,
    /// Write the switch: the `changes` of the offer being confirmed (the
    /// settings item's action, or an answer without `confirm`), as they
    /// were shown. The switch is worked out again from the settings as they
    /// are when it is written; it is written only when that gives this same
    /// list, and otherwise nothing is written and the answer carries the
    /// list as it is now (`outdated`). Without it nothing is written, and
    /// the answer says what the switch would change.
    #[serde(default)]
    pub confirm: Option<Vec<SettingsFitChange>>,
}

/// A setting's value as the switch shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SettingsFitValue {
    /// An image or the audio backend.
    Text(String),
    /// Run arguments, one `podman run` argument each, as the field holds
    /// them (one per line in the dashboard).
    Args(Vec<String>),
}

/// One field the switch changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsFitChange {
    /// The model class: `chat`, `aux`, `audio` or `image`.
    pub class: String,
    /// The model whose own setting this is; `None` for the class's setting.
    pub model: Option<String>,
    /// The field as the dashboard names it: "Settings → Runtimes → Chat →
    /// Image" for a class, "Image override" or "Extra podman run args
    /// override" on a model's page.
    pub field: String,
    /// The value now. `None`: the model has no setting of its own and uses
    /// its class's.
    pub from: Option<SettingsFitValue>,
    /// The value after the switch. `None`: the model's own setting is
    /// cleared, so it uses its class's from then on.
    pub to: Option<SettingsFitValue>,
}

/// A setting the switch leaves as it is, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsFitLeft {
    /// The model class: `chat`, `aux`, `audio` or `image`.
    pub class: String,
    /// The model whose own setting this is; `None` for the class's.
    pub model: Option<String>,
    /// The field, as [`SettingsFitChange::field`] names it.
    pub field: String,
    /// Its value.
    pub value: SettingsFitValue,
    /// Why it stays, in plain words.
    pub why: String,
}

/// A running model whose settings the switch changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsFitRunning {
    /// The model class: `chat`, `aux`, `audio` or `image`.
    pub class: String,
    pub model: String,
    /// What the confirmed switch did to it; `None` in an offer.
    pub stop: Option<SettingsFitStop>,
}

/// What a confirmed switch did to a running model it concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SettingsFitStop {
    /// Stopped: its next start uses the new settings.
    Stopped,
    /// It had stopped by then.
    NotRunning,
    /// It is answering `in_flight` requests, so it keeps running with the
    /// settings from before until it stops.
    Busy { in_flight: u32 },
    /// Stopping it failed; it may still run with the settings from before.
    Failed { error: String },
}

/// Why a confirmed switch wrote nothing: the switch worked out when it was
/// to be written is not the list that was confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SettingsFitOutdated {
    /// Changes of the confirmed list the switch no longer makes as listed.
    pub withdrawn: Vec<SettingsFitChange>,
    /// Changes the switch makes now that the confirmed list did not have.
    pub added: Vec<SettingsFitChange>,
    /// Models whose own settings were changed while the switch was being
    /// written.
    pub models: Vec<String>,
}

/// `machine_settings_fit`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSettingsFit {
    pub fit: SettingsFit,
    /// Whether the settings were written, or the answer only says what the
    /// switch would change.
    pub applied: bool,
    /// What was done, or would be, in plain sentences.
    pub sentence: String,
    /// Each field changed (or that would be), as it was and as it is. When
    /// a confirmed list was outdated, the switch as it is now: confirm this
    /// list to write it.
    pub changes: Vec<SettingsFitChange>,
    /// What was left as it is, and why.
    pub left: Vec<SettingsFitLeft>,
    /// The running models the switch concerns, and after a write what
    /// happened to each.
    pub running: Vec<SettingsFitRunning>,
    /// Set when `confirm` was not the switch as it is now, so nothing was
    /// written: what differs.
    pub outdated: Option<SettingsFitOutdated>,
    /// The settings save's own notes.
    pub notes: Vec<String>,
    /// The machine check after the write; `None` when nothing was written.
    pub check: Option<MachineCheck>,
}

/// Which item (§1.1 … §1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MachineItemId {
    /// §1.1: the graphics card and its driver.
    GraphicsCard,
    /// §1.2: what lets models use the card (NVIDIA's container toolkit and
    /// lmgw's own description of the card).
    CardForModels,
    /// §1.3: the program that runs models.
    RunningModels,
    /// §1.4: whether the model settings fit the card.
    SettingsFit,
    /// §1.5: free disk space.
    Room,
    /// §1.6: the download of what the card needs.
    Download,
}

/// How an item came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ItemState {
    Ok,
    Problem,
    /// Worth knowing; never changes the verdict.
    Info,
    /// Not checked, because an item before it is not ok: the sentence says
    /// which.
    NotChecked,
}

/// Why an item is not plainly ok (§1's tables).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MachineCause {
    /// No NVIDIA or AMD card; `others` names the display devices there are.
    NoCard { others: Vec<String> },
    /// An NVIDIA card bound to another driver (`nouveau`) or none.
    NoNvidiaDriver { bound: Option<String> },
    /// The driver libraries on disk are newer than the kernel module that
    /// runs: a restart finishes the update.
    DriverMismatch { kernel: String, userspace: String },
    /// The driver is installed and its kernel module is not running.
    /// `secure_boot`: Secure Boot keeps it from loading — it is on, a module
    /// is built for the running kernel, and this boot started after it was
    /// put there. Otherwise it may still be building, or a restart starts
    /// it.
    DriverNotRunning { secure_boot: bool },
    /// The kernel module runs and its libraries are missing.
    DriverLibrariesMissing,
    /// The kernel module's version could not be read.
    DriverUnreadable { why: String },
    /// The versions agree, and NVML in a separate process failed.
    NvmlFailed { error: String },
    /// An AMD card without a device node to compute on.
    AmdNoComputeNode,
    /// NVIDIA's container toolkit is not installed.
    ToolkitMissing,
    /// The toolkit comes from Fedora's retired package.
    ToolkitRetired { package: String, version: String },
    /// lmgw's description of the card could not be written, and the toolkit
    /// that writes it is to blame (it failed, or what it wrote names paths
    /// that are not there).
    SpecNotWritten { error: String, owner: String },
    /// Writing lmgw's description of the card did not finish within the
    /// load timeout: the card's driver stopped answering.
    SpecWriteTimedOut { error: String },
    /// A folder lmgw keeps its description of the card in is not one only
    /// this user may open.
    SpecDirNotPrivate { error: String },
    /// lmgw's own files for the description could not be written or read.
    SpecFilesFailed { error: String },
    /// lmgw writes no description of its own here; the system's is used
    /// (D26).
    SpecOnSystemDirs { why: String },
    /// NVIDIA's software source has not loaded since `last_loaded` while the
    /// system's others did.
    ToolkitSourceFrozen {
        section: String,
        last_loaded: String,
    },
    /// Another enabled repository offers the toolkit.
    ToolkitOtherProvider { repo: String },
    /// Podman is not installed.
    PodmanMissing,
    /// Podman does not work; its own error.
    PodmanBroken { error: String },
    /// This Podman has no `--cdi-spec-dir`.
    PodmanTooOld { version: String },
    /// The settings are NVIDIA's, and the card is AMD's.
    SettingsForNvidia,
    /// The settings are AMD's, and the card is NVIDIA's.
    SettingsForAmd,
    /// These kinds of models run on the processor.
    RunsOnProcessor { classes: Vec<String> },
    /// Models still running with the settings from before a switch of the
    /// model settings (WP12): they use the new ones once they stop.
    RunningOldSettings { models: Vec<String> },
    /// Settings name a CDI device of another kind, which only the system's
    /// specs define.
    OtherCdiKind { devices: Vec<String> },
    /// Engine images not on the machine yet.
    ImagesMissing { references: Vec<String> },
}

/// What can be done, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachinePlan {
    /// SHA-256 over the steps as root runs them (lmgw-machine's
    /// `HashedPlan`), never over this answer: its sentences and sizes do not
    /// change it.
    pub hash: String,
    pub how: PlanHow,
    pub steps: Vec<PlanStep>,
}

/// Where the plan is carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlanHow {
    /// The button in the app's window: one confirmation, one password.
    Helper,
    /// Commands to type in a terminal; `why` says why there is no button.
    /// With no steps, `why` says what is in the way and what to do instead.
    Instructions { why: String },
}

/// One step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanStep {
    pub kind: PlanStepKind,
    /// The confirmation's bullet.
    pub sentence: String,
    /// The exact argv of each program it runs as root, in order.
    pub commands: Vec<Vec<String>>,
    /// The file it writes, in full.
    pub writes_file: Option<PlanFile>,
    /// The download, when the user's package metadata can tell.
    pub download_bytes: Option<u64>,
    /// The same step as lines to type in a terminal, with `sudo`.
    pub terminal: Vec<String>,
    /// What the package the step installs then changes by itself, through
    /// its own install scripts, beyond the step's commands (machine-check
    /// design §4.5). For the fold: display only, never in the hash.
    pub package_scripts: Option<String>,
}

/// Which step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlanStepKind {
    S1AddRepo,
    S1EnableRepo,
    S2AddKey,
    S4Install,
    S4Update,
    S4Swap,
}

/// A file a step writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanFile {
    pub path: String,
    pub content: String,
    /// The mode root gives it (`0o644`, shown as `0644`); in the hash.
    pub mode: u32,
    /// lmgw's own older file replaced, rather than a new file created.
    pub replaces: bool,
    /// For an OpenPGP public key: its fingerprint, computed from `content`,
    /// in groups of four hex digits as people compare it
    /// (`C95B 321B … F796 ECB0`). Display only.
    pub key_fingerprint: Option<String>,
}

/// The verdict alone, in `GET /api/status` and the `machine` frame of
/// `/api/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSummary {
    pub verdict: MachineVerdict,
    pub headline: String,
    pub checked_at: String,
    /// [`MachineCheck::failed_start`].
    #[serde(default)]
    pub failed_start: Option<MachineFailedStart>,
}

impl From<&MachineCheck> for MachineSummary {
    fn from(c: &MachineCheck) -> Self {
        Self {
            verdict: c.verdict,
            headline: c.headline.clone(),
            checked_at: c.checked_at.clone(),
            failed_start: c.failed_start.clone(),
        }
    }
}

// --- The fix: the app window's shell command (§3.5, §4.3, WP7) -----------
//
// Not part of the HTTP API: `machine_apply` is a Tauri command only the
// app's own window calls (D3), so these types have no JSON schema and no
// OpenAPI entry. The dashboard's `MachineCheck` component (WP8a) calls the
// command and renders what these say.

/// What `machine_apply` reports while it runs, in order, over the command's
/// own `Channel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MachineApplyEvent {
    /// The person pressed "Set it up" in the shell's own confirmation, and
    /// the computer is asking for the password now. The progress view says
    /// "Your computer is asking for your password".
    Password,
    /// The password was accepted: the setup runs as root now (the helper's
    /// first line).
    Authorized,
    /// A setup that began earlier (before lmgw was restarted) is still
    /// running: lmgw follows it until it ends, without asking anything.
    /// `state` is systemd's word for it (`active`, `activating`, …).
    Following { state: String },
    /// The steps begin: `steps` of them.
    Started { steps: usize },
    /// Step `step` of `of` (counted from 1) begins.
    StepStarted {
        step: usize,
        of: usize,
        kind: PlanStepKind,
    },
    /// One line of a step's output, whole, as the program printed it. It is
    /// text, never markup: shown as text.
    Line {
        step: usize,
        kind: PlanStepKind,
        stream: MachineLineStream,
        line: String,
    },
    /// The step ended: `exit` 0 when it did what it says; otherwise the
    /// failing program's exit code, the `signal` that ended it, or the
    /// `error` that kept the setup from starting it or writing its file.
    StepEnded {
        step: usize,
        kind: PlanStepKind,
        exit: Option<i32>,
        signal: Option<i32>,
        error: Option<String>,
    },
    /// Something said outside a step's output: the system task's own words,
    /// a journal that did not take a line. Text, never markup.
    Note { line: String },
    /// The steps are over: `exit` 0 when every step ran, 3 when one failed.
    Finished { exit: i32 },
    /// lmgw checks this computer again; the command's answer follows.
    Checking,
}

/// Where a step's line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineLineStream {
    /// The step's program's standard output.
    Stdout,
    /// Its standard error.
    Stderr,
    /// The setup about the step: the command it starts, the file it writes.
    Helper,
}

/// How a `machine_apply` call ended (§3.5's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineApplyOutcome {
    /// Every step ran (exit 0). The check in the answer says whether the
    /// card works now.
    Applied,
    /// There was nothing left to do: nothing was run.
    NothingToDo,
    /// The person chose "Not now" in the confirmation: nothing was run.
    Declined,
    /// The setup can't be started from here: `sentence` says why and what
    /// to do instead. Nothing was run and no password was asked for.
    Unavailable,
    /// The computer changed since the plan was shown (found before the
    /// password, or the helper's exit 2): the check in the answer has the
    /// new list. Nothing was changed.
    PlanChanged,
    /// The helper's exit 2 although lmgw, computing the plan again, still
    /// got one the system does not compute (D88): the button is not offered
    /// for that plan again, and the detail shows both plans.
    ReadDifferently,
    /// lmgw was updated while it was running (the helper is another
    /// version): restart lmgw, then try again.
    Updated,
    /// The password window was closed (pkexec's 126). Nothing was changed.
    Dismissed,
    /// The desktop has no password window running (pkexec's 127, "No
    /// authentication agent found"): the commands are for a terminal.
    NoAgent,
    /// The password wasn't accepted, or this account may not change the
    /// system (pkexec's 127 otherwise).
    NotAuthorized,
    /// A step failed (exit 3): `step` names it, `sentence` the plain
    /// reason; the steps before it stay done.
    StepFailed,
    /// The helper would not act (exit 4).
    Refused,
    /// The system task could not be started, or ended without saying how
    /// its steps went (exit 6).
    TaskFailed,
    /// The setup stopped unexpectedly (a crash, a signal, an exit code
    /// nothing gives).
    Failed,
    /// A setup that was already running ended, and lmgw could not read how
    /// (the system journal is an administrator's): the check says what is
    /// in place now.
    Ended,
}

/// `machine_apply`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineApplyResult {
    pub outcome: MachineApplyOutcome,
    /// What the person reads: what happened and what to do next, plain.
    pub sentence: String,
    /// The step that failed, or the last one that began.
    pub step: Option<PlanStepKind>,
    /// Every line the steps printed and every note, in order, as text: the
    /// lines the `line` and `note` events carried.
    pub output: Vec<String>,
    /// The technical fold: what pkexec, the helper or systemd said, and for
    /// `read_differently` both plans in full.
    pub detail: Vec<String>,
    /// The check this answer rests on: the fresh one after the steps, or
    /// the one made before the confirmation. `None` when the setup was
    /// refused before lmgw checked.
    pub check: Option<MachineCheck>,
}
