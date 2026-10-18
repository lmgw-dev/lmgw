//! A labelled set from an agent run (labs-store design §4, Q4): the rows a
//! batch agent's finished apply job received, as a set run not yet run, of
//! the question set that classified them (each row's stamp). Each item's
//! state is the prompt the classify call sent (`Row.prompt`), its form the
//! set restricted to the output field's question (what the agent asks), its
//! expected answer the applied row's output field, the owner's edit
//! included. Resume on the set-run page runs it.

use serde::{Deserialize, Serialize};

use super::batch::BatchView;

/// The mapping's state column of a set run saved from an agent run: the
/// classify call's prompt.
pub const AGENT_STATE_COLUMN: &str = "prompt";

/// The mapping's key column of a set run saved from an agent run: the row's
/// id (a mail's message id), kept per item and carried into both exports.
pub const AGENT_KEY_COLUMN: &str = "id";

/// The source name a set run saved from agent run `job_id` of agent
/// `agent_id` keeps.
pub fn agent_source_name(job_id: i64, agent_id: &str) -> String {
    format!("agent run {job_id} of {agent_id}")
}

/// `POST /decision-lab/api/batches/from-agent`: save a finished apply run's
/// rows as a labelled set run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchFromAgent {
    /// The apply job whose received rows are the labels. Required.
    pub job_id: i64,
    /// An explicit override: the question set the run is of, whatever set
    /// classified the rows. Absent, the set stamped on the rows
    /// (`AgentRowSet`), which must be one set.
    #[serde(default)]
    pub preset_id: Option<i64>,
}

/// Why a row of the apply job is no item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BatchFromAgentSkip {
    /// The model was never asked about it (no id, or its fetch failed): it
    /// has no state to ask again.
    #[default]
    NeverAsked,
    /// Its call failed and its output is still the fallback the failure
    /// gave it: no answer, and no label of the owner's.
    CallFailed,
}

/// A row of the apply job that is no item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchFromAgentSkipped {
    /// The row's id.
    pub id: String,
    pub kind: BatchFromAgentSkip,
    /// Why it is no item, with the row's error.
    pub reason: String,
}

/// What a save from an agent run made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchFromAgentSaved {
    /// The new set run, `not_started`; `preset_name` names its set.
    pub batch: BatchView,
    /// The rows that are no item, each with why.
    pub skipped: Vec<BatchFromAgentSkipped>,
    /// The set was edited since its rows were classified (a row's stamp
    /// names an older `updated_at`): the set run asks the set as it is now.
    pub set_changed: bool,
}
