//! Tool inventory

use serde::{Deserialize, Serialize};

/// `GET /api/tools` — every tool this gateway can serve northbound, from the
/// built-in toolsets and from the registered servers alike, with the
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

/// One source of tools, so a source that currently offers none (a disabled
/// server, self-admin off, a server that has not connected) is still visible
/// with the reason: the per-tool rows cannot carry that.
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
    /// Tools this source contributes to the inventory right now.
    pub tool_count: i64,
    /// How many of those the owner has switched off.
    pub disabled_count: i64,
}

/// One tool the gateway can serve, with its provenance and current state.
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
    /// The name it would have if no other source offered a tool of that
    /// name: set when a collision gave it its server's name as a prefix.
    pub moved_from: Option<String>,
    /// Why it moved: who else claims [`Self::moved_from`], and whether that
    /// claim is only a server's last listing (it is not connected now).
    pub moved_reason: Option<String>,
    /// The per-tool switch (`POST /api/op/tool_set`).
    pub enabled: bool,
    /// Offered right now: `enabled` **and** its source's condition holds.
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<String>,
    /// A disable record whose tool the gateway no longer offers.
    pub stale: bool,
    pub disabled_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inventory with nothing in it is two empty arrays, which the reader
    /// takes for the same inventory.
    #[test]
    fn an_empty_inventory_is_two_empty_arrays() {
        let empty = serde_json::json!({"sources": [], "tools": []});
        assert_eq!(
            serde_json::to_value(ToolInventory::default()).unwrap(),
            empty
        );
        let read: ToolInventory = serde_json::from_value(empty).unwrap();
        assert_eq!(read, ToolInventory::default());
    }
}
