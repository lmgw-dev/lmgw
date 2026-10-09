//! MCP servers

use serde::{Deserialize, Serialize};

/// `GET /api/mcp-servers` — config plus live status. List-valued fields are
/// newline-joined strings (the ops-plane schema shape); secret env/header
/// values arrive as `<set>` and round-trip safely.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServersResponse {
    pub mcp_servers: Vec<McpServerView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerView {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// `stdio | http | sse | device`. A `device` row is a paired device's
    /// hosted tools (client-apps design §5.2): created by its hosting grant,
    /// connected while the device's host link is open.
    pub transport: String,
    pub command: Option<String>,
    pub args: String,
    /// `KEY=value` lines, as stored — a server's credentials usually live here.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub env: String,
    pub cwd: Option<String>,
    pub container_image: Option<String>,
    pub extra_run_args: String,
    pub url: Option<String>,
    /// `Name: value` lines, as stored — an HTTP server's bearer lives here.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub headers: String,
    pub tool_prefix: String,
    pub timeout_ms: u64,
    pub autostart: bool,
    pub idle_seconds: i64,
    pub allow_sampling: bool,
    pub sampling_alias: Option<String>,
    /// `ready | connecting | error | stopped` at list time; live updates come
    /// from the SSE `mcp` frame.
    pub status: String,
    pub tool_count: i64,
    /// The MCP tasks this server runs for Chat threads now, at list time
    /// (MCP Tasks design §6): started by a call of one of its `required`
    /// tools and not ended yet. No cap (T9); a server with open tasks is
    /// never reaped for idleness (T17).
    pub open_tasks: i64,
    pub status_detail: Option<String>,
    /// A `device` row's key id (`api_keys.id`); `null` for every other row.
    pub device_key_id: Option<i64>,
    /// A `device` row's device, by its short name (`desktop`); `null` for
    /// every other row.
    pub device: Option<String>,
}

/// `GET /api/mcp-servers/{id}/tools` — one server's exposed surface, for the
/// Chat thread picker that narrows which of them a conversation may call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerToolsResponse {
    pub tools: Vec<McpToolView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpToolView {
    /// The exposed (prefixed) name a model sees and a thread stores.
    pub name: String,
    /// The server's own name for it, shown when a prefix hides it.
    pub upstream_name: String,
    pub description: Option<String>,
}
