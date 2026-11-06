//! What the root helper and the app say to each other (machine-check design
//! §3.1, §3.5, WP6): the helper's exit codes, the JSON lines it prints, and
//! the system task it starts.
//!
//! The helper (`lmgw-machine-helper`, its own crate) prints one [`Event`] per
//! line on stdout; the app's shell (WP7) reads them while it waits and maps
//! the exit code with [`Exit::from_code`]. Both sides use these types, so a
//! line one writes is a line the other parses.
//!
//! The steps run as the transient system unit [`UNIT_SERVICE`], under a
//! shutdown inhibitor ([`unit_argv`]): quitting lmgw or logging out does not
//! kill `dnf` mid-transaction (D22), and the unit's fixed name is the lock
//! that keeps a second setup from starting beside it.

use serde::{Deserialize, Serialize};

use crate::plan::{plan, HashedPlan, Inputs, Plan, Provider, StepKind, Want};

/// The transient unit's name, without its suffix (`systemd-run --unit=`).
pub const UNIT: &str = "lmgw-machine-setup";

/// The transient unit as systemd names it.
pub const UNIT_SERVICE: &str = "lmgw-machine-setup.service";

/// The cgroup every process of that unit is in (`/proc/self/cgroup`'s
/// `0::` line on cgroup v2). `run` refuses to run anywhere else.
pub const UNIT_CGROUP: &str = "/system.slice/lmgw-machine-setup.service";

/// The `SYSLOG_IDENTIFIER` of the helper's journal entries:
/// `journalctl -t lmgw-machine-setup`, or `-u lmgw-machine-setup` for the
/// unit's own.
pub const JOURNAL_ID: &str = "lmgw-machine-setup";

pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
pub const SYSTEMD_INHIBIT: &str = "/usr/bin/systemd-inhibit";
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// The unit's description, as `systemctl status` shows it.
pub const UNIT_DESCRIPTION: &str = "lmgw: setting up the graphics card";

/// What the inhibitor holds back, who holds it and why, as the desktop's
/// shutdown dialog shows it.
pub const INHIBIT_WHAT: &str = "shutdown:sleep";
pub const INHIBIT_WHO: &str = "lmgw";
pub const INHIBIT_WHY: &str = "Setting up the graphics card for local models";

/// The unit's own `/tmp` and `/var/tmp` (`PrivateTmp=yes`): dnf and the
/// packages' scriptlets run as root, and the shared `/tmp` holds whatever
/// any user put there first. `ProtectHome=read-only` is not set yet: it
/// waits for the live round to show that no step needs to write under
/// `/home` or `/root`.
pub const UNIT_PRIVATE_TMP: &str = "--property=PrivateTmp=yes";

/// The command `apply` starts: the unit, waited for with its output on
/// `apply`'s pipes, unloaded when it ends even after a failure and with a
/// `/tmp` of its own, running the inhibitor, which runs
/// `helper run <sha256>`. Every program by its absolute path.
pub fn unit_argv(sha256: &str) -> Vec<String> {
    [
        SYSTEMD_RUN,
        "--wait",
        "--pipe",
        "--collect",
        "--quiet",
        UNIT_PRIVATE_TMP,
        &format!("--unit={UNIT}"),
        &format!("--description={UNIT_DESCRIPTION}"),
        SYSTEMD_INHIBIT,
        &format!("--what={INHIBIT_WHAT}"),
        &format!("--who={INHIBIT_WHO}"),
        &format!("--why={INHIBIT_WHY}"),
        "--mode=block",
        crate::plan::embedded::HELPER,
        "run",
        sha256,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `systemctl show --property=ActiveState --value <unit>`: the question
/// whether the unit runs.
pub fn unit_state_args() -> [&'static str; 4] {
    ["show", "--property=ActiveState", "--value", UNIT_SERVICE]
}

/// Is a unit in `state` (systemd's `ActiveState`) still running? `inactive`
/// and `failed` are not; a unit nobody started reads `inactive`.
pub fn unit_running(state: &str) -> bool {
    matches!(
        state.trim(),
        "active" | "activating" | "deactivating" | "reloading" | "refreshing"
    )
}

/// The one argument `apply` and `run` take: 64 lowercase hex digits, as
/// [`HashedPlan::sha256`] prints them.
pub fn is_plan_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// How the helper ended. Clear of pkexec's own 126 (dismissed) and 127 (not
/// authorised), and of 1, which every program uses for its own failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 0: every step ran (or the plan had none).
    Done,
    /// 2: the hash is none of root's plans for this machine; nothing changed.
    /// The [`Event::PlanChanged`] line says root's plans, its schema and its
    /// lmgw version.
    PlanChanged,
    /// 3: a step failed; the steps before it stay done, none after it ran.
    StepFailed,
    /// 4: the helper would not act: the arguments, who started it, or where
    /// it runs ([`Event::Refused`] says why).
    Refused,
    /// 5: the system task is already running, from an earlier start.
    AlreadyRunning,
    /// 6: the system task could not be started, or ended without saying
    /// how its steps went ([`Event::TaskFailed`] says what was seen).
    TaskFailed,
}

impl Exit {
    pub const fn code(self) -> i32 {
        match self {
            Self::Done => 0,
            Self::PlanChanged => 2,
            Self::StepFailed => 3,
            Self::Refused => 4,
            Self::AlreadyRunning => 5,
            Self::TaskFailed => 6,
        }
    }

    /// The helper's own codes; `None` for any other (pkexec's 126 and 127, a
    /// panic's 101, a signal).
    pub fn from_code(code: i32) -> Option<Self> {
        [
            Self::Done,
            Self::PlanChanged,
            Self::StepFailed,
            Self::Refused,
            Self::AlreadyRunning,
            Self::TaskFailed,
        ]
        .into_iter()
        .find(|e| e.code() == code)
    }
}

/// Where a line of a step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// The step's program's standard output.
    Stdout,
    /// Its standard error.
    Stderr,
    /// The helper about the step: the command it starts, the file it writes.
    Helper,
}

/// One of root's plans for this machine: the plan for one value of the one
/// flag only the user's side knows (D44).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanVariant {
    /// lmgw's spec generation failed with NVIDIA's toolkit (S4′ offered).
    pub update_toolkit: bool,
    pub sha256: String,
    pub plan: HashedPlan,
    /// Why steps this machine needs could not be planned (the plan then has
    /// none), in plain words.
    pub blocked: Vec<String>,
}

impl PlanVariant {
    pub fn of(p: &Plan, update_toolkit: bool) -> Self {
        let hashed = p.hashed();
        Self {
            update_toolkit,
            sha256: hashed.sha256(),
            plan: hashed,
            blocked: p.blocked.iter().map(ToString::to_string).collect(),
        }
    }
}

/// Root's plans for the machine `inputs` describes, one per value of
/// [`Want::update_toolkit`], each with the [`Plan`] it hashes: their
/// hashes are [`crate::plan::accepted_hashes`].
pub fn variants(inputs: &Inputs, providers: &[Provider]) -> Vec<(PlanVariant, Plan)> {
    [false, true]
        .into_iter()
        .map(|update_toolkit| {
            let p = plan(inputs, Want { update_toolkit }, providers);
            (PlanVariant::of(&p, update_toolkit), p)
        })
        .collect()
}

/// One line the helper prints. `apply`'s lines are its own and, once the
/// system task runs, `run`'s, passed on as they come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first line of `apply` and of `run` once the caller check passed
    /// (a refused caller gets `refused` alone): the helper runs as root by
    /// its own route. `version` is the lmgw release it was built with,
    /// `schema` its [`crate::plan::PLAN_SCHEMA`], `pkexec_uid` (`apply`
    /// only) the user pkexec says asked for it.
    Begun {
        verb: String,
        version: String,
        schema: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pkexec_uid: Option<u32>,
    },
    /// `plan`'s answer: root's plans for this machine.
    Plan {
        schema: u32,
        version: String,
        plans: Vec<PlanVariant>,
    },
    /// The hash asked for (`asked`) is none of root's plans: nothing was
    /// changed (exit 2). Another `schema` or `version` than the app's own
    /// means lmgw was updated while it ran; the same ones mean the computer
    /// changed since the plan was shown.
    PlanChanged {
        asked: String,
        schema: u32,
        version: String,
        plans: Vec<PlanVariant>,
    },
    /// The helper would not act (exit 4).
    Refused { why: String },
    /// The system task runs already (exit 5); `state` is systemd's.
    AlreadyRunning { state: String },
    /// The plan asked for has no steps: nothing was started (exit 0).
    NothingToDo,
    /// `run` matched the plan and starts its steps.
    Started { sha256: String, steps: usize },
    /// Step `step` of `of` (counted from 1) starts.
    StepStarted {
        step: usize,
        of: usize,
        kind: StepKind,
    },
    /// One line of a step's output, whole, as it came.
    Line {
        step: usize,
        kind: StepKind,
        stream: Stream,
        line: String,
    },
    /// The step ended: `exit` 0 when it did what it says; otherwise the
    /// failing program's exit code, the `signal` that ended it, or the
    /// `error` that kept the helper from starting it or writing its file.
    StepEnded {
        step: usize,
        kind: StepKind,
        exit: Option<i32>,
        signal: Option<i32>,
        error: Option<String>,
    },
    /// Something said outside a step's output: systemd-run's own words, a
    /// journal that did not take a line.
    Note { line: String },
    /// `run`'s last line: `exit` 0 when every step ran, 3 when one failed.
    Finished { exit: i32 },
    /// `apply`: the system task could not be started, or ended without
    /// saying how its steps went (exit 6).
    TaskFailed { why: String },
}

impl Event {
    /// The line as the helper prints it (no newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("an Event always serializes")
    }

    /// A line the helper printed, or `None` for one that is not an event.
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }
}
