//! The resource aggregate and the routing of a read (L14).
//!
//! **Whose a URI is.** Its *candidates* are the enabled servers it can
//! belong to, by server name: in a tool prefix's namespace (the longest
//! prefix that strips from it), that prefix's servers; otherwise every bare
//! server, and — for a URI without an authority, which no prefix changes —
//! every server. A candidate *claims* it in one of three ways, strongest
//! first ([`claims`]): a tool of its own names it in `_meta.ui.resourceUri`;
//! its `resources/list` lists it; it has a `uriTemplate` the URI fits (a
//! prefixed server's only with a literal `scheme://`, [`template_usable`]).
//! The strongest claim has it, the first by name of the candidates that
//! claim it that way. A URI in a namespace that no candidate claims is the
//! first candidate's still (a server may leave a UI resource unlisted, and
//! one that is not connected lists nothing); any other URI no server claims
//! is not found.
//!
//! `resources/list` shows a server's resource only where that rule gives it
//! to the server, so the list and a read never disagree: a bare server's
//! URI that falls in a prefix's namespace, or that another server claims
//! more strongly or earlier by name, is left out (logged at debug).
//!
//! **Who is asked.** Only what the answer needs goes over a server's link.
//! A listing lists the servers the caller reaches, and of the others only
//! those that could take one of their URIs from them (a candidate earlier
//! by name; a stronger claim, a tool's, needs no request). A read lists
//! the URI's candidates, and none when the caller reaches none of them. A
//! device row the caller does not reach is never asked about a URI with an
//! authority — what its tools name still counts, from the tool list lmgw
//! already holds.
//!
//! **Reads go where calls go.** A server that is not connected is connected
//! (a service agent's app started first) — for a caller whose scope
//! narrows it, before its reach is judged, since a stopped server offers no
//! tools to judge by; never for a caller that cannot reach it at all. A
//! device row's read goes over its link as a call does — with `_meta`,
//! bounded by the row's `timeout_ms`, cancelled on the device when lmgw
//! stops waiting, a closed link reported with why — and any other server's
//! is bounded by its `timeout_ms`. A server's own JSON-RPC error passes on
//! with its code. Nothing bounds a resource's size beyond what bounds the
//! link it comes over: a device's link closes on `mcp.host_max_message_mb`,
//! naming it, and the read reports that close.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::future::join_all;
use rmcp::model::{
    ClientRequest, ErrorData, ReadResourceRequest, ReadResourceRequestParams, ServerResult,
};
use rmcp::service::{Peer, ServiceError};
use rmcp::RoleClient;
use serde_json::Value;

use crate::config::{McpServer, Snapshot};

use super::super::host::CallFrom;
use super::super::scope::ToolScope;
use super::super::{Aggregate, CallError, InFlightGuard, McpManager, McpStatus};
use super::apps::ui_resource;
use super::uri::{namespaced, template_usable};

mod claims;

use claims::{listed_owners, owner, rivals, unasked, Known, Servers};

/// Why a resource request failed, as `/mcp` answers it.
#[derive(Debug)]
pub enum ResourceError {
    /// No server has the URI (MCP's `-32002`).
    NotFound(String),
    /// The caller does not reach the server that has it (`-32002`, as a
    /// tool outside a scope answers as one not there, with why).
    OutOfReach(String),
    /// The server answered with a JSON-RPC error: its code, passed on.
    Server { code: i32, message: String },
    /// The server could not be reached, timed out, or failed.
    Call(CallError),
}

/// MCP's "resource not found".
pub const RESOURCE_NOT_FOUND: i64 = -32002;

impl ResourceError {
    pub fn rpc_code(&self) -> i64 {
        match self {
            Self::NotFound(_) | Self::OutOfReach(_) => RESOURCE_NOT_FOUND,
            Self::Server { code, .. } => i64::from(*code),
            Self::Call(e) => e.rpc_code(),
        }
    }
}

impl std::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::OutOfReach(m) => f.write_str(m),
            Self::Server { message, .. } => f.write_str(message),
            Self::Call(e) => write!(f, "{e}"),
        }
    }
}

/// Does `scope`'s caller reach `server`'s resources? Its scope's own answer
/// for the row ([`ToolScope::may_reach`], L16 for a device row), and where
/// the scope narrows it, a tool of the server it admits that the owner has
/// not switched off, or the server's whole namespace.
pub(crate) fn reaches(
    scope: &ToolScope,
    server: &McpServer,
    agg: &Aggregate,
    snap: &Snapshot,
) -> bool {
    if !scope.may_reach(server) {
        return false;
    }
    if !scope.narrows_for(server) {
        return true;
    }
    let offered = agg.reverse.iter().any(|(name, (sid, _))| {
        *sid == server.id && !agg.tool_disabled(snap, name) && scope.admits(name)
    });
    let prefix = server.tool_prefix.trim();
    offered || (!prefix.is_empty() && scope.admits_namespace(prefix))
}

/// The refusal of a resource of `server` to `scope`'s caller.
fn out_of_reach(uri: &str, server: &McpServer, scope: &ToolScope) -> ResourceError {
    let label = super::super::exec::server_label(server);
    ResourceError::OutOfReach(if server.is_device() {
        format!(
            "resource '{uri}' — '{label}' is a paired device's hosted label: only the owner, \
             that device, and a key, device or agent whose tool scope names '{label}__' \
             explicitly reach it; {} does not",
            scope.describe()
        )
    } else {
        format!(
            "resource '{uri}' — server '{label}' is outside the tool scope of {}",
            scope.describe()
        )
    })
}

/// Which listing a request reads.
#[derive(Clone, Copy, PartialEq)]
enum Listing {
    Resources,
    Templates,
}

/// A connected server, its peer, and whether it serves resources at all.
type Connected = (McpServer, Peer<RoleClient>, bool);

/// What every server in `conns` is known by without asking it: the URIs
/// its tools name.
fn known_of(conns: &[Connected], agg: &Aggregate) -> HashMap<i64, Known> {
    let mut ui = ui_by_server(agg);
    conns
        .iter()
        .map(|(server, _, _)| {
            let known = Known {
                server: server.clone(),
                ui: ui.remove(&server.id).unwrap_or_default(),
                resources: Vec::new(),
                templates: Vec::new(),
            };
            (server.id, known)
        })
        .collect()
}

/// Ask every server of `conns` in `ids` that serves resources for the
/// listings `which`, concurrently, each bounded by its `timeout_ms`, into
/// `known`. A server that fails is left out and logged, as a server that
/// cannot list its tools is.
async fn ask(
    conns: &[Connected],
    ids: &HashSet<i64>,
    which: &[Listing],
    known: &mut HashMap<i64, Known>,
) {
    let reads = conns
        .iter()
        .filter(|(s, _, serves)| *serves && ids.contains(&s.id))
        .flat_map(|(server, peer, _)| {
            which.iter().map(move |w| async move {
                let timeout = Duration::from_millis(server.timeout_ms);
                (server.id, *w, listed(server, peer, timeout, *w).await)
            })
        });
    for (id, which, items) in join_all(reads).await {
        if let Some(k) = known.get_mut(&id) {
            match which {
                Listing::Resources => k.resources = items,
                Listing::Templates => k.templates = items,
            }
        }
    }
}

/// The answer for a URI no server has.
fn not_found(exposed: &str) -> ResourceError {
    ResourceError::NotFound(format!(
        "resource not found: '{exposed}' — no server on this gateway lists it, names it in a \
         tool's _meta.ui.resourceUri or has a template it fits, and it is in no tool prefix's \
         namespace"
    ))
}

impl McpManager {
    /// The connected, enabled servers, by name, with their peers and
    /// whether they serve resources at all.
    async fn connected(&self, snap: &Snapshot) -> Vec<Connected> {
        let conns = self.conns.read().await;
        let mut out: Vec<Connected> = conns
            .iter()
            .filter(|(_, c)| c.status == McpStatus::Ready)
            .filter_map(|(id, c)| {
                let server = snap.mcp_servers.get(id).filter(|s| s.enabled)?;
                let peer = match &c.running {
                    Some(r) => r.peer().clone(),
                    None => c.device.as_ref()?.peer()?.clone(),
                };
                let serves = peer
                    .peer_info()
                    .is_some_and(|i| i.capabilities.resources.is_some());
                Some((server.clone(), peer, serves))
            })
            .collect();
        out.sort_by(|a, b| a.0.name.cmp(&b.0.name).then(a.0.id.cmp(&b.0.id)));
        out
    }

    /// `resources/list` on `/mcp`: every connected server's resources that
    /// are its own by the module doc's rule and that `scope`'s caller
    /// reaches, URIs namespaced. Nothing is connected for it: the spec's
    /// "the ready servers' resources".
    pub async fn list_resources(&self, snap: &Snapshot, scope: &ToolScope) -> Vec<Value> {
        self.aggregate_listing(snap, scope, Listing::Resources)
            .await
    }

    /// `resources/templates/list` on `/mcp`, as [`list_resources`](Self::list_resources).
    pub async fn list_resource_templates(&self, snap: &Snapshot, scope: &ToolScope) -> Vec<Value> {
        self.aggregate_listing(snap, scope, Listing::Templates)
            .await
    }

    async fn aggregate_listing(
        &self,
        snap: &Snapshot,
        scope: &ToolScope,
        which: Listing,
    ) -> Vec<Value> {
        let agg = self.aggregate(snap).await;
        let servers = Servers::of(snap);
        let conns = self.connected(snap).await;
        let reached: HashSet<i64> = conns
            .iter()
            .filter(|(s, _, _)| reaches(scope, s, &agg, snap))
            .map(|(s, _, _)| s.id)
            .collect();
        let mut known = known_of(&conns, &agg);
        ask(&conns, &reached, &[which], &mut known).await;
        let (key, owners) = match which {
            Listing::Resources => {
                let rivals = rivals(&servers, &known, &reached, scope);
                ask(&conns, &rivals, &[Listing::Resources], &mut known).await;
                ("uri", Some(listed_owners(&servers, &known)))
            }
            Listing::Templates => ("uriTemplate", None),
        };
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for (server, _, _) in conns.iter().filter(|(s, _, _)| reached.contains(&s.id)) {
            let Some(k) = known.get(&server.id) else {
                continue;
            };
            let p = &server.tool_prefix;
            let items = match which {
                Listing::Resources => &k.resources,
                Listing::Templates => &k.templates,
            };
            for item in items {
                let Some(own) = item.get(key).and_then(Value::as_str) else {
                    continue;
                };
                let exposed = namespaced(p, own);
                let left_out = match &owners {
                    Some(owners) => (owners.get(&exposed).map(|o| o.2) != Some(server.id))
                        .then_some(
                            "another server has that URI (a tool prefix's namespace, a \
                                    stronger claim, or one earlier by name)",
                        ),
                    None if !template_usable(p, own) => Some(
                        "a prefixed server's template without a literal scheme:// claims nothing",
                    ),
                    None => (!servers.is_candidate(server, &exposed))
                        .then_some("the template is in another tool prefix's namespace"),
                };
                if let Some(why) = left_out {
                    tracing::debug!(
                        server = %server.name,
                        uri = %exposed,
                        "MCP resource left out of /mcp's listing: {why}"
                    );
                    continue;
                }
                if !seen.insert(exposed.clone()) {
                    continue;
                }
                let mut item = item.clone();
                item[key] = Value::String(exposed);
                out.push(item);
            }
        }
        out
    }

    /// `resources/read` on `/mcp` of `exposed`, for `scope`'s caller, run
    /// as `from` (a device's read carries it in `_meta`, as a call does).
    pub async fn read_resource(
        &self,
        snap: &Snapshot,
        exposed: &str,
        scope: &ToolScope,
        from: &CallFrom,
    ) -> Result<Value, ResourceError> {
        let servers = Servers::of(snap);
        let (cands, in_namespace) = servers.candidates(exposed);
        // Reach first where it costs nothing: a caller that reaches none of
        // the candidates learns no more, and no server is asked or woken.
        if !cands.iter().any(|(s, _)| scope.may_reach(s)) {
            return Err(match cands.first() {
                None => not_found(exposed),
                Some((s, _)) if in_namespace => out_of_reach(exposed, s, scope),
                Some(_) => ResourceError::OutOfReach(format!(
                    "resource '{exposed}' — no server within the tool scope of {} has it",
                    scope.describe()
                )),
            });
        }
        let agg = self.aggregate(snap).await;
        // A namespace with one server is that server's: nothing to list.
        let found = if in_namespace && cands.len() == 1 {
            cands.into_iter().next()
        } else {
            let conns = self.connected(snap).await;
            let mut known = known_of(&conns, &agg);
            let asked: HashSet<i64> = cands
                .iter()
                .filter(|(s, _)| !unasked(scope, s, exposed))
                .map(|(s, _)| s.id)
                .collect();
            ask(
                &conns,
                &asked,
                &[Listing::Resources, Listing::Templates],
                &mut known,
            )
            .await;
            owner(cands, in_namespace, &known, exposed)
        };
        let Some((server, own)) = found else {
            return Err(not_found(exposed));
        };
        if !scope.may_reach(server) {
            return Err(out_of_reach(exposed, server, scope));
        }
        // A scope that narrows the server is judged by the server's tools,
        // and a stopped server offers none: connect it first (a device row
        // connects only when its device does).
        let agg = if scope.narrows_for(server) && !self.is_ready(server.id).await {
            if server.is_device() {
                return Err(ResourceError::Call(self.offline(server).await));
            }
            self.ensure_connected(server)
                .await
                .map_err(ResourceError::Call)?;
            self.aggregate(snap).await
        } else {
            agg
        };
        if !reaches(scope, server, &agg, snap) {
            return Err(out_of_reach(exposed, server, scope));
        }
        let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
            ReadResourceRequestParams::new(own),
        ));
        match self
            .request_on(server, request, from)
            .await
            .map_err(ResourceError::Call)?
        {
            Ok(ServerResult::ReadResourceResult(result)) => {
                let mut v = serde_json::to_value(result).unwrap_or_default();
                if let Some(contents) = v.get_mut("contents").and_then(Value::as_array_mut) {
                    for c in contents {
                        if let Some(Value::String(u)) = c.get_mut("uri") {
                            *u = namespaced(&server.tool_prefix, u);
                        }
                    }
                }
                Ok(v)
            }
            Ok(_) => Err(ResourceError::Call(CallError::Upstream {
                server: server.name.clone(),
                detail: "it answered resources/read with something that is not a resource".into(),
            })),
            Err(e) => Err(ResourceError::Server {
                code: e.code.0,
                message: format!("server '{}': {}", server.name, e.message),
            }),
        }
    }

    /// Connect `server` when it is not, as a call does: a service agent's
    /// app started first. The error says why it is still not connected.
    async fn ensure_connected(&self, server: &McpServer) -> Result<(), CallError> {
        if self.is_ready(server.id).await {
            return Ok(());
        }
        if let (Some(app), Some(agent)) = (self.app(), server.agent_id.clone()) {
            if let Err(e) = crate::agents::service::ensure_by_id(&app, &agent).await {
                return Err(CallError::NotConnected {
                    server: server.name.clone(),
                    detail: e.reason.clone(),
                });
            }
        }
        self.start_one(server).await;
        let conns = self.conns.read().await;
        let detail = match conns.get(&server.id).map(|c| &c.status) {
            Some(McpStatus::Ready) => return Ok(()),
            Some(McpStatus::Error(e)) => e.clone(),
            _ => "not ready".to_string(),
        };
        Err(CallError::NotConnected {
            server: server.name.clone(),
            detail,
        })
    }

    /// Send `request` to `server`, as a tool call is sent: a device row's
    /// over its link ([`device_request`](Self::device_request)), any other
    /// connected first when it is not, bounded by its `timeout_ms`.
    async fn request_on(
        &self,
        server: &McpServer,
        request: ClientRequest,
        from: &CallFrom,
    ) -> Result<Result<ServerResult, ErrorData>, CallError> {
        if server.is_device() {
            return self.device_request(server, request, from).await;
        }
        self.ensure_connected(server).await?;
        let (peer, in_flight, detail) = {
            let conns = self.conns.read().await;
            match conns.get(&server.id) {
                Some(c) if c.status == McpStatus::Ready => (
                    c.running.as_ref().map(|r| r.peer().clone()),
                    Some(c.in_flight.clone()),
                    String::new(),
                ),
                Some(c) => match &c.status {
                    McpStatus::Error(e) => (None, None, e.clone()),
                    _ => (None, None, "not ready".to_string()),
                },
                None => (None, None, "not ready".to_string()),
            }
        };
        let Some(peer) = peer else {
            return Err(CallError::NotConnected {
                server: server.name.clone(),
                detail,
            });
        };
        let _in_flight = in_flight.map(InFlightGuard::new);
        let timeout = Duration::from_millis(server.timeout_ms);
        let outcome = tokio::time::timeout(timeout, peer.send_request(request)).await;
        if let Some(c) = self.conns.write().await.get_mut(&server.id) {
            c.last_used = Instant::now();
        }
        match outcome {
            Err(_) => Err(CallError::Timeout {
                server: server.name.clone(),
                timeout_ms: server.timeout_ms,
            }),
            Ok(Err(ServiceError::McpError(e))) => Ok(Err(e)),
            Ok(Err(e)) => Err(CallError::Upstream {
                server: server.name.clone(),
                detail: e.to_string(),
            }),
            Ok(Ok(result)) => Ok(Ok(result)),
        }
    }
}

/// The URIs each server's tools name in `_meta.ui.resourceUri`, as `/mcp`
/// shows them (the aggregate namespaced them): one pass over the tools.
fn ui_by_server(agg: &Aggregate) -> HashMap<i64, HashSet<String>> {
    let mut out: HashMap<i64, HashSet<String>> = HashMap::new();
    for t in &agg.tools {
        let (Some((sid, _)), Some(uri)) = (
            agg.reverse.get(t.name.as_ref()),
            ui_resource(t.meta.as_ref()),
        ) else {
            continue;
        };
        out.entry(*sid).or_default().insert(uri.to_string());
    }
    out
}

/// One server's own listing, every page, as JSON; nothing (and a warning)
/// when it fails or outlasts `timeout`.
async fn listed(
    server: &McpServer,
    peer: &Peer<RoleClient>,
    timeout: Duration,
    which: Listing,
) -> Vec<Value> {
    let what = match which {
        Listing::Resources => "resources/list",
        Listing::Templates => "resources/templates/list",
    };
    let got = match which {
        Listing::Resources => tokio::time::timeout(timeout, peer.list_all_resources())
            .await
            .map(|r| {
                r.map(|v| {
                    v.iter()
                        .filter_map(|x| serde_json::to_value(x).ok())
                        .collect()
                })
            }),
        Listing::Templates => tokio::time::timeout(timeout, peer.list_all_resource_templates())
            .await
            .map(|r| {
                r.map(|v| {
                    v.iter()
                        .filter_map(|x| serde_json::to_value(x).ok())
                        .collect()
                })
            }),
    };
    match got {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => {
            tracing::warn!("MCP server '{}': {what} failed: {e}", server.name);
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(
                "MCP server '{}': {what} took longer than its timeout_ms ({} ms)",
                server.name,
                server.timeout_ms
            );
            Vec::new()
        }
    }
}
