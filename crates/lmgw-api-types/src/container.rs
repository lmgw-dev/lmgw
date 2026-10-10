//! The `container` op's answers: what `POST /api/op/container` and the
//! `lmgw__container` tool answer for each `action`, addressed to one model
//! (`model`) or to a group of them (`target`).
//!
//! `R` and `V` are the shapes of a runtime row and of the GPU ledger: the
//! gateway fills in its own views, a reader (and the API document) uses
//! [`RuntimeStatus`] and [`VramStatus`].

use serde::{Deserialize, Serialize};

use crate::{RuntimeStatus, VramStatus};

/// Which shape an answer has follows from the address and the action; every
/// shape carries `ok`. The actions that change something add a `message`, a
/// `note` or both (apply); the status and log answers carry neither.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "ContainerAnswer"))]
pub enum ContainerAnswer<R = RuntimeStatus, V = VramStatus> {
    /// A group, `action: apply`.
    GroupApply(GroupApplied),
    /// A group, `action: start`.
    GroupStart(GroupStarted),
    /// A group, `action: restart`.
    GroupRestart(GroupRestarted),
    /// A group, `action: stop`.
    GroupStop(GroupStopped),
    /// A group, `action: status`.
    GroupStatus(GroupStatus<R, V>),
    /// One model, `action: logs`.
    Logs(ModelLogs),
    /// One model, `action: status`.
    Status(ModelStatus<R>),
    /// One model, `action: start`, `stop`, `restart` or `apply`.
    Done(ModelDone),
}

impl<R, V> ContainerAnswer<R, V> {
    /// Whether the action did what it was asked.
    pub fn ok(&self) -> bool {
        match self {
            Self::GroupApply(a) => a.ok,
            Self::GroupStart(a) => a.ok,
            Self::GroupRestart(a) => a.ok,
            Self::GroupStop(a) => a.ok,
            Self::GroupStatus(a) => a.ok,
            Self::Logs(a) => a.ok,
            Self::Status(a) => a.ok,
            Self::Done(a) => a.ok,
        }
    }

    /// The sentence the answer carries, when it has one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::GroupApply(a) => Some(&a.message),
            Self::GroupRestart(a) => Some(&a.message),
            Self::GroupStop(a) => Some(&a.message),
            Self::Done(a) => Some(&a.message),
            Self::GroupStart(_) | Self::GroupStatus(_) | Self::Logs(_) | Self::Status(_) => None,
        }
    }
}

/// A model named by class and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerModel {
    /// `chat`, `aux`, `audio` or `image`.
    pub class: String,
    pub model_id: String,
}

/// A model and the port its container is up on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerPort {
    pub class: String,
    pub model_id: String,
    /// The loopback port the container is published on.
    pub port: u16,
}

/// A model whose container is still serving requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerBusy {
    pub class: String,
    pub model_id: String,
    /// How many requests it is serving.
    pub in_flight: u32,
}

/// A model an action failed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerFailure {
    pub class: String,
    pub model_id: String,
    pub error: String,
}

/// A model with problems found by the static checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerProblems {
    pub class: String,
    pub model_id: String,
    pub issues: Vec<String>,
}

/// A model that starts but misbehaves under some input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerAdvisories {
    pub class: String,
    pub model_id: String,
    pub advisories: Vec<String>,
}

/// One model, `start`, `stop`, `restart` or `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelDone {
    /// False when the model was still serving requests and was left
    /// running, or could not be stopped under the hold.
    pub ok: bool,
    pub class: String,
    pub model_id: String,
    /// The port, when the action left the container up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `apply` only: whether the configuration was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    /// `apply` only: whether the container is running afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    /// `apply` only: the GPU hold kept the container from starting again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<bool>,
    pub message: String,
}

/// One model, `logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelLogs {
    pub ok: bool,
    pub class: String,
    pub model_id: String,
    /// The container's name.
    pub container: String,
    /// How many lines were asked for.
    pub tail: usize,
    /// The container's recent output.
    pub logs: String,
}

/// One model, `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelStatus<R = RuntimeStatus> {
    pub ok: bool,
    pub class: String,
    pub model_id: String,
    /// The engine the class runs: `llama-server`, `audio.cpp` or `sd-server`.
    pub engine: String,
    /// Null when no model with this id is configured.
    pub enabled: Option<bool>,
    pub warm_start: Option<bool>,
    pub idle_seconds: Option<i64>,
    /// The container image in effect.
    pub image: Option<String>,
    /// The running container, or null when nothing has started this model.
    pub runtime: Option<R>,
}

/// A group, `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupStatus<R = RuntimeStatus, V = VramStatus> {
    pub ok: bool,
    /// The group asked about: `all`, `chat`, `aux`, `audio` or `image`.
    pub target: String,
    /// The containers of the group.
    pub runtime: Vec<R>,
    /// The GPU ledger, whole even when one group was asked about.
    pub vram: V,
}

/// A group, `start`: only the models flagged warm_start are started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupStarted {
    pub ok: bool,
    pub target: String,
    pub started: Vec<ContainerPort>,
    pub already_running: Vec<ContainerModel>,
    /// Left alone because the GPU hold is on.
    pub held: Vec<ContainerModel>,
    pub errors: Vec<ContainerFailure>,
    pub note: String,
}

/// A group, `stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupStopped {
    pub ok: bool,
    pub target: String,
    pub stopped: Vec<ContainerModel>,
    /// Still serving requests and left running; `override` forces them.
    pub busy: Vec<ContainerBusy>,
    pub errors: Vec<ContainerFailure>,
    pub message: String,
}

/// A group, `restart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupRestarted {
    pub ok: bool,
    pub target: String,
    pub restarted: Vec<ContainerPort>,
    pub busy: Vec<ContainerBusy>,
    /// Left running untouched because the GPU hold would not let them start
    /// again.
    pub held: Vec<ContainerModel>,
    pub errors: Vec<ContainerFailure>,
    pub message: String,
}

/// A group, `apply`: every running member recreated, and the static checks
/// over every enabled model in scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupApplied {
    pub ok: bool,
    pub target: String,
    /// Stopped and not started again because the GPU hold is on.
    pub held: Vec<ContainerModel>,
    pub models_enabled: usize,
    pub models_with_problems: usize,
    pub problems: Vec<ContainerProblems>,
    pub models_with_advisories: usize,
    pub advisories: Vec<ContainerAdvisories>,
    pub recreated: Vec<ContainerPort>,
    pub busy: Vec<ContainerBusy>,
    pub errors: Vec<ContainerFailure>,
    pub message: String,
    pub note: String,
}
