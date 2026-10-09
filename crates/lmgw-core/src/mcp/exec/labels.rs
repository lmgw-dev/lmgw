//! One label's tools as a client is shown them, and the labels a caller may
//! name (realtime-server-tools design §1.2–§1.4).
//!
//! [`list_label`] is the one path from a `{type: "mcp", server_label}` entry
//! to the tools a client sees: `/v1/realtime` writes them into the label's
//! `mcp_list_tools` item, and `GET /v1/mcp/servers/{label}` answers them, so
//! the two can never disagree. It is [`resolve`] with that entry alone — the
//! caller's scope, the owner's per-tool switch, the targeted connect.
//!
//! **Wire names** (§1.3) are the exposed names minus their namespace: a
//! built-in toolset's label, or the registered server's own tool prefix, read
//! from the server's record — or, for a tool a collision gave its server's
//! prefix (`mcp::names`), the tool's own name. Never by splitting on `__`: a
//! bare server may expose a literal `x__y`, and its exposed name is already
//! the server's own.
//!
//! [`labels`] says what a caller could name **without connecting anything**
//! (§1.4): the built-in toolsets [`resolve`] would answer for it, and the
//! enabled registered servers and service agents it may be told about
//! (`shown_to`) — the same rule that says which labels are available when
//! one is not, so the list, the detail's 404 and that message agree.

use serde::Serialize;

use crate::config::{McpServer, Snapshot};
use crate::ir::ToolDef;
use crate::state::SharedState;

use super::super::scope::ToolScope;
use super::super::spec::{ApprovalRule, McpToolSpec};
use super::super::Aggregate;
use super::{
    builtin_label, find_server, resolve, resolve_builtin, server_label, DOCS_LABEL, KB_LABEL,
    SELF_ADMIN_LABEL,
};

/// One tool of a label: as the model is offered it, and as the wire names it.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelTool {
    /// The definition the model is offered, under its exposed name — unique
    /// across servers.
    pub def: ToolDef,
    /// The exposed name minus its namespace (§1.3).
    pub wire: String,
}

impl LabelTool {
    /// Always a string, `""` when the server gave none: `@openai/agents`
    /// drops a whole `mcp_list_tools` event over a `null`.
    pub fn description(&self) -> &str {
        self.def.description.as_deref().unwrap_or_default()
    }
}

/// What one label resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelTools {
    pub tools: Vec<LabelTool>,
    /// The exposed names a built-in toolset serves (`Resolved::builtin`):
    /// what a run's executor routes in-process.
    pub builtin: Vec<String>,
    /// The registered server the label names, by id; `None` for a built-in
    /// toolset. Its tools are called on it, or not at all
    /// (`McpManager::call_listed`).
    pub server: Option<i64>,
}

/// Why a label has no tools for this caller. Either way the message is the
/// resolver's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelError {
    /// Not a label this caller may use: unknown, disabled, out of its
    /// scope, or a built-in toolset that answers it nothing. An unknown
    /// label's message names the available ones.
    Unavailable(String),
    /// A server this caller may use that could not be listed: not reachable,
    /// failing, or offering nothing it may have.
    Unlisted(String),
}

impl LabelError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(m) | Self::Unlisted(m) => m,
        }
    }
}

/// The tools `spec` resolves to for `scope`, with their wire names. Connects
/// the one server it names, if any, within the lazy-list budget.
pub async fn list_label(
    state: &SharedState,
    spec: &McpToolSpec,
    scope: &ToolScope,
) -> Result<LabelTools, LabelError> {
    let resolved = resolve(state, std::slice::from_ref(spec), scope).await;
    let snap = state.snapshot();
    let label = spec.server_label.as_str();
    let server = find_server(&snap, label).filter(|_| builtin_label(label).is_none());
    if let Some((_, why)) = resolved.failed.into_iter().next() {
        // Judged on what the connect left listed: a server whose every tool
        // this caller's scope keeps out is not one it may use.
        let agg = state.mcp.aggregate(&snap).await;
        let usable =
            server.is_some_and(|s| s.enabled && scope.may_reach(s) && shown_to(s, &agg, scope));
        return Err(if usable {
            LabelError::Unlisted(why)
        } else {
            LabelError::Unavailable(why)
        });
    }
    let Some((_, defs)) = resolved.listed.into_iter().next() else {
        // `resolve` reports every entry it is given, one way or the other.
        return Err(LabelError::Unlisted(format!(
            "'{label}' resolved to neither tools nor an error"
        )));
    };
    // The namespace the exposed names carry: the toolset's label, or the
    // server's own prefix — none for a bare server.
    let namespace = match builtin_label(label) {
        Some(b) => Some(b.to_string()),
        None => server
            .map(|s| s.tool_prefix.trim().to_string())
            .filter(|p| !p.is_empty()),
    };
    // A tool a collision gave its server's prefix goes by its own name on
    // the wire, as every other does.
    let tools = defs
        .into_iter()
        .map(|def| LabelTool {
            wire: match resolved.moved.get(&def.name) {
                Some(own) => own.clone(),
                None => wire_name(namespace.as_deref(), &def.name).to_string(),
            },
            def,
        })
        .collect();
    Ok(LabelTools {
        tools,
        builtin: resolved.builtin,
        server: server.map(|s| s.id),
    })
}

/// `exposed` minus `<namespace>__`, or as it is when it carries none.
fn wire_name<'a>(namespace: Option<&str>, exposed: &'a str) -> &'a str {
    namespace
        .and_then(|ns| exposed.strip_prefix(ns))
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(exposed)
}

/// What a label is (§1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelKind {
    /// One of lmgw's own toolsets: `lmgw`, `docs`, `kb`.
    Builtin,
    /// A registered MCP server.
    Server,
    /// A service agent's tools, registered by the agent.
    Agent,
}

/// One label a caller may name, as `GET /v1/mcp/servers` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEntry {
    /// What goes into `{type: "mcp", server_label}`: the label [`resolve`]
    /// matches first — a toolset's own, a server's tool prefix, else its
    /// name.
    pub server_label: String,
    pub kind: LabelKind,
    /// A server's configured name; a toolset's label.
    pub name: String,
    /// A toolset's one-line purpose; `""` for a server, whose config has no
    /// such field.
    pub description: String,
}

/// The built-in toolsets in [`labels`]' order, with their purpose.
const BUILTINS: [(&str, &str); 3] = [
    (
        SELF_ADMIN_LABEL,
        "lmgw's own state and configuration: models, upstreams, MCP servers, settings and usage \
         (self-admin: the owner, and devices the owner allowed lmgw's admin tools)",
    ),
    (
        DOCS_LABEL,
        "Versioned library documentation kept by lmgw: resolve a library, then query its docs",
    ),
    (
        KB_LABEL,
        "The owner's knowledge bases: list, search and read them",
    ),
];

/// Every label `scope` may name, connecting nothing: the built-in toolsets
/// that would resolve for it — `lmgw` only for a caller that may use the
/// admin tools (an owner credential, or a device the owner allowed them) and
/// while the self-admin mode lists any of them for it — then the enabled
/// registered servers
/// and service agents it may be told about as `agg` stands (`shown_to`),
/// by label.
pub fn labels(snap: &Snapshot, agg: &Aggregate, scope: &ToolScope) -> Vec<LabelEntry> {
    let mut out: Vec<LabelEntry> = BUILTINS
        .iter()
        .filter(|(label, _)| builtin_resolves(snap, label, scope))
        .map(|(label, purpose)| builtin_entry(label, purpose))
        .collect();
    let mut servers: Vec<LabelEntry> = snap
        .mcp_servers
        .values()
        .filter(|s| s.enabled && scope.may_reach(s) && shown_to(s, agg, scope))
        .map(server_entry)
        .collect();
    servers.sort_by(|a, b| a.server_label.cmp(&b.server_label));
    out.extend(servers);
    out
}

/// Whether `scope` may be told about the server `s`, as far as `agg` shows
/// (realtime-server-tools §1.4, final review #3): any server, for a caller
/// with no list of its own — unless `s` is a device row it does not reach
/// whole (`ToolScope::narrows_for`); else one the aggregate shows a tool of that the
/// list admits — or, with nothing of it listed now, one whose namespace the
/// list can reach. Never a bare server with nothing listed: it has no
/// namespace to read (`ToolScope::may_reach` calls it only worth asking
/// about), so a scoped caller is not told of it until a tool of it is one
/// the caller may use.
pub(crate) fn shown_to(s: &McpServer, agg: &Aggregate, scope: &ToolScope) -> bool {
    if !scope.narrows_for(s) {
        return true;
    }
    let exposed: Vec<&String> = agg
        .reverse
        .iter()
        .filter(|(_, (id, _))| *id == s.id)
        .map(|(name, _)| name)
        .collect();
    exposed.iter().any(|name| scope.admits(name))
        || (exposed.is_empty() && !s.tool_prefix.trim().is_empty() && scope.may_reach(s))
}

/// `label`'s entry as [`labels`] lists it — also when it addresses a server
/// by name — or `None` for one `scope` may not name.
pub fn label_entry(snap: &Snapshot, label: &str, scope: &ToolScope) -> Option<LabelEntry> {
    if let Some(builtin) = builtin_label(label) {
        let (_, purpose) = BUILTINS.iter().find(|(l, _)| *l == builtin)?;
        return builtin_resolves(snap, builtin, scope).then(|| builtin_entry(builtin, purpose));
    }
    find_server(snap, label)
        .filter(|s| s.enabled && scope.may_reach(s))
        .map(server_entry)
}

/// Whether [`resolve`] would answer the built-in `label` with tools for
/// `scope` — decided in-process, as `resolve` does.
fn builtin_resolves(snap: &Snapshot, label: &'static str, scope: &ToolScope) -> bool {
    let all = McpToolSpec {
        server_label: label.to_string(),
        allowed_tools: None,
        require_approval: ApprovalRule::Never,
    };
    resolve_builtin(snap, label, &all, scope).is_ok()
}

fn builtin_entry(label: &str, purpose: &str) -> LabelEntry {
    LabelEntry {
        server_label: label.to_string(),
        kind: LabelKind::Builtin,
        name: label.to_string(),
        description: purpose.to_string(),
    }
}

fn server_entry(s: &McpServer) -> LabelEntry {
    LabelEntry {
        server_label: server_label(s),
        kind: if s.agent_id.is_some() {
            LabelKind::Agent
        } else {
            LabelKind::Server
        },
        name: s.name.clone(),
        description: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::wire_name;

    #[test]
    fn a_wire_name_drops_its_namespace_and_nothing_else() {
        assert_eq!(wire_name(Some("docs"), "docs__query"), "query");
        assert_eq!(wire_name(Some("ha"), "ha__lights__on"), "lights__on");
        // A bare server's name is its own, `__` and all.
        assert_eq!(wire_name(None, "x__y"), "x__y");
        // A name outside the namespace (a rename, a race with a prefix
        // change) is left as it is rather than cut somewhere else.
        assert_eq!(wire_name(Some("ha"), "hab__x"), "hab__x");
        assert_eq!(wire_name(Some("ha"), "other"), "other");
    }
}
