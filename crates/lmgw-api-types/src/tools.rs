//! Tool inventory

use serde::{Deserialize, Serialize};

/// `GET /api/tools` — every tool this gateway can serve northbound, from the
/// built-in toolsets and from the registered servers alike, with the owner's
/// per-tool switch and the reason a tool is not currently offered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolInventory {
    /// Group headers. A source with `tool_count: 0` is still here, carrying the
    /// reason it contributes nothing.
    pub sources: Vec<ToolSourceView>,
    pub tools: Vec<ToolEntryView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolSourceView {
    /// The label a thread or a `{"type":"mcp"}` block attaches: `lmgw`, `docs`,
    /// or a server's prefix/name.
    pub label: String,
    pub name: String,
    /// `builtin | server`.
    pub kind: String,
    pub server_id: Option<i64>,
    /// `/mcp` or `/mcp/admin`.
    pub plane: String,
    pub available: bool,
    pub reason: Option<String>,
    pub tool_count: i64,
    pub disabled_count: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolEntryView {
    /// Fully-qualified exposed name — the key the switch is stored under.
    pub name: String,
    pub source_label: String,
    pub source_name: String,
    /// `builtin | server`.
    pub source_kind: String,
    pub server_id: Option<i64>,
    pub plane: String,
    pub description: Option<String>,
    pub upstream_name: Option<String>,
    /// The owner's switch (`POST /api/op/tool_set`).
    pub enabled: bool,
    /// Offered right now: `enabled` **and** its source's condition holds.
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<String>,
    /// A disable record whose tool the gateway no longer offers.
    pub stale: bool,
    pub disabled_at: Option<String>,
}
