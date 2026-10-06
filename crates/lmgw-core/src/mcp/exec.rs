//! MCP tools as a [`ToolExecutor`] for the `/v1/responses` loop (§21).
//!
//! The northbound `/mcp` endpoint hands tools *out* to an agent; this hands the
//! same tools to a loop running **inside** the gateway. Both go through
//! [`McpManager`], so lazy connect, per-server timeouts, idle reaping and the
//! `request_logs` row are identical — a tool call made on the model's behalf is
//! as visible in the Logs tab as one an external agent made.
//!
//! ## Labels, not URLs
//!
//! OpenAI's `{"type":"mcp"}` tool names a `server_url` for its servers to dial.
//! lmgw resolves `server_label` against its own registered servers instead:
//! they are already connected, Podman-isolated, prefixed and reaped, and having
//! the gateway dial an arbitrary URL on a client's say-so would hand any
//! `/v1/responses` caller an outbound request primitive.
//!
//! ## The built-in toolsets are labels too, and opt-in
//!
//! [`resolve`] also answers three reserved labels — `lmgw` (self-admin),
//! `docs` (quickdoc) and `kb` (knowledge bases) — so a thread or a `/v1/responses` client can attach a
//! built-in toolset exactly like a registered server, with the same per-tool
//! `allowed_tools` narrowing. They are **never** attached implicitly: a caller
//! that does not name the label gets none of those tools, which is what keeps a
//! coding agent's ordinary request from carrying the gateway's configuration
//! API.
//!
//! `lmgw` stays bound by the [`SelfAdmin`](crate::config::SelfAdmin) mode gate
//! wherever it is reached from — attaching the label cannot escalate past the
//! Setting, and a refusal says so. It is also bound by **who asks**: a
//! `/v1/responses` caller attaches it only with an owner credential, the same
//! one the `/mcp/admin` route asks for ([`ToolScope`]).
//!
//! ## The caller's scope
//!
//! Every label resolves through the caller's [`ToolScope`] — a client key's
//! tool scope, an agent token's manifest — so a name outside it is neither
//! offered to the model nor executed. The gateway's own runs pass
//! [`ToolScope::gateway`].

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{ResolvedTool, ToolExecutor, ToolOutcome};
use crate::config::Snapshot;
use crate::ir::{ToolDef, ToolResultBlock};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

use super::kb::KbAccess;
use super::scope::ToolScope;
use super::spec::McpToolSpec;
use super::{docs, kb, selfadmin};

mod labels;
mod served_by;

use labels::shown_to;
pub use labels::{
    label_entry, labels, list_label, LabelEntry, LabelError, LabelKind, LabelTool, LabelTools,
};
pub(crate) use served_by::server_of;

/// The label a client uses to name a registered server in a `{"type":"mcp"}`
/// tool: its tool prefix, or its name when it has none.
///
/// The prefix is the right default because it is already the visible namespace
/// — the tools appear as `<prefix>__<tool>` on `/mcp`, so the label a client
/// would guess is the one it has seen.
pub fn server_label(s: &crate::config::McpServer) -> String {
    match s.tool_prefix.trim() {
        "" => s.name.clone(),
        p => p.to_string(),
    }
}

/// Tools resolved for one run, plus what to report per server.
pub struct Resolved {
    pub tools: Vec<ResolvedTool>,
    /// `(label, tools)` per successfully listed server → `mcp_list_tools`.
    pub listed: Vec<(String, Vec<ToolDef>)>,
    /// `(label, error)` for a server that could not be resolved or listed.
    pub failed: Vec<(String, String)>,
    /// Names resolved from a **built-in** toolset. The run's executor routes
    /// exactly these in-process; anything else goes to the southbound manager.
    /// An explicit set rather than a prefix test, so a run that never attached
    /// `lmgw` cannot reach the self-admin plane by naming one of its tools.
    pub builtin: Vec<String>,
    /// Names resolved from a registered server, and its id: a call of one
    /// runs on that server or not at all ([`McpExecutor::with_listed`]).
    pub servers: HashMap<String, i64>,
}

/// Resolve the `{"type":"mcp"}` entries of a request against the registered
/// servers, filtered by each entry's `allowed_tools` and by the caller's
/// `scope`.
///
/// An unknown label is reported per-server rather than failing the request: the
/// other servers can still do useful work, and the client sees a failed
/// `mcp_list_tools` item naming what was available instead of a bare 400.
pub async fn resolve(state: &SharedState, specs: &[McpToolSpec], scope: &ToolScope) -> Resolved {
    let mut out = Resolved {
        tools: Vec::new(),
        listed: Vec::new(),
        failed: Vec::new(),
        builtin: Vec::new(),
        servers: HashMap::new(),
    };
    if specs.is_empty() {
        return out;
    }
    let snap = state.snapshot();
    // A label naming a **service agent** is the explicit ask that starts its
    // container (container-runtime §3.3). The aggregate never starts one on its
    // own — listing every server's tools must not `podman run` every service on
    // the box — so a thread that attached this agent's label has to say so
    // here, before the list is taken. A start that fails is reported per-server
    // by the loop below, like any other server that could not be reached.
    //
    // Only for a caller who could use it: naming a label is not a reason to
    // start a container whose tools the caller's scope keeps out anyway.
    let mut woke = false;
    for spec in specs.iter().filter(|s| !leaves_nothing(s)) {
        let Some(server) = find_server(&snap, &spec.server_label) else {
            continue;
        };
        let Some(agent) = server.agent_id.clone() else {
            continue;
        };
        if !scope.may_reach(server) {
            continue;
        }
        woke = true;
        if let Err(e) = crate::agents::service::ensure_by_id(state, &agent).await {
            tracing::warn!(
                "agent '{agent}': its app could not be started for a tool list: {}",
                e.reason
            );
        }
    }
    // Only the servers named here are connected and listed (§1.2 of the
    // realtime-server-tools design): a built-in label needs none, and a label
    // the caller cannot reach is answered without one.
    let named: Vec<i64> = specs
        .iter()
        .filter(|s| builtin_label(&s.server_label).is_none() && !leaves_nothing(s))
        .filter_map(|s| find_server(&snap, &s.server_label))
        .filter(|s| scope.may_reach(s))
        .map(|s| s.id)
        .collect();
    let agg = state.mcp.list_tools_of(&snap, &named).await;
    // An agent's list names a label's *current* tools, and a service agent
    // that was asleep a moment ago had none; read it again now it is up.
    let refreshed = if woke {
        Some(scope.refresh(state).await)
    } else {
        None
    };
    let scope = refreshed.as_ref().unwrap_or(scope);

    for spec in specs {
        // A built-in toolset is resolved by its reserved label before the
        // registered servers are searched — `ops` refuses those labels for a
        // user-created server, so there is nothing here to shadow.
        if let Some(label) = builtin_label(&spec.server_label) {
            match resolve_builtin(&snap, label, spec, scope) {
                Ok(defs) => {
                    out.builtin.extend(defs.iter().map(|d| d.name.clone()));
                    out.tools.extend(defs.iter().map(|d| {
                        let short = builtin_short_name(label, &d.name);
                        ResolvedTool::server_side(label, d.clone())
                            .gated(spec.require_approval.requires(&d.name, short))
                    }));
                    out.listed.push((label.to_string(), defs));
                }
                Err(why) => out.failed.push((spec.server_label.clone(), why)),
            }
            continue;
        }
        // A server this caller can never use is answered like one that does
        // not exist, so a scoped key cannot map the inventory by probing. A
        // bare server has no namespace for `may_reach` to read — it was only
        // worth connecting — so it is known now, by what it lists: one that
        // offers this caller nothing is not there for it either.
        let Some(server) = find_server(&snap, &spec.server_label)
            .filter(|s| scope.may_reach(s))
            .filter(|s| !s.tool_prefix.trim().is_empty() || shown_to(s, &agg, scope))
        else {
            out.failed.push((
                spec.server_label.clone(),
                format!(
                    "no MCP server with label '{}' is registered on this gateway \
                     (available: {}). lmgw resolves server_label against its own \
                     servers and does not dial a server_url.",
                    spec.server_label,
                    available_labels(&snap, &agg, scope)
                ),
            ));
            continue;
        };
        if !server.enabled {
            out.failed.push((
                spec.server_label.clone(),
                format!("MCP server '{}' is disabled", server.name),
            ));
            continue;
        }
        if leaves_nothing(spec) {
            out.failed
                .push((spec.server_label.clone(), ALLOWED_LEFT_NOTHING.into()));
            continue;
        }

        // The aggregate keys tools by their *exposed* name and maps each back to
        // its owning server, which is exactly the filter needed here — and it is
        // the same map `tools/call` routes on, so a name listed here is a name
        // the executor below can dispatch.
        let mut defs: Vec<ToolDef> = Vec::new();
        // Which of them the client gated, decided here rather than in the loop:
        // `require_approval` is per *server entry*, and this is the only place
        // that knows both the entry and the tools it resolved to.
        let mut gated: Vec<bool> = Vec::new();
        // Tools the server offers that the caller's scope keeps out, so an
        // empty result can say it was the scope and not the server.
        let mut scoped_out = 0usize;
        for t in &agg.tools {
            let Some((server_id, upstream_name)) = agg.reverse.get(t.name.as_ref()) else {
                continue;
            };
            if *server_id != server.id {
                continue;
            }
            // Match either spelling throughout: clients think in the upstream
            // tool's own name, but see the exposed one in `mcp_list_tools`.
            let exposed = t.name.as_ref();
            // The owner's per-tool switch, applied on every plane.
            if snap.tool_disabled(exposed) {
                continue;
            }
            if !spec.allows(exposed, upstream_name) {
                continue;
            }
            if !scope.admits(exposed) {
                scoped_out += 1;
                continue;
            }
            gated.push(spec.require_approval.requires(exposed, upstream_name));
            defs.push(ToolDef {
                name: t.name.to_string(),
                description: t.description.as_ref().map(|d| d.to_string()),
                parameters: serde_json::to_value(&t.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
            });
        }

        if defs.is_empty() {
            // Prefer the connection's own error — "container image not found"
            // is a far better answer than "the server exposed no tools".
            let detail = match state.mcp.status_view(server.id, &snap).await {
                Some(v) if v.detail.as_ref().is_some_and(|d| !d.is_empty()) => {
                    v.detail.unwrap_or_default()
                }
                _ if scoped_out > 0 => format!(
                    "none of the tools this server offers are within the tool scope of {}",
                    scope.describe()
                ),
                _ => match &spec.allowed_tools {
                    Some(a) => format!("none of allowed_tools {a:?} are exposed by this server"),
                    None => "the server exposed no tools".to_string(),
                },
            };
            out.failed.push((spec.server_label.clone(), detail));
            continue;
        }

        let label = server_label(server);
        out.servers
            .extend(defs.iter().map(|d| (d.name.clone(), server.id)));
        out.tools.extend(
            defs.iter()
                .zip(&gated)
                .map(|(d, g)| ResolvedTool::server_side(label.clone(), d.clone()).gated(*g)),
        );
        out.listed.push((label, defs));
    }
    out
}

fn find_server<'a>(snap: &'a Snapshot, label: &str) -> Option<&'a crate::config::McpServer> {
    snap.mcp_servers
        .values()
        .find(|s| server_label(s) == label)
        // A server addressed by its name even though it has a prefix is a near
        // miss worth honoring rather than a 400 the user has to decode.
        .or_else(|| snap.mcp_servers.values().find(|s| s.name == label))
}

/// The labels this caller could attach: a scoped caller is not told about a
/// server none of whose tools it may use, or it would learn the gateway's
/// inventory from its own typos. An unscoped caller still hears about a server
/// that offers nothing right now — its failure is reported where it is named.
fn available_labels(snap: &Snapshot, agg: &super::Aggregate, scope: &ToolScope) -> String {
    let mut labels: Vec<String> = snap
        .mcp_servers
        .values()
        .filter(|s| s.enabled && shown_to(s, agg, scope))
        .map(server_label)
        .collect();
    labels.sort();
    let admits_any = |entries: Vec<Value>| {
        entries.iter().any(|t| {
            t.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| scope.admits(n))
        })
    };
    if admits_any(kb::list()) {
        labels.insert(0, KB_LABEL.to_string());
    }
    if admits_any(docs::list()) {
        labels.insert(0, DOCS_LABEL.to_string());
    }
    if scope.self_admin() {
        labels.insert(0, SELF_ADMIN_LABEL.to_string());
    }
    labels.join(", ")
}

// ---------------------------------------------------------------------------
// Built-in toolsets as attachable labels
// ---------------------------------------------------------------------------

/// The label the quickdoc toolset is attached by — the same string reserved
/// from southbound servers as their tool prefix.
pub const DOCS_LABEL: &str = super::RESERVED_DOCS_PREFIX;

/// The label the knowledge-base toolset is attached by (chat-complete design
/// §9.4) — the string reserved from southbound servers as their tool prefix.
pub const KB_LABEL: &str = super::RESERVED_KB_PREFIX;

/// The reserved label this spec names, if any.
///
/// Checked before the registered servers, not after: these labels are reserved
/// (`ops` refuses them as a tool prefix), and letting a server claim one by
/// being *named* `docs` would let it answer documentation lookups on the
/// gateway's behalf — the same shadowing [`build_aggregate`] already blocks for
/// tool names.
fn builtin_label(label: &str) -> Option<&'static str> {
    match label {
        SELF_ADMIN_LABEL => Some(SELF_ADMIN_LABEL),
        DOCS_LABEL => Some(DOCS_LABEL),
        KB_LABEL => Some(KB_LABEL),
        _ => None,
    }
}

/// A built-in tool's own name: its full name minus `<label>__`, the second
/// spelling `allowed_tools` and `require_approval` match, as the upstream
/// name is for a registered server's tool.
fn builtin_short_name<'a>(label: &str, name: &'a str) -> &'a str {
    name.strip_prefix(label)
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(name)
}

/// What a label resolves to when its `allowed_tools` is an empty list —
/// said as such, not as a server or toolset that offers nothing. Such a
/// server is neither woken nor connected.
const ALLOWED_LEFT_NOTHING: &str = "allowed_tools is an empty list, so it left nothing: \
     name the tools to allow, or leave allowed_tools out for all of them";

fn leaves_nothing(spec: &McpToolSpec) -> bool {
    spec.allowed_tools.as_ref().is_some_and(Vec::is_empty)
}

/// A `tools/list` JSON entry as an IR tool definition.
fn tool_def(entry: &Value) -> ToolDef {
    ToolDef {
        name: entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: entry
            .get("description")
            .and_then(Value::as_str)
            .map(String::from),
        parameters: entry
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"})),
    }
}

/// One built-in toolset, narrowed by `allowed_tools` and by the owner's
/// per-tool switch. `Err` is the reason to report against the label, in the
/// same `mcp_list_tools`-failed slot an unreachable server uses — a toolset
/// that resolved to nothing must say why rather than quietly not being there.
fn resolve_builtin(
    snap: &Snapshot,
    label: &'static str,
    spec: &McpToolSpec,
    scope: &ToolScope,
) -> Result<Vec<ToolDef>, String> {
    // Who asks, before what the Setting allows: an answer about the mode would
    // tell a client key how to get the configuration API, and no mode does.
    if label == SELF_ADMIN_LABEL && !scope.self_admin() {
        return Err(
            "the 'lmgw' self-admin toolset needs an owner credential: a client key or an \
             agent token does not reach the gateway's configuration. Present an owner key \
             (Usage → Keys), or use /mcp/admin."
                .to_string(),
        );
    }
    // The self-admin mode gate is applied by `selfadmin::list` itself, so
    // attaching the label can never widen what the Setting allows.
    let entries = match label {
        SELF_ADMIN_LABEL => selfadmin::list(snap.settings.self_admin),
        KB_LABEL => kb::list(),
        _ => docs::list(),
    };
    if entries.is_empty() {
        return Err(format!(
            "the '{label}' toolset offers nothing right now: the self-admin tools are \
             switched off (Self-admin tools, under Settings → Network & access)"
        ));
    }
    let offered: Vec<ToolDef> = entries.iter().map(tool_def).collect();
    let defs: Vec<ToolDef> = offered
        .iter()
        .filter(|d| !snap.tool_disabled(&d.name))
        .filter(|d| spec.allows(&d.name, builtin_short_name(label, &d.name)))
        .cloned()
        .collect();
    let in_scope: Vec<ToolDef> = defs
        .iter()
        .filter(|d| scope.admits(&d.name))
        .cloned()
        .collect();
    if in_scope.is_empty() && !defs.is_empty() {
        return Err(format!(
            "none of the '{label}' toolset's tools are within the tool scope of {}",
            scope.describe()
        ));
    }
    let defs = in_scope;
    if defs.is_empty() {
        let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
        return Err(match &spec.allowed_tools {
            Some(a) if a.is_empty() => ALLOWED_LEFT_NOTHING.to_string(),
            Some(a) => format!(
                "none of allowed_tools {a:?} are offered by the '{label}' toolset \
                 (it has: {})",
                names.join(", ")
            ),
            _ => format!("every tool in the '{label}' toolset is disabled by the owner"),
        });
    }
    Ok(defs)
}

/// Executes resolved MCP tools, logging each call like any other request (§10).
pub struct McpExecutor {
    state: SharedState,
    ctx: RequestCtx,
    proto: &'static str,
    /// The names the run listed from a registered server, and its id
    /// ([`Self::with_listed`]).
    listed: HashMap<String, i64>,
}

impl McpExecutor {
    pub fn new(state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            state,
            ctx,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
            listed: HashMap::new(),
        }
    }

    /// Run each of these names on the server the run listed it from, and
    /// nowhere else ([`McpManager::call_listed`](super::McpManager::call_listed)):
    /// between the listing and the call another server may have come to own
    /// the name. A name not among them routes by the aggregate as it is.
    pub fn with_listed(mut self, listed: HashMap<String, i64>) -> Self {
        self.listed = listed;
        self
    }

    /// Log this executor's calls under a different `ingress_proto`, so Logs
    /// distinguishes a tool a Chat thread ran from one `/v1/responses` ran —
    /// the same distinction `"chat"` and `"responses"` already make for the
    /// model turns between them.
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }
}

#[async_trait]
impl ToolExecutor for McpExecutor {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let arguments = match args {
            Value::Object(m) => Some(m.clone()),
            // The model emitted something that isn't an object. Pass nothing and
            // let the tool's own schema validation explain — better than a
            // gateway-authored guess at what it meant.
            _ => None,
        };
        let snap = self.state.snapshot();
        // A tool the owner switched off is refused inside `McpManager::call`,
        // by name — hiding it from `tools/list` is not enough, because a model
        // working from an earlier list still reaches here. It arrives as a tool
        // error carrying the reason, like any other routing failure.
        let routed = match self.listed.get(name) {
            Some(&server) => {
                self.state
                    .mcp
                    .call_listed(&snap, name, server, arguments)
                    .await
            }
            None => self.state.mcp.call(&snap, name, arguments).await,
        };
        let (server_name, outcome) = match routed {
            Ok((result, server)) => {
                let is_error = result.is_error == Some(true);
                (
                    Some(server),
                    ToolOutcome {
                        blocks: blocks_from_result(&result),
                        is_error,
                    },
                )
            }
            // A routing/transport failure is still the *model's* to recover
            // from — it can try another tool or answer without one — so it
            // comes back as a tool error rather than aborting the run.
            Err(e) => (
                e.server().map(str::to_string),
                ToolOutcome::error(e.to_string()),
            ),
        };
        super::ingress::record_tool_call(
            &self.state,
            &self.ctx,
            self.proto,
            name,
            server_name,
            started,
            if outcome.is_error {
                Some(preview(&outcome.blocks))
            } else {
                None
            },
        )
        .await;
        outcome
    }
}

// ---------------------------------------------------------------------------
// The self-admin plane, as a ToolExecutor (Admin Chat)
// ---------------------------------------------------------------------------

/// The `lmgw__*` built-ins offered to a run, as IR tool definitions.
///
/// Filtered by the same [`SelfAdmin`](crate::config::SelfAdmin) mode gate the
/// `/mcp/admin` route uses, so read-only really means read-only wherever the
/// tools are reached from: at `read_only` the mutating tools are not listed,
/// and [`selfadmin::call`] refuses them even if a model names one anyway. The
/// owner's per-tool switch applies on top, as it does on every other plane.
pub fn self_admin_tools(snap: &Snapshot) -> Vec<ToolDef> {
    selfadmin::list(snap.settings.self_admin)
        .iter()
        .map(tool_def)
        .filter(|d| !snap.tool_disabled(&d.name))
        .collect()
}

/// Label the self-admin tools carry in a run, and in the `mcp_call` items a
/// transcript shows. The same string reserved from southbound tool prefixes.
pub const SELF_ADMIN_LABEL: &str = super::RESERVED_TOOL_PREFIX;

/// Executes the built-in `lmgw__*` tools for an in-process run (Admin Chat).
///
/// The counterpart of [`McpExecutor`] for the other half of the tool plane. It
/// goes through [`selfadmin::call`], so the mode gate, the argument validation
/// and the `isError` result shape are the same code an MCP client hits — the
/// only difference is that no HTTP and no token are involved, because nothing
/// left the process.
pub struct SelfAdminExecutor {
    state: SharedState,
    ctx: RequestCtx,
    proto: &'static str,
}

impl SelfAdminExecutor {
    pub fn new(state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            state,
            ctx,
            proto: crate::telemetry::ADMIN_TOOL_PROTO,
        }
    }

    /// Log this executor's calls under a different `ingress_proto`, the way
    /// [`McpExecutor::with_proto`] does and for the same reason: "which surface
    /// asked" is a property of the run, not of the toolset. An agent run that
    /// attaches the `lmgw` label is an agent's tool call, not Admin Chat's.
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }
}

#[async_trait]
impl ToolExecutor for SelfAdminExecutor {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let arguments = match args {
            Value::Object(m) => Some(m.clone()),
            _ => None,
        };
        let call = if self.state.snapshot().tool_disabled(name) {
            Ok(selfadmin::err_result(&super::disabled_message(name)))
        } else {
            selfadmin::call(&self.state, name, arguments).await
        };
        let outcome = match call {
            Ok(v) => ToolOutcome {
                blocks: ToolResultBlock::one(builtin_text(&v)),
                is_error: v.get("isError") == Some(&Value::Bool(true)),
            },
            // An unknown name reaches the model as a tool error it can correct,
            // not as a failed run: the model picked the name, and it is the one
            // that has to pick a different one.
            Err(e) => ToolOutcome::error(e.to_string()),
        };
        super::ingress::record_tool_call(
            &self.state,
            &self.ctx,
            self.proto,
            name,
            Some(SELF_ADMIN_SERVER.to_string()),
            started,
            if outcome.is_error {
                Some(preview(&outcome.blocks))
            } else {
                None
            },
        )
        .await;
        outcome
    }
}

/// Executes the built-in `docs__*` tools for an in-process run.
///
/// The quickdoc counterpart of [`SelfAdminExecutor`]: same [`docs::call`] an
/// MCP client on `/mcp` reaches, so a corpus query from a Chat thread and one
/// from a coding agent are the same code path with the same results.
pub struct DocsExecutor {
    state: SharedState,
    ctx: RequestCtx,
    /// Who to record as the asker on a `docs__request` (quickdoc §7). `/mcp`
    /// takes it from `initialize`; an in-process run has to say so itself.
    client: Option<String>,
    proto: &'static str,
}

impl DocsExecutor {
    pub fn new(state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            state,
            ctx,
            client: None,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
        }
    }

    pub fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(client.into());
        self
    }

    /// See [`McpExecutor::with_proto`]: the docs toolset is reachable from more
    /// than one surface, and the log row should name the one that asked.
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }
}

#[async_trait]
impl ToolExecutor for DocsExecutor {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let arguments = match args {
            Value::Object(m) => Some(m.clone()),
            _ => None,
        };
        let call = if self.state.snapshot().tool_disabled(name) {
            Ok(selfadmin::err_result(&super::disabled_message(name)))
        } else {
            docs::call(&self.state, name, arguments, self.client.as_deref()).await
        };
        let outcome = match call {
            Ok(v) => ToolOutcome {
                blocks: ToolResultBlock::one(builtin_text(&v)),
                is_error: v.get("isError") == Some(&Value::Bool(true)),
            },
            Err(e) => ToolOutcome::error(e.to_string()),
        };
        super::ingress::record_tool_call(
            &self.state,
            &self.ctx,
            self.proto,
            name,
            Some(DOCS_SERVER.to_string()),
            started,
            if outcome.is_error {
                Some(preview(&outcome.blocks))
            } else {
                None
            },
        )
        .await;
        outcome
    }
}

/// Executes the built-in `kb__*` tools for an in-process run (chat-complete
/// design §9.3 tool mode, and any run that attaches the `kb` label).
///
/// The same [`kb::call`] `/mcp` reaches. What differs per caller is **which
/// bases** the tools see: by default the ones shared on `/mcp`
/// ([`KbAccess::McpVisible`]); a Chat thread restricts them to its own
/// selection with [`Self::only`], which ignores that switch.
pub struct KbExecutor {
    state: SharedState,
    ctx: RequestCtx,
    access: KbAccess,
    /// `budget_tokens` when the model passes none — a Chat thread's
    /// retrieval budget. `None`: the owner's `chat_kb_budget_tokens`.
    default_budget: Option<usize>,
    proto: &'static str,
}

impl KbExecutor {
    pub fn new(state: SharedState, ctx: RequestCtx) -> Self {
        Self {
            state,
            ctx,
            access: KbAccess::McpVisible,
            default_budget: None,
            proto: crate::telemetry::RESPONSES_TOOL_PROTO,
        }
    }

    /// Reach exactly these bases, whatever their `mcp_visible` says.
    pub fn only(mut self, kb_ids: impl IntoIterator<Item = i64>) -> Self {
        self.access = KbAccess::Only(kb_ids.into_iter().collect());
        self
    }

    pub fn with_access(mut self, access: KbAccess) -> Self {
        self.access = access;
        self
    }

    pub fn with_default_budget(mut self, budget: Option<usize>) -> Self {
        self.default_budget = budget;
        self
    }

    /// See [`McpExecutor::with_proto`].
    pub fn with_proto(mut self, proto: &'static str) -> Self {
        self.proto = proto;
        self
    }
}

#[async_trait]
impl ToolExecutor for KbExecutor {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = Instant::now();
        let arguments = match args {
            Value::Object(m) => Some(m.clone()),
            _ => None,
        };
        let call = if self.state.snapshot().tool_disabled(name) {
            Ok(selfadmin::err_result(&super::disabled_message(name)))
        } else {
            kb::call(
                &self.state,
                name,
                arguments,
                &self.access,
                self.default_budget,
            )
            .await
        };
        let outcome = match call {
            Ok(v) => ToolOutcome {
                blocks: ToolResultBlock::one(builtin_text(&v)),
                is_error: v.get("isError") == Some(&Value::Bool(true)),
            },
            Err(e) => ToolOutcome::error(e.to_string()),
        };
        super::ingress::record_tool_call(
            &self.state,
            &self.ctx,
            self.proto,
            name,
            Some(KB_SERVER.to_string()),
            started,
            if outcome.is_error {
                Some(preview(&outcome.blocks))
            } else {
                None
            },
        )
        .await;
        outcome
    }
}

/// The text blocks of a built-in tool result, joined — both built-in planes
/// answer with a single text block by design (`selfadmin::ok_result`).
fn builtin_text(v: &Value) -> String {
    v.get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Routes a run's tool calls to whichever plane owns the tool.
///
/// A thread can hold tools from all three planes at once — the self-admin
/// built-ins, the quickdoc built-ins and registered MCP servers — and the loop
/// has one executor. Dispatch is on the tool's name against **the built-in
/// names this run actually resolved**, not on the `lmgw__`/`docs__` prefix: a
/// run that never attached the self-admin toolset must not be able to reach it
/// by having the model guess a name. Everything else goes to MCP, whose router
/// already answers unknown names with a tool error the model can correct.
pub struct SplitExecutor {
    admin: SelfAdminExecutor,
    docs: DocsExecutor,
    kb: KbExecutor,
    builtin_names: HashSet<String>,
    mcp: McpExecutor,
}

impl SplitExecutor {
    /// The `kb__*` half starts as the `/mcp` view of the knowledge bases
    /// (the `mcp_visible` ones), logged like the docs half; a caller that
    /// restricts it to its own selection replaces it with [`Self::with_kb`].
    pub fn new(
        admin: SelfAdminExecutor,
        docs: DocsExecutor,
        builtin_names: impl IntoIterator<Item = String>,
        mcp: McpExecutor,
    ) -> Self {
        let kb = KbExecutor::new(docs.state.clone(), docs.ctx.clone()).with_proto(docs.proto);
        Self {
            admin,
            docs,
            kb,
            builtin_names: builtin_names.into_iter().collect(),
            mcp,
        }
    }

    /// The knowledge-base half this run uses — a Chat thread's, restricted
    /// to its selected bases ([`KbExecutor::only`]).
    pub fn with_kb(mut self, kb: KbExecutor) -> Self {
        self.kb = kb;
        self
    }
}

#[async_trait]
impl ToolExecutor for SplitExecutor {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        if !self.builtin_names.contains(name) {
            return self.mcp.call(name, args).await;
        }
        if selfadmin::owns(name) {
            self.admin.call(name, args).await
        } else if kb::owns(name) {
            self.kb.call(name, args).await
        } else {
            self.docs.call(name, args).await
        }
    }
}

/// `upstream_name` on a self-admin tool call's log row, so the Logs tab shows a
/// provenance instead of a blank.
const SELF_ADMIN_SERVER: &str = "lmgw (self-admin)";

/// Same, for a `docs__*` call made inside the gateway.
const DOCS_SERVER: &str = "lmgw (docs)";

/// Same, for a `kb__*` call.
pub(super) const KB_SERVER: &str = "lmgw (knowledge)";

/// First ~200 characters of a failed result, for the log row's error message.
fn preview(blocks: &[ToolResultBlock]) -> String {
    let (text, _) = crate::ir::flatten_tool_result(blocks);
    text.chars().take(200).collect()
}

/// `CallToolResult` → IR blocks (§7). This is the conversion the block-shaped
/// `ToolResult` exists for: an MCP tool that returns an image or structured
/// content reaches the model as an image or structured content, instead of
/// being stringified on the way in.
fn blocks_from_result(result: &rmcp::model::CallToolResult) -> Vec<ToolResultBlock> {
    let mut out: Vec<ToolResultBlock> = Vec::new();
    for c in &result.content {
        let raw = match serde_json::to_value(c) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let block = match raw.get("type").and_then(Value::as_str) {
            Some("text") => {
                ToolResultBlock::text(raw.get("text").and_then(Value::as_str).unwrap_or_default())
            }
            Some("image") => ToolResultBlock::Image {
                mime: raw
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_string(),
                data: raw
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("audio") => ToolResultBlock::Audio {
                mime: raw
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("audio/wav")
                    .to_string(),
                data: raw
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("resource") => {
                let r = raw.get("resource").cloned().unwrap_or(Value::Null);
                ToolResultBlock::Resource {
                    uri: r
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    mime: r.get("mimeType").and_then(Value::as_str).map(String::from),
                    text: r.get("text").and_then(Value::as_str).map(String::from),
                }
            }
            // A content type this build of rmcp knows and we don't: keep it as
            // structured JSON so the model still sees it (§14).
            _ => ToolResultBlock::Json { value: raw },
        };
        out.push(block);
    }
    // `structuredContent` is the MCP-native way to return data rather than
    // prose, and it is exactly what Gemini's functionResponse wants.
    if let Some(sc) = &result.structured_content {
        out.push(ToolResultBlock::Json { value: sc.clone() });
    }
    if out.is_empty() {
        out.push(ToolResultBlock::text(""));
    }
    out
}
