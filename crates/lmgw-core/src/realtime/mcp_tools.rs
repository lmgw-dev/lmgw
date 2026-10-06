//! A session's `mcp` tools (realtime-server-tools design §1): as they arrive
//! (§1.1) — the label-only spelling that reuses an earlier definition, and the
//! rules a `session.update` or a `response.create` is refused by — and once
//! they are listed (`listing`, §1.2) into the session's tool table
//! (`table`, §1.2, §1.3), which a response's tools and `tool_choice` are
//! looked up in, and what a response runs of them (`served`, §2.1, §2.4).
//!
//! `allowed_tools` and `require_approval` go through the parser
//! `/v1/responses` uses (`mcp::spec`), so `read_only` is refused on both
//! routes alike. Beyond it, two rules are this route's own: one entry per
//! `server_label` in a `tools` array (OpenAI's rule), and no
//! `require_approval` that could gate a tool — approvals are not built here
//! yet (decision 4). Each is an `invalid_value` naming the entry, and a
//! refused update changes nothing.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::protocol::{ErrorObject, McpTool, Session, Tool};
use crate::mcp::spec::{self, McpToolSpec};

mod listing;
mod served;
mod table;

pub(crate) use listing::McpSession;
pub(crate) use table::{McpOffer, McpTable, Moment, Owner};

/// A `{type: "mcp", server_label}` entry with nothing else in it takes the
/// session's earlier definition of that label, filters and all, as OpenAI's
/// guide allows. Applied to the update before the merge, whose arrays
/// replace. A label the session never defined stays as written: all of its
/// tools, no gate.
pub fn reuse_definitions(update: &mut Map<String, Value>, current: &Session) {
    let Some(Value::Array(tools)) = update.get_mut("tools") else {
        return;
    };
    for entry in tools.iter_mut() {
        let Some(label) = label_only(entry) else {
            continue;
        };
        let earlier = current
            .tools
            .iter()
            .flatten()
            .filter_map(Tool::as_mcp)
            .find(|t| t.server_label == label);
        if let Some(Ok(v)) = earlier.map(|t| serde_json::to_value(Tool::Mcp(t.clone()))) {
            *entry = v;
        }
    }
}

/// The label of an entry that is `type` and `server_label` alone.
fn label_only(entry: &Value) -> Option<String> {
    let o = entry.as_object()?;
    let alone = o.get("type").and_then(Value::as_str) == Some("mcp")
        && o.keys().all(|k| k == "type" || k == "server_label");
    alone
        .then(|| o.get("server_label").and_then(Value::as_str))
        .flatten()
        .map(str::to_string)
}

/// The `mcp` entries of one `tools` array — `param` names it
/// (`session.tools`, `response.tools`).
pub fn check(tools: &[Tool], param: &str) -> Result<(), ErrorObject> {
    let mut labels: HashSet<&str> = HashSet::new();
    for (n, tool) in tools.iter().enumerate() {
        let Some(t) = tool.as_mcp() else {
            continue;
        };
        let at = format!("{param}[{n}]");
        let label = t.server_label.as_str();
        if !labels.insert(label) {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!("{param} names the MCP server_label '{label}' twice; one entry per label"),
            )
            .with_param(format!("{at}.server_label")));
        }
        let spec = spec_of(t).map_err(|(field, why)| {
            ErrorObject::invalid("invalid_value", why).with_param(format!("{at}.{field}"))
        })?;
        if !spec.require_approval.gates_nothing() {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!(
                    "require_approval on mcp server '{label}' would gate a tool behind an \
                     approval, and approvals are not built on /v1/realtime yet: send \"never\" \
                     or leave it out (an object form is taken only when it gates no tool)"
                ),
            )
            .with_param(format!("{at}.require_approval")));
        }
    }
    Ok(())
}

/// What the session resolves an `mcp` tool to, by the shared parser — or
/// the field it refused and why.
pub fn spec_of(t: &McpTool) -> Result<McpToolSpec, (&'static str, String)> {
    let label = t.server_label.as_str();
    Ok(McpToolSpec {
        allowed_tools: spec::parse_allowed_tools(t.allowed_tools.as_ref(), label)
            .map_err(|e| ("allowed_tools", e))?,
        require_approval: spec::parse_require_approval(t.require_approval.as_ref(), label)
            .map_err(|e| ("require_approval", e))?,
        server_label: t.server_label.clone(),
    })
}
