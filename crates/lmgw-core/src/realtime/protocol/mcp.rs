//! Server-side MCP tools on the wire (realtime-server-tools design §1, §2):
//! the session's `mcp` tool, `tool_choice` naming one, and the `mcp_call` /
//! `mcp_list_tools` items.
//!
//! **Labels, not URLs** (decision 2). `server_url`, `connector_id`,
//! `headers`, `authorization` and `server_description` are accepted and
//! echoed, and nothing reads them: lmgw never dials a URL a client names. The
//! two secrets among them are replaced by [`REDACTED`] as the tool is parsed,
//! so the session never holds them and no echo can carry them (§1.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What `authorization` and every `headers` value echo as.
pub const REDACTED: &str = "[redacted]";

/// `{type: "mcp", server_label, …}`: a registered MCP server or one of the
/// built-in toolsets (`lmgw`, `docs`, `kb`), named by its label.
///
/// `allowed_tools` and `require_approval` are kept as the client wrote them,
/// for the echo; what they mean is the shared parser's
/// (`crate::mcp::spec`), and the session checks them when the tool arrives
/// (`realtime::mcp_tools`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub server_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<String>,
    /// Header names as sent, every value [`REDACTED`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// [`REDACTED`] when the client sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_description: Option<String>,
    /// A list of names or `{tool_names: […]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Value>,
    /// `"never"`, `"always"` or `{never: {tool_names}, always: {tool_names}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<Value>,
}

impl McpTool {
    /// The tool with its secrets replaced, as the session keeps it.
    pub fn redacted(mut self) -> Self {
        if self.authorization.is_some() {
            self.authorization = Some(REDACTED.into());
        }
        if let Some(headers) = &mut self.headers {
            headers.values_mut().for_each(|v| *v = REDACTED.to_string());
        }
        self
    }
}

/// `tool_choice: {type: "mcp", server_label, name?}` (§1.1): one tool of a
/// label, or — without `name` — any of that label's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpChoice {
    #[serde(rename = "type")]
    pub kind: McpTag,
    pub server_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The `"mcp"` literal of [`McpChoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTag {
    Mcp,
}

/// A server-side tool call (§2.2): the model called a tool of an `mcp`
/// label, and the gateway ran it.
///
/// **Every field but `id` is always written**, `null` where empty (§1.3):
/// `@openai/agents` validates the item inside a listener with no try/catch,
/// so a missing `output` throws there. The `skip_serializing_if` the other
/// items use must not reach these fields.
///
/// It has no `call_id` and no `status`: the session keeps the call's id for
/// rendering, and its state (made, running, done) in the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCallItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The label as the client wrote it in its tools (§1.3).
    pub server_label: String,
    /// The tool's own name: the exposed name minus its `<prefix>__` (§1.3).
    pub name: String,
    /// The arguments as the model streamed them: a JSON *string*.
    #[serde(default)]
    pub arguments: String,
    /// Always `null` until approvals exist (§6).
    #[serde(default)]
    pub approval_request_id: Option<String>,
    /// The result as text; `null` while the call runs, or when it failed.
    #[serde(default)]
    pub output: Option<String>,
    /// Why the call failed; `null` otherwise.
    #[serde(default)]
    pub error: Option<McpCallError>,
}

/// An `mcp_call`'s `error`. The gateway writes `tool_execution_error`; the
/// other two are the API's, accepted on a client's replayed item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallError {
    #[serde(rename = "type")]
    pub kind: McpErrorKind,
    /// The status or JSON-RPC code of a `protocol_error` / `http_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpErrorKind {
    ToolExecutionError,
    ProtocolError,
    HttpError,
}

/// One label's listing (§1.2): what the session offers of it. The server's
/// own record — a client cannot create one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListToolsItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub server_label: String,
    /// Empty until the listing completes.
    #[serde(default)]
    pub tools: Vec<McpListedTool>,
}

/// One tool of an `mcp_list_tools` item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListedTool {
    /// The wire name (§1.3).
    pub name: String,
    /// Always a string, `""` when the server gave none: `@openai/agents`
    /// drops the whole event over a `null`.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
    /// Always `null`: lmgw does not keep MCP tool annotations (decision 8).
    #[serde(default)]
    pub annotations: Option<Value>,
}

/// `mcp_approval_request` (client-apps design §6.4): a call a session bound
/// to a chat thread waits on, as OpenAI's item carries it. The server's own
/// item — a client cannot create one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApprovalRequestItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub server_label: String,
    /// The tool's own name (decision 6).
    pub name: String,
    /// A JSON string.
    #[serde(default)]
    pub arguments: String,
}

/// `mcp_approval_response`: a client's decision on an
/// `mcp_approval_request`, taken on a bound session only (§6.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApprovalResponseItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub approval_request_id: String,
    pub approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
