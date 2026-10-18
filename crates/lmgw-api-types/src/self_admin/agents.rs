//! `lmgw__agents`.
//!
//! The other five agent tools answer the dashboard's own types: `lmgw__agent_get`
//! an [`AgentDetail`](crate::AgentDetail), `lmgw__agent_delete` an
//! [`AgentDeleted`](crate::AgentDeleted), and `lmgw__agent_set`,
//! `lmgw__agent_install` and `lmgw__agent_run` the answer of the op of the same
//! name with a `next_step` beside it ([`WithNextStep`](crate::WithNextStep)).

use serde::{Deserialize, Serialize};

/// `lmgw__agents`: the catalog, trimmed to what a model needs to decide what to
/// call next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentList {
    pub agents: Vec<AgentListRow>,
    /// `agents.len()`.
    pub count: usize,
    /// What to call next.
    pub next_step: String,
}

/// One agent of `lmgw__agents`: the dashboard's card, cut down.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentListRow {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `chat`, `batch` or `container`.
    pub kind: String,
    /// The model alias the stored config resolves the manifest's to; absent
    /// while the config has not filled in a `{{config.model}}` template.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub model: Option<String>,
    pub enabled: bool,
    /// `builtin`, `imported` or `authored`.
    pub source: String,
    /// The tool labels the agent attaches, in manifest order.
    pub tools: Vec<String>,
    /// Every label resolves and every narrowed tool name is currently listed.
    pub requires_ok: bool,
    /// Why the stored manifest could not be read; `null` when it could.
    pub error: Option<String>,
}
