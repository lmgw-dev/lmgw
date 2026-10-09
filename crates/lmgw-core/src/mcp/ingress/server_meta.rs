//! Whose a `/mcp` tool is (client-apps design §7.6, `lmgw/server`): every
//! tool of a registered server is listed with `_meta["lmgw/server"]`
//! (`lmgw_api_types::mcp_apps::ToolServer`) — the server's label, its name,
//! and the tool's name as the server lists it.
//!
//! An MCP Apps view calls its server's tools by the server's own names. A
//! host maps such a call onto `/mcp` by its server's prefix, which fails for
//! a tool a collision moved to `<server name>__<tool>` (`mcp::names`) or the
//! owner renamed: nothing in the listing said which server the moved name
//! belongs to. The stamp says it for every tool, so a host needs one lookup
//! whether a tool moved or not. Only `_meta` is added, which MCP leaves to
//! the server; lmgw replaces anything a server put under its own key.

use lmgw_api_types::mcp_apps::{ToolServer, SERVER_META};
use serde_json::Value;

use crate::config::Snapshot;
use crate::mcp::Aggregate;

/// Stamp `tool` (a `tools/list` entry under its exposed name) with the
/// server `agg` routes it to; a name `agg` does not route is left as is.
pub(super) fn stamp(tool: &mut Value, agg: &Aggregate, snap: &Snapshot) {
    let Some(exposed) = tool.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some((server_id, upstream)) = agg.reverse.get(exposed) else {
        return;
    };
    let Some(server) = snap.mcp_servers.get(server_id) else {
        return;
    };
    let Ok(stamp) = serde_json::to_value(ToolServer {
        label: crate::mcp::exec::server_label(server),
        name: server.name.clone(),
        tool: upstream.clone(),
    }) else {
        return;
    };
    let Some(entry) = tool.as_object_mut() else {
        return;
    };
    // A `_meta` that is no object (no MCP server's) is passed as it came.
    if let Some(m) = entry
        .entry("_meta")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
    {
        m.insert(SERVER_META.to_string(), stamp);
    }
}
