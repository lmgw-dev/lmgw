//! MCP Apps on lmgw (client-apps design §7): the extension revision lmgw
//! follows, its identifiers, and the Chat `tool` frames' fields an MCP Apps
//! host reads — `ready` (show the call's view before it runs) and `result`.
//!
//! **The revision**: SEP-1865 "MCP Apps", `modelcontextprotocol/ext-apps`
//! `specification/2026-01-26/apps.mdx`, Stable ([`REVISION`]). `/mcp`
//! advertises [`EXTENSION`] with `mimeTypes` holding [`MIME_TYPE`], serves
//! `resources/list`, `resources/templates/list` and `resources/read`, and
//! namespaces a prefixed server's resource URIs before the authority —
//! `ui://weather/card` from tool prefix `p` is `ui://p__weather/card` — in
//! all of them, in tools' `_meta.ui.resourceUri` and in the resource links
//! and embedded resources of tool results. A host reads a view's resource
//! with `resources/read` on `/mcp` under its own key, which must reach the
//! server (its tool scope; a device-hosted label only when named).
//!
//! What the extension leaves to the host stays the host's: refusing a
//! view's call of a tool whose `visibility` lacks `"app"`, and of another
//! server's app-only tool; enforcing a resource's `_meta.ui.csp` and
//! `permissions`. `/mcp` lists every tool with its `_meta` so a host can —
//! the app-only ones to a session whose `initialize` declared
//! [`EXTENSION`], and to no other.
//!
//! **Whose a listed tool is** ([`SERVER_META`], [`ToolServer`]): `/mcp`'s
//! `tools/list` stamps every tool of a registered server with
//! `_meta["lmgw/server"]` — the server's label, its name, and the tool's
//! name as the server itself lists it. A view calls its server's tools by
//! the server's own names; a host routes such a call to the listed tool of
//! that server with that `tool`, whatever name `/mcp` exposes it under (its
//! prefix, the server's name when a collision moved it, the owner's
//! rename). lmgw's own toolsets (`docs__`, `kb__`) carry none.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MCP Apps specification revision lmgw follows.
pub const REVISION: &str = "2026-01-26";

/// The extension's identifier in `capabilities.extensions`.
pub const EXTENSION: &str = "io.modelcontextprotocol/ui";

/// A UI resource's MIME type.
pub const MIME_TYPE: &str = "text/html;profile=mcp-app";

/// The `_meta` key of a `/mcp` tool naming the server it belongs to
/// ([`ToolServer`]).
pub const SERVER_META: &str = "lmgw/server";

/// `_meta["lmgw/server"]` of a tool `/mcp` lists (module doc): set by lmgw
/// on every tool of a registered server, replacing anything the server put
/// under that key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolServer {
    /// The server's label: its tool prefix, else its name — what the Chat's
    /// `tool` frames carry as `server_label`. Two servers that share a
    /// prefix share it.
    pub label: String,
    /// The server's name, unique among the gateway's servers.
    pub name: String,
    /// The tool's name as the server itself lists it: the name its views
    /// call it by.
    pub tool: String,
}

impl ToolServer {
    /// The stamp of a `tools/list` entry, when it has one.
    pub fn of_tool(tool: &Value) -> Option<Self> {
        serde_json::from_value(tool.get("_meta")?.get(SERVER_META)?.clone()).ok()
    }
}

/// A Chat `tool` frame whose `event` is `ready` (also the `data` of a bound
/// realtime session's `lmgw.chat.frame` for it): a call's arguments are
/// complete, before it runs — what a host that shows the call's view at once
/// needs (client-apps design §7.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolReadyFrame {
    /// The call's position in its turn.
    pub index: usize,
    /// The tool's exposed name, as the model called it.
    pub name: String,
    /// The call's arguments.
    pub arguments: Value,
    /// The call's id, as its `result` frame carries it.
    #[serde(default)]
    pub call_id: Option<String>,
    /// The label the tool came from; `null` for a client's own function.
    #[serde(default)]
    pub server_label: Option<String>,
    /// The call waits for an approval and does not run now: an `approval`
    /// frame follows. `false` for a call that runs (a resumed turn's decided
    /// calls included). Absent from an lmgw before 2026-10-09.
    #[serde(default)]
    pub needs_approval: Option<bool>,
    /// The UI resource the tool links to, as on the result frame.
    #[serde(default)]
    pub ui_resource: Option<String>,
}

/// A Chat `tool` frame whose `event` is `result` (also the `data` of a
/// bound realtime session's `lmgw.chat.frame` for it), as an MCP Apps host
/// reads it. MCP's `CallToolResult` for the view is `{content,
/// structuredContent: structured_content, isError: is_error}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolResultFrame {
    /// The call's position in its turn.
    pub index: usize,
    /// The tool's exposed name (`<prefix>__<tool>`).
    pub name: String,
    /// The result as text.
    pub output: String,
    pub is_error: bool,
    pub ms: u64,
    /// The label the tool came from: a server's tool prefix (else its
    /// name), or a built-in toolset's (`lmgw`, `docs`, `kb`).
    #[serde(default)]
    pub server_label: Option<String>,
    /// The UI resource the tool links to (`_meta.ui.resourceUri`),
    /// namespaced as `/mcp` serves it: `resources/read` it there.
    #[serde(default)]
    pub ui_resource: Option<String>,
    /// The MCP result's `structuredContent`, for the view. The model was
    /// given it only when `content` is empty.
    #[serde(default)]
    pub structured_content: Option<Value>,
    /// The MCP result's `content` blocks as the server sent them (resource
    /// URIs namespaced as `/mcp` sends them; images in full — no bound of
    /// lmgw's own, a device's result is bounded by `mcp.host_max_message_mb`
    /// on its link); a tool lmgw runs itself, or a call that never reached
    /// a server, says its result in the same shape. Absent from an lmgw
    /// before 2026-10-09: `output` as one text block then.
    #[serde(default)]
    pub content: Option<Vec<Value>>,
    /// The call's id, as its `ready` frame carries it.
    #[serde(default)]
    pub call_id: Option<String>,
    /// For a call that started an MCP task: which. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<ResultTask>,
}

/// The MCP task a call started, on its `result` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResultTask {
    /// lmgw's id of the task (`ThreadTask.id`).
    pub id: i64,
    /// The server's id of the task.
    pub task_id: String,
    pub server_label: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_result_frame_reads_with_and_without_the_apps_fields() {
        let f: ToolResultFrame = serde_json::from_value(json!({
            "event": "result", "index": 0, "name": "wx__show", "output": "sunny",
            "is_error": false, "ms": 12, "server_label": "wx",
            "ui_resource": "ui://wx__weather/card", "structured_content": {"temp_c": 21}
        }))
        .unwrap();
        assert_eq!(f.ui_resource.as_deref(), Some("ui://wx__weather/card"));
        assert_eq!(f.structured_content, Some(json!({"temp_c": 21})));
        let old: ToolResultFrame = serde_json::from_value(json!({
            "event": "result", "index": 1, "name": "x", "output": "", "is_error": true, "ms": 0
        }))
        .unwrap();
        assert_eq!(old.server_label, None);
        assert_eq!(old.ui_resource, None);
        assert_eq!(old.content, None);
    }

    #[test]
    fn a_ready_and_a_result_frame_read_with_the_fields_for_a_view() {
        let r: ToolReadyFrame = serde_json::from_value(json!({
            "event": "ready", "index": 0, "name": "wx__show", "arguments": {"city": "Berlin"},
            "call_id": "c1", "server_label": "wx", "needs_approval": false,
            "ui_resource": "ui://wx__weather/card"
        }))
        .unwrap();
        assert_eq!(r.needs_approval, Some(false));
        assert_eq!(r.call_id.as_deref(), Some("c1"));
        let old: ToolReadyFrame = serde_json::from_value(json!({
            "event": "ready", "index": 0, "name": "x", "arguments": {}
        }))
        .unwrap();
        assert_eq!(old.needs_approval, None);
        let f: ToolResultFrame = serde_json::from_value(json!({
            "event": "result", "index": 0, "name": "wx__show", "output": "sunny",
            "is_error": false, "ms": 3, "call_id": "c1",
            "content": [{"type": "text", "text": "sunny"},
                        {"type": "image", "data": "AAAA", "mimeType": "image/png"}],
            "structured_content": {"temp_c": 21}
        }))
        .unwrap();
        assert_eq!(f.content.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn a_listed_tool_says_its_server() {
        let t = json!({"name": "alpha__lookup", "inputSchema": {"type": "object"},
            "_meta": {"lmgw/server": {"label": "alpha", "name": "alpha", "tool": "lookup"}}});
        assert_eq!(
            ToolServer::of_tool(&t),
            Some(ToolServer {
                label: "alpha".into(),
                name: "alpha".into(),
                tool: "lookup".into()
            })
        );
        assert_eq!(ToolServer::of_tool(&json!({"name": "docs__search"})), None);
    }
}
