//! MCP Gateway — **northbound** Streamable HTTP server (§6, §7, §17 M3).
//!
//! A hand-rolled, minimal-but-compliant MCP **Streamable HTTP** server that
//! exposes lmgw itself as an MCP server. Milestone 3 swapped the M0 spike's one
//! hard-coded `echo` tool for the real **aggregate** ([`McpManager`]): northbound
//! `tools/list` returns the `__`-prefixed, hide/rename-applied catalog of every
//! connected southbound server, and `tools/call` routes through the manager's
//! reverse map to the owning upstream — logging each call into the unified
//! `request_logs` feed (§10). The §6 MUST-list (validated against a real Claude
//! Code client, 2026-06-30) is preserved verbatim from the spike:
//!   - single JSON-RPC request/notification/response per POST — **no batching**
//!     (removed in MCP 2025-06-18);
//!   - `Accept` must allow `application/json` (we only return JSON in MVP);
//!   - notifications/responses → `202 Accepted`, no body;
//!   - `initialize` negotiates `MCP-Protocol-Version`, assigns `MCP-Session-Id`;
//!   - later requests must carry a known session id (`400` missing / `404` gone);
//!   - lifecycle ordering (non-`initialize`/`ping` before init is rejected — it
//!     would have no session id);
//!   - `GET /mcp` (M5): opens the server→client SSE stream for a **valid
//!     session** (200 SSE), pushing `notifications/tools/list_changed` when the
//!     aggregate's composition changes; a session-less GET is still `400`/`404`
//!     (the §6 session errors), no longer the M0 `405` placeholder;
//!   - `DELETE /mcp` terminates the session;
//!   - `Origin` validation → `403` (DNS-rebinding defense, enforced regardless of
//!     the auth toggle).
//!
//! **Test seam (§16).** The protocol dispatch is split from the tool plane via
//! the [`ToolPlane`] trait: the HTTP layer (sessions, the MUST-list) is the same
//! for everyone, while `tools/list`/`tools/call` go through a `ToolPlane` the
//! integration tests can replace with a fake — so the golden dispatch tests
//! don't need a live southbound MCP server.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseFrame, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Router};
use futures::stream::{self, StreamExt};
use serde_json::{json, Map, Value};
use tokio_stream::wrappers::BroadcastStream;

use crate::mcp::scope::ToolScope;
use crate::mcp::{selfadmin, CallError};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

mod resources;
mod server_meta;
mod tool_row;

pub(crate) use tool_row::{record_tool_call, record_tool_canceled, RowWatch};

/// Protocol revision we advertise. Clients on an older supported rev still work.
const PROTOCOL_VERSION: &str = "2025-11-25";
/// Revisions we accept on the `MCP-Protocol-Version` header / `initialize`.
const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// `GET /mcp` SSE keep-alive interval (§6/§9). Axum emits an SSE comment line
/// (`:`) every this-often when the stream is idle, so a proxy/client that times
/// out a quiet connection keeps it open between `tools/list_changed` pushes. A
/// **visible** named constant, not a hidden cap — it only paces keep-alive
/// comments and never bounds or truncates the notification stream itself.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Issued northbound session ids → what their `initialize` said.
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What a northbound session's `initialize` said that lmgw keeps.
#[derive(Clone, Default)]
struct Session {
    /// `clientInfo.name`, kept because `docs__request` records who asked
    /// (quickdoc §7): a queue entry that says "claude-code wanted tower
    /// docs" is actionable in a way an anonymous one is not. `None` when
    /// the client sent no `clientInfo`.
    client_name: Option<String>,
    /// Whether the client declared the MCP Apps extension
    /// (`capabilities.extensions["io.modelcontextprotocol/ui"]`): an apps
    /// host, which renders views and calls their app-only tools for them.
    /// A client that did not is offered no tool whose `_meta.ui.visibility`
    /// leaves `"model"` out (client-apps design §7.2).
    apps_host: bool,
}

impl Session {
    /// The session `initialize`'s `params` open.
    fn of_initialize(params: &Value) -> Self {
        Self {
            client_name: params
                .get("clientInfo")
                .and_then(|c| c.get("name"))
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            apps_host: params
                .get("capabilities")
                .and_then(|c| c.get("extensions"))
                .and_then(|e| e.get(crate::mcp::resources::UI_EXTENSION))
                .is_some_and(|v| !v.is_null()),
        }
    }
}

/// The `/mcp` routes, each declaring the capability it needs (principals
/// §3.2): `Inference` for the aggregate plane and its session handlers, so an
/// MCP client authenticates with the same gateway keys as `/v1`, and `Admin`
/// for the self-admin `POST /mcp/admin` beside them.
///
/// **Two planes, two routes (§21 stage 2).** `/mcp` is the aggregate of the
/// registered southbound servers. The built-in `lmgw__*` self-admin tools used
/// to ride along on it, which meant every coding agent holding a gateway key
/// was also holding the gateway's configuration API — a much larger grant than
/// "let this agent use my MCP servers", and one nobody had opted into. They now
/// live on `/mcp/admin` behind an **owner** credential (principals §3.7): the
/// `owner:self-admin` key, which is seeded *disabled* on a gateway that had no
/// self-admin token, so the admin plane is still *closed* until it is
/// deliberately opened. The owner's own path to those tools is Admin Chat,
/// which runs in-process and needs no credential at all.
pub fn routes(state: &SharedState) -> Router<SharedState> {
    use axum::handler::Handler;

    use crate::principal::Cap;
    use crate::server::{require, require_admin_token};

    Router::new()
        .route(
            "/mcp",
            post(mcp_post)
                .get(mcp_get)
                .delete(mcp_delete)
                .route_layer(require(state, Cap::Inference)),
        )
        // The one path in the gateway whose methods do not share a capability
        // (principals §3.5, §3.7): the POST is the self-admin plane, the GET
        // and DELETE are the aggregate plane's session handlers and belong to
        // whoever may call `/mcp` at all. Per handler with `Handler::layer`,
        // therefore, and never `route_layer` on the path.
        .route(
            "/mcp/admin",
            post(admin_post.layer(require_admin_token(state)))
                .get(mcp_get.layer(require(state, Cap::Inference)))
                .delete(mcp_delete.layer(require(state, Cap::Inference))),
        )
}

// ---------------------------------------------------------------------------
// Tool plane — the seam between protocol dispatch and the aggregate (§16).
// ---------------------------------------------------------------------------

/// The aggregate behind `tools/list` / `tools/call`, abstracted so the
/// northbound dispatch can be golden-tested with a fake (no live MCP server).
/// `McpManager` is wired in via [`ManagerPlane`]; tests inject their own.
#[async_trait]
pub trait ToolPlane: Send + Sync {
    /// Exposed tools as JSON-RPC `tools/list` `tools` array entries (already
    /// serialized rmcp `Tool`s with exposed names). Triggers the lazy-list
    /// contract (§9): bounded connects, partial results.
    async fn list_tools(&self) -> Vec<Value>;

    /// Route + invoke a tool by exposed name. `Ok` carries the serialized
    /// `CallToolResult` (`content` + `isError`, verbatim, §7); `Err` carries the
    /// typed failure the dispatcher maps to JSON-RPC + a non-ok log row (§10).
    async fn call_tool(
        &self,
        name: &str,
        args: Option<Map<String, Value>>,
    ) -> Result<Value, CallError>;

    /// `resources/*` (client-apps design §7.2): `None` for a plane that
    /// serves no resources, whose dispatch answers `-32601` as for any
    /// method it does not know; else the result or the JSON-RPC error.
    async fn resources(
        &self,
        _method: &str,
        _params: &Value,
    ) -> Option<Result<Value, (i64, String)>> {
        None
    }
}

/// Server name recorded on the log row for a built-in `lmgw__*` call, so the
/// unified feed shows self-admin calls with a provenance instead of a blank.
const SELF_SERVER_NAME: &str = "lmgw (self-admin)";

/// Same, for a built-in `docs__*` call.
const DOCS_SERVER_NAME: &str = "lmgw (docs)";

/// The `/mcp/admin` plane: the built-in `lmgw__*` tools ([`selfadmin`]) and
/// nothing else. Deliberately not a superset of [`AggregatePlane`] — a client
/// that wants both connects to both, and one that only wants tools cannot be
/// handed the configuration API by accident.
///
/// **No agent allow list or key tool scope applies here**, deliberately. The
/// gate on this plane is an owner credential ([`admin_gate`]) plus the
/// self-admin *mode*; an owner key has no tool scope, and a manifest's
/// `tools[]` is not a second gate: an agent that presents both its
/// own token and an owner key is a client that already holds the configuration
/// credential, and filtering its view by its manifest would read as a boundary
/// while changing nothing about what it may do. Presenting an agent token here
/// therefore behaves exactly as presenting none.
struct AdminPlane {
    state: SharedState,
    ctx: RequestCtx,
}

#[async_trait]
impl ToolPlane for AdminPlane {
    async fn list_tools(&self) -> Vec<Value> {
        let snap = self.state.snapshot();
        let mut tools = selfadmin::list(snap.settings.self_admin);
        retain_enabled(&snap, &mut tools);
        tools
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Option<Map<String, Value>>,
    ) -> Result<Value, CallError> {
        let started = Instant::now();
        // Not a self-admin name: on this plane that is a lookup miss, not a
        // routing hint. Sending it to the aggregate would quietly re-merge the
        // two planes the split exists to keep apart.
        if !selfadmin::owns(name) {
            let e = CallError::ToolNotFound(name.to_string());
            record_mcp_call(
                &self.state,
                &self.ctx,
                name,
                None,
                started,
                CallOutcome::Failed(&e),
            )
            .await;
            return Err(e);
        }
        if let Some(e) = disabled(&self.state, name) {
            record_mcp_call(
                &self.state,
                &self.ctx,
                name,
                None,
                started,
                CallOutcome::Failed(&e),
            )
            .await;
            return Err(e);
        }
        let result = selfadmin::call(&self.state, name, args).await;
        let (server, outcome) = match &result {
            Ok(v) => (
                Some(SELF_SERVER_NAME.to_string()),
                CallOutcome::Completed {
                    tool_error: builtin_tool_error(v),
                },
            ),
            Err(e) => (None, CallOutcome::Failed(e)),
        };
        record_mcp_call(&self.state, &self.ctx, name, server, started, outcome).await;
        result
    }
}

/// The `/mcp` plane: the aggregate of every registered southbound server plus
/// the built-in `docs__*` and `kb__*` toolsets, routed through the live [`McpManager`]
/// against the current snapshot. Every `tools/call` lands in `request_logs`
/// (§10).
///
/// Both built-in namespaces are [reserved](super::RESERVED_NAMESPACES) from
/// southbound servers, so a southbound tool can never collide with one — which
/// is what makes the split between this plane and [`AdminPlane`] total rather
/// than a precedence rule. `docs__*` is here rather than on the admin plane
/// deliberately (quickdoc §7): it is the tool surface for ordinary agents, and
/// putting the documentation lookup behind the configuration token would leave
/// every agent guessing from pretraining instead.
struct AggregatePlane {
    state: SharedState,
    ctx: RequestCtx,
    /// What this session's `initialize` said: its client name, recorded by
    /// `docs__request`, and whether it is an MCP Apps host.
    session: Session,
}

#[async_trait]
impl ToolPlane for AggregatePlane {
    async fn list_tools(&self) -> Vec<Value> {
        let snap = self.state.snapshot();
        let agg = self.state.mcp.list_tools(&snap).await;
        // rmcp `Tool` serializes camelCase (`inputSchema` etc.) — pass through.
        let mut tools: Vec<Value> = super::docs::list();
        tools.extend(super::kb::list());
        // An app-only tool is for an MCP Apps host's views; a client that
        // declared no such host is offered only what a model may see.
        tools.extend(
            agg.tools
                .iter()
                .filter(|t| self.session.apps_host || super::resources::model_visible(t))
                // A switch set under the name a collision moved it from.
                .filter(|t| !agg.tool_disabled(&snap, t.name.as_ref()))
                .filter_map(super::tasks::meta::listed)
                // Whose it is, so a host can route a view's call of it.
                .map(|mut t| {
                    server_meta::stamp(&mut t, &agg, &snap);
                    t
                }),
        );
        retain_enabled(&snap, &mut tools);
        // The caller's own reach — an agent's manifest, a client key's tool
        // scope — on top of the owner's switch, so a tool the owner switched
        // off stays off for a caller whose list names it.
        let scope = ToolScope::of_request(&self.state, &self.ctx).await;
        tools.retain(|t| {
            t.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| scope.admits(n))
        });
        tools
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Option<Map<String, Value>>,
    ) -> Result<Value, CallError> {
        let started = Instant::now();
        // The caller's own reach first, re-checked rather than trusted from
        // the list, because a list is a snapshot and a call is not (§3.1).
        // First, too, so that a name outside it answers the same whatever the
        // owner's switch says about it — "disabled" would confirm the tool
        // exists to a caller that is not meant to know.
        //
        // `lmgw__*` is skipped here so the "they moved to /mcp/admin" hint
        // below still reaches every caller. Nothing is widened by that: the
        // reserved namespace is off this plane for every caller, and no list
        // can put it back — a scoped caller gets the same refusal everyone
        // else does, with the one sentence that explains it.
        //
        // The server the name routed to as the scope admitted it is the one
        // it runs on (`ToolScope::routed`): a name that changed hands since
        // — to a device this caller does not reach — is refused, not run.
        let mut routed = None;
        if !selfadmin::owns(name) {
            let scope = ToolScope::of_request(&self.state, &self.ctx).await;
            routed = scope.routed(name);
            if !scope.admits(name) {
                let e = CallError::ToolNotFound(scope.refusal(name));
                record_mcp_call(
                    &self.state,
                    &self.ctx,
                    name,
                    None,
                    started,
                    CallOutcome::Failed(&e),
                )
                .await;
                return Err(e);
            }
        }
        // The owner's per-tool switch, ahead of every route below: a name that
        // is off is off whether it belongs to a built-in or to a server.
        if let Some(e) = disabled(&self.state, name) {
            record_mcp_call(
                &self.state,
                &self.ctx,
                name,
                None,
                started,
                CallOutcome::Failed(&e),
            )
            .await;
            return Err(e);
        }
        // The knowledge bases shared on `/mcp` (chat-complete §9.4): only
        // the `mcp_visible` ones exist here.
        // A device's retrieval is charged to it (review W3-3); any other
        // key's stays the gateway's own.
        let charged = crate::devices::charged(&self.ctx);
        if super::kb::owns(name) {
            let result = super::kb::call(
                &self.state,
                name,
                args,
                &super::kb::KbAccess::McpVisible,
                (None, charged.as_ref()),
            )
            .await;
            let (server, outcome) = match &result {
                Ok(v) => (
                    Some(super::exec::KB_SERVER.to_string()),
                    CallOutcome::Completed {
                        tool_error: builtin_tool_error(v),
                    },
                ),
                Err(e) => (None, CallOutcome::Failed(e)),
            };
            record_mcp_call(&self.state, &self.ctx, name, server, started, outcome).await;
            return result;
        }
        if super::docs::owns(name) {
            let result = super::docs::call(
                &self.state,
                name,
                args,
                self.session.client_name.as_deref(),
                charged.as_ref(),
            )
            .await;
            let (server, outcome) = match &result {
                Ok(v) => (
                    Some(DOCS_SERVER_NAME.to_string()),
                    CallOutcome::Completed {
                        tool_error: builtin_tool_error(v),
                    },
                ),
                Err(e) => (None, CallOutcome::Failed(e)),
            };
            record_mcp_call(&self.state, &self.ctx, name, server, started, outcome).await;
            return result;
        }
        // A self-admin name reaching this plane is a client that has not been
        // told the tools moved. Answer with where they went rather than a bare
        // "unknown tool", which would read as "lmgw broke".
        if selfadmin::owns(name) {
            let e = CallError::ToolNotFound(format!(
                "{name} — the lmgw__* self-admin tools are no longer served on /mcp; \
                 they moved to /mcp/admin, which needs the owner:self-admin key \
                 from Usage → Keys"
            ));
            record_mcp_call(
                &self.state,
                &self.ctx,
                name,
                None,
                started,
                CallOutcome::Failed(&e),
            )
            .await;
            return Err(e);
        }

        let snap = self.state.snapshot();
        // `call` resolves the owning server while routing and hands its name back
        // (errors carry it too) — so the log path labels the row without a second
        // O(tools) aggregate rebuild (§10).
        let from = super::host::CallFrom::of(&self.ctx);
        let routed = match routed {
            Some(server) => {
                self.state
                    .mcp
                    .call_listed(&snap, name, server, args, &from)
                    .await
            }
            None => self.state.mcp.call(&snap, name, args, &from).await,
        };
        let (server_name, result): (
            Option<String>,
            Result<rmcp::model::CallToolResult, CallError>,
        ) = match routed {
            Ok((r, sname)) => (Some(sname), Ok(r)),
            Err(e) => (e.server().map(str::to_string), Err(e)),
        };
        let outcome = match &result {
            Err(e) => CallOutcome::Failed(e),
            Ok(r) if r.is_error == Some(true) => CallOutcome::Completed {
                tool_error: Some(content_preview(r)),
            },
            Ok(_) => CallOutcome::Completed { tool_error: None },
        };
        record_mcp_call(&self.state, &self.ctx, name, server_name, started, outcome).await;
        result.map(|r| serde_json::to_value(r).unwrap_or_else(|_| json!({})))
    }

    async fn resources(
        &self,
        method: &str,
        params: &Value,
    ) -> Option<Result<Value, (i64, String)>> {
        resources::serve(&self.state, &self.ctx, method, params).await
    }
}

/// Drop the tools the owner switched off from a `tools/list` array (§ tool
/// inventory). Applied to both planes and to both halves of the aggregate, so
/// there is no route on which a disabled tool is still advertised.
fn retain_enabled(snap: &crate::config::Snapshot, tools: &mut Vec<Value>) {
    tools.retain(|t| {
        t.get("name")
            .and_then(Value::as_str)
            .is_none_or(|n| !snap.tool_disabled(n))
    });
}

/// The typed refusal for a disabled tool, or `None` when it is offered.
/// Listing is not a security boundary on its own — a client can keep a name
/// from an earlier list, so `tools/call` checks it again.
fn disabled(state: &SharedState, name: &str) -> Option<CallError> {
    state
        .snapshot()
        .tool_disabled(name)
        .then(|| CallError::Disabled(name.to_string()))
}

/// Pull the error text out of a built-in tool result flagged `isError`, so a
/// refused or failed self-admin call logs the reason rather than a bare 502.
fn builtin_tool_error(v: &Value) -> Option<String> {
    if v.get("isError") != Some(&Value::Bool(true)) {
        return None;
    }
    let text = v
        .get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("tool returned isError");
    Some(text.to_string())
}

/// Write a `request_logs` row for one northbound `tools/call` (§10).
///
/// **Stats-pollution fix 1 (errors).** A tool/JSON-RPC failure rides inside HTTP
/// 200 and a `CallToolResult{isError:true}` *is* a successful HTTP response, so
/// both would render GREEN under the dashboard's `ok = status < 400 &&
/// error_kind.is_none()`. We therefore synthesize a **non-200 `status` + an
/// `error_kind`** for any failure: a typed [`CallError`] (transport/timeout/
/// not-found) and an `isError: true` result both log as `502` so the unified
/// feed shows them as failures. **Fix 2 (tokens)** lives in `telemetry::stats()`:
/// `ingress_proto = 'mcp'` rows carry NULL tokens and are excluded from the
/// token / error-rate aggregates (here we leave the token columns NULL).
/// One `tools/call`'s outcome reduced to what the log row needs. Both halves of
/// [`GatewayPlane`] funnel through it, so a built-in `lmgw__*` call — including
/// a configuration change — is audited exactly like a proxied one, without the
/// logging path having to know which side produced the result.
enum CallOutcome<'a> {
    /// The call completed. `tool_error` carries the message when the tool
    /// itself reported failure (`isError: true`).
    Completed { tool_error: Option<String> },
    /// Routing, transport or timeout failure — no tool result at all.
    Failed(&'a CallError),
}

async fn record_mcp_call(
    state: &SharedState,
    ctx: &RequestCtx,
    exposed_tool: &str,
    server_name: Option<String>,
    started: Instant,
    outcome: CallOutcome<'_>,
) {
    // Map the outcome to the one thing the row needs: the failure message, if
    // any. A typed CallError and an `isError: true` result both count.
    let completed = matches!(outcome, CallOutcome::Completed { .. });
    let error = match outcome {
        CallOutcome::Failed(e) => Some(e.to_string()),
        CallOutcome::Completed { tool_error } => tool_error,
    };
    // Cost per run counts tool calls too (§3.1, catalog §4.5). No usage: a
    // `tools/call` carries none, which is why `counts_in_token_stats` already
    // excludes the `"mcp"` proto.
    //
    // `Completed` only — a tool that ran and reported `isError` was still
    // called, while a name that was refused (unknown, disabled, off the
    // agent's allow list) never reached a tool at all. A refusal counted here
    // would let a loop of rejected names inflate the run's `tool_calls`.
    if let Some(run) = ctx.run.filter(|_| completed) {
        state.agent_meters.note_tool_call(run);
    }
    record_tool_call(state, ctx, "mcp", exposed_tool, server_name, started, error).await
}

/// First chunk of text from a tool-error result, for the log's `error_msg`.
fn content_preview(r: &rmcp::model::CallToolResult) -> String {
    for block in &r.content {
        if let Ok(v) = serde_json::to_value(block) {
            if let Some(t) = v.get("text").and_then(Value::as_str) {
                return t.to_string();
            }
        }
    }
    "tool returned isError".to_string()
}

// ---------------------------------------------------------------------------
// POST /mcp — the JSON-RPC dispatcher
// ---------------------------------------------------------------------------

async fn mcp_post(
    axum::extract::State(state): axum::extract::State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Origin first: a DNS-rebinding defense that must run regardless of auth.
    // Here rather than inside [`dispatch`] because the verdict now needs the
    // catalog — an agent origin is one of the shapes it admits (§4.2) — and
    // `dispatch` is the protocol half, golden-tested against a fake plane with
    // no state behind it.
    if !origin_allowed(&state, &headers, Plane::Aggregate).await {
        return refuse_origin();
    }
    let session = session_of(&headers);
    let plane = AggregatePlane {
        state,
        ctx,
        session,
    };
    dispatch(&plane, &headers, &body, Plane::Aggregate).await
}

/// `POST /mcp/admin` — the self-admin plane.
///
/// Its credential is `require_admin_token` on the route (principals §3.7): an
/// enabled owner row, in any of the three spellings. What is left here is the
/// **mode** gate — `settings.self_admin` in `Off | ReadOnly | Full`, which
/// filters the tool list and refuses writes — and that is a different
/// question: who may call this plane, versus how much of it a model-driven
/// client is allowed to see.
async fn admin_post(
    axum::extract::State(state): axum::extract::State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The same rebinding defense as the aggregate plane, for the same reason
    // and in the same place.
    if !origin_allowed(&state, &headers, Plane::Admin).await {
        return refuse_origin();
    }
    let plane = AdminPlane { state, ctx };
    dispatch(&plane, &headers, &body, Plane::Admin).await
}

/// Which plane a dispatch is serving — only the `initialize` blurb differs.
#[derive(Clone, Copy, PartialEq)]
enum Plane {
    Aggregate,
    Admin,
}

/// The protocol dispatcher proper, generic over the [`ToolPlane`] so it is
/// golden-testable without a live southbound MCP server (§16). Owns the §6
/// MUST-list; the tool plane owns aggregation + routing.
async fn dispatch(
    plane: &dyn ToolPlane,
    headers: &HeaderMap,
    body: &Bytes,
    which: Plane,
) -> Response {
    // We only emit application/json in MVP, so the client must accept it.
    if !accepts_json(headers) {
        return (StatusCode::NOT_ACCEPTABLE, "must accept application/json").into_response();
    }

    let val: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return bad_request("invalid JSON"),
    };
    // Batching was removed in MCP 2025-06-18; reject arrays outright.
    if val.is_array() {
        return bad_request("JSON-RPC batching is not supported");
    }
    let Some(obj) = val.as_object() else {
        return bad_request("request must be a JSON object");
    };

    let method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").cloned();

    // A notification (method, no id) or a client→server response (no method):
    // acknowledged with 202 and no body.
    match (method, &id) {
        (Some(_), None) | (None, _) => {
            return StatusCode::ACCEPTED.into_response();
        }
        _ => {}
    }
    let method = method.unwrap();
    let id = id.unwrap();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    // Everything except `initialize` requires a negotiated session + version.
    if method != "initialize" {
        if let Some(v) = hget(headers, "mcp-protocol-version") {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return bad_request(&format!("unsupported MCP-Protocol-Version: {v}"));
            }
        }
        match hget(headers, "mcp-session-id") {
            None => return bad_request("missing MCP-Session-Id"),
            Some(sid) if !session_exists(sid) => {
                return (StatusCode::NOT_FOUND, "unknown or terminated session").into_response();
            }
            Some(_) => {}
        }
    }

    match method {
        "initialize" => {
            // Agree on the client's version if we support it, else ours.
            let client_ver = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let agreed = if SUPPORTED_VERSIONS.contains(&client_ver) {
                client_ver
            } else {
                PROTOCOL_VERSION
            };
            let sid = new_session(Session::of_initialize(&params));
            let instructions = match which {
                Plane::Aggregate => {
                    "lmgw MCP gateway: aggregates the tools of every connected upstream MCP \
                     server under per-server `__` prefixes, plus its own `docs__*` and \
                     `kb__*` toolsets. \
                     Open the GET /mcp stream to receive notifications/tools/list_changed when \
                     a server connects/disconnects (newly lazy-connected servers' tools then \
                     appear after you re-list). The servers' resources pass through as well \
                     (resources/list, resources/templates/list, resources/read), a prefixed \
                     server's URIs namespaced before the authority (ui://weather/card from \
                     prefix p is ui://p__weather/card), MCP Apps UI resources included.\n\n\
                     `docs__*` serves this gateway's ingested library documentation. When you \
                     need a library's real API surface rather than what you remember of it: \
                     `docs__resolve` to find the corpus (ids are `library@version`), then \
                     `docs__query` for the sections, which come back as markdown with verbatim \
                     source text and deep links. If no corpus matches, `docs__request` files \
                     the gap with the gateway's owner — it does not ingest anything, so do not \
                     wait on it.\n\n\
                     `kb__*` searches the owner's own knowledge bases (their documents, not \
                     library docs): `kb__list` names them, `kb__search` returns numbered \
                     excerpts with file and page to cite, `kb__read` reads on in a document \
                     from an excerpt's chunk.\n\n\
                     The gateway's own `lmgw__*` self-admin tools are NOT served here — they \
                     are on /mcp/admin, behind a separate token."
                }
                Plane::Admin => {
                    "lmgw self-admin plane: built-in `lmgw__*` tools for inspecting and \
                     configuring this gateway. Start with `lmgw__status`. Whether the \
                     mutating tools are listed depends on the owner's self-admin setting; at \
                     'read only' they are absent and calling one is refused with that \
                     reason. The registered upstream MCP servers' tools are not served here \
                     — they are on /mcp.\n\n\
                     To serve a new local llama.cpp model, the normal path is:\n\
                     1. `lmgw__hf_repo` to see what a Hugging Face repo contains (files come \
                     back classified as weights / mmproj projector / speculative drafter).\n\
                     2. `lmgw__hf_add` to download a weights file together with its \
                     companions, then `lmgw__hf_downloads` until the status is 'done'.\n\
                     3. `lmgw__local_model_plan` on the downloaded weights — it reads the \
                     GGUF metadata and returns a complete, ready-to-apply parameter set with \
                     a rationale for each value, so you do not need to know llama.cpp.\n\
                     4. `lmgw__local_model_set action=create` with those parameters.\n\
                     5. `lmgw__local_model_test` to confirm it actually loads and generates — \
                     it starts the model's own container itself, so there is no separate \
                     'apply' step for a model that has never run before. If a model with that \
                     id might already be running under stale config, run \
                     `lmgw__container model=<id> action=apply` first to recreate it.\n\n\
                     Embedding and rerank models are the AUX class, not chat: they are \
                     encoders (a `pooling_type` or classifier head in the GGUF header, no LM \
                     head), live in their own models directory, and are exposed under the \
                     aux prefix (usually `embed/<id>`). Same path, different target: \
                     `lmgw__hf_add … target=aux`, `lmgw__local_model_plan … target=aux` \
                     (returns kind, pooling and ctx_size read from the header), \
                     `lmgw__aux_model_set action=create`, then `lmgw__local_model_test … \
                     target=aux` (it embeds or reranks a probe input — an encoder cannot \
                     generate). Never create a chat-class row for an embedding model, and \
                     never copy or hard-link a GGUF between the class directories: \
                     `lmgw__gguf_files target=aux` lists the aux dir, and a file only stays \
                     updatable where the downloader put it. CPU-only placement is \
                     `--n-gpu-layers 0` in `extra_args`.\n\n\
                     IMAGE models (stable-diffusion.cpp) are the fourth class and the one \
                     place the hf_add chain does not fit: a pipeline is a diffusion model \
                     (or an all-in-one checkpoint) plus a VAE plus one to three text \
                     encoders, living in DIFFERENT Hugging Face repos, and most of them are \
                     `.safetensors` rather than GGUF. So the path is \
                     `lmgw__image_recipes` (the shipped families, each with every file it \
                     needs, its size, whether the repo is licence-gated and whether it is \
                     already on the box) -> `lmgw__image_recipe_add key=<key>` (queues the \
                     missing components and hands back a prefilled row) -> \
                     `lmgw__hf_downloads` until every queued id says 'done' -> \
                     `lmgw__image_model_set action=create` with that row -> \
                     `lmgw__local_model_test target=image`, which generates one small image. \
                     For a pipeline no recipe covers: `lmgw__hf_add target=image` per file \
                     (it fetches no companions — the components are in other repos), \
                     `lmgw__gguf_files target=image` to see what landed (it lists every kind \
                     sd-server loads, not just GGUF), `lmgw__local_model_plan target=image` \
                     to see whether a recipe matches it after all, and `files` written by \
                     hand otherwise. `edit` on the row is load-bearing, not descriptive: \
                     sd-server segfaults on a reference-image request to a pipeline that \
                     cannot take one, so lmgw refuses /v1/images/edits for every row without \
                     it.\n\n\
                     AUDIO models (audio.cpp: speech recognition, text-to-speech, cloning, \
                     music, …) come as catalog packages: `lmgw__audio_catalog action=list` \
                     (`action=refresh` first when nothing is cached) names each package's \
                     suggested model id, path, task and mode, `action=download` queues its \
                     files, `lmgw__hf_downloads` until done, then `lmgw__audio_model_set \
                     action=create` with those suggestions; prove it with a request to \
                     /v1/audio/speech or /v1/audio/transcriptions on `audio/<id>` and read it \
                     back with `lmgw__local_model_get target=audio`.\n\n\
                     Models already on disk that lmgw did not download are supported — pass \
                     `gguf_path` to `lmgw__local_model_set` (or `lmgw__aux_model_set`) and \
                     use `lmgw__gguf_files` to find them — but that route cannot fetch \
                     anything and finds no companions, so prefer the HF path. Use \
                     `lmgw__model_inspect` to learn what any GGUF is (architecture, context \
                     length, which class serves it, whether the running llama.cpp build \
                     even supports it) and `lmgw__local_model_get` to read back a \
                     configured model of either class, including the exact command line it \
                     renders.\n\n\
                     If `lmgw__status` reports `vram.hold_active: true`, the GPU is held: \
                     requests to a local model with no configured fallback come back as a 503 \
                     `gpu_hold`; audio models that run on the CPU keep serving. \
                     `lmgw__hold_set active=false` releases the hold; \
                     `active=true` engages it and stops idle local containers. The global \
                     fallback (chat-class local models only) is `hold_fallback_alias` on \
                     `lmgw__settings_set`; a per-model one is set on `lmgw__local_model_set` \
                     via `hold_fallback_mode` and `hold_fallback`. While a benchmark run \
                     holds the GPU (`vram.benchmark` on `lmgw__status`, started with \
                     `lmgw__bench_start`), local requests are answered the same way, with a \
                     503 `gpu_benchmark` instead.\n\n\
                     CONTAINER BUILDS: lmgw builds its own engine images (llama.cpp, \
                     ik_llama.cpp, audio.cpp, stable-diffusion.cpp) from git, optionally with \
                     PRs merged on top. `lmgw__builds` lists the builds and the repo presets; \
                     `lmgw__forge_prs` finds PR numbers; `lmgw__build_set action=create \
                     preset=official slug=official-master-pr123 extras='pr 123'` defines one; \
                     `lmgw__build_check_merge id=<id>` says in seconds whether the extras \
                     merge; `lmgw__build_run id=<id>` starts a run (minutes) — follow it with \
                     `lmgw__build_log` and `lmgw__builds id=<id>`. A verified run moves the \
                     build's moving tag; models whose image names that tag pick it up when \
                     recreated (`lmgw__container model=<id> action=apply`), and \
                     `lmgw__local_model_test` proves it serves. `lmgw__container_images` \
                     lists every engine image with its users; `lmgw__container_image_delete` \
                     removes one nothing uses. Builds carry an `update` badge from the periodic \
                     update check (a moved ref, a pushed or merged PR); registry images in use \
                     carry `registry_update`, and `lmgw__container_image_pull` pulls a newer \
                     one."
                }
            };
            // The aggregate passes resources and the MCP Apps metadata
            // through (client-apps design §7.2); the admin plane has tools.
            let capabilities = match which {
                Plane::Aggregate => resources::aggregate_capabilities(),
                Plane::Admin => json!({ "tools": { "listChanged": true } }),
            };
            let result = json!({
                "protocolVersion": agreed,
                "capabilities": capabilities,
                "serverInfo": { "name": "lmgw", "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions,
            });
            json_rpc(StatusCode::OK, Some(&sid), rpc_ok(id, result))
        }
        "ping" => json_rpc(StatusCode::OK, None, rpc_ok(id, json!({}))),
        "tools/list" => {
            let tools = plane.list_tools().await;
            json_rpc(StatusCode::OK, None, rpc_ok(id, json!({ "tools": tools })))
        }
        "tools/call" => {
            // §14 normalization: a malformed `tools/call` is **invalid params**
            // (`-32602`), distinct from a well-formed call for a tool that
            // doesn't exist (`-32601`, decided in the tool plane). An absent /
            // non-string `name`, or a present-but-non-object `arguments`, is the
            // client's mistake — surface it as `-32602` rather than coercing it
            // to an empty name / dropped args and mislabeling it tool-not-found.
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return json_rpc(
                    StatusCode::OK,
                    None,
                    rpc_err(
                        id,
                        -32602,
                        "invalid params: tools/call requires a string `name`",
                    ),
                );
            };
            let name = name.to_string();
            let args = match params.get("arguments") {
                None | Some(Value::Null) => None,
                Some(Value::Object(o)) => Some(o.clone()),
                Some(_) => {
                    return json_rpc(
                        StatusCode::OK,
                        None,
                        rpc_err(
                            id,
                            -32602,
                            "invalid params: tools/call `arguments` must be an object",
                        ),
                    );
                }
            };
            match plane.call_tool(&name, args).await {
                Ok(result) => json_rpc(StatusCode::OK, None, rpc_ok(id, result)),
                Err(e) => {
                    // §14: unknown tool → -32601; transport/timeout → internal.
                    // Always HTTP 200 with a JSON-RPC error object (the surface
                    // is JSON-RPC); the *log row* is what's mapped non-ok (§10).
                    json_rpc(
                        StatusCode::OK,
                        None,
                        rpc_err(id, e.rpc_code(), &e.to_string()),
                    )
                }
            }
        }
        other => match plane.resources(other, &params).await {
            Some(Ok(result)) => json_rpc(StatusCode::OK, None, rpc_ok(id, result)),
            Some(Err((code, message))) => {
                json_rpc(StatusCode::OK, None, rpc_err(id, code, &message))
            }
            None => json_rpc(
                StatusCode::OK,
                None,
                rpc_err(id, -32601, &format!("method not found: {other}")),
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// GET /mcp — server→client SSE channel for `tools/list_changed` push (§6/§8)
// ---------------------------------------------------------------------------

/// `GET /mcp` — open the server→client SSE stream (§6/§8/§9). This is the
/// channel deferred in the M0–M3 spike (which answered `405`); with the
/// northbound `tools/list_changed` broadcast in place (M5), a **valid session**
/// now gets a real stream instead.
///
/// §6 MUST-list, preserved:
/// - `Origin` validation first (DNS-rebinding defense, regardless of auth).
/// - `MCP-Protocol-Version`, when present, must be supported (`400` otherwise) —
///   honored exactly like `POST`.
/// - `MCP-Session-Id` is **required**: missing → `400`, unknown/terminated →
///   `404` (the same session errors `POST` uses). We open a stream **only** for a
///   valid session — so a session-less GET is still a clean `4xx`, never `405`.
///
/// The stream emits JSON-RPC **notifications** (no `id`): nothing on open, then a
/// `notifications/tools/list_changed` frame each time the manager's northbound
/// broadcast fires (a server connects/disconnects/reaps, or an upstream's
/// `on_tool_list_changed`). The client re-`tools/list`s to see the new aggregate.
/// The per-session subscriber is the `BroadcastStream`; it lives exactly as long
/// as the HTTP connection (dropped on client disconnect or `DELETE /mcp`), so it
/// never leaks. Keep-alive comments pace at [`SSE_KEEPALIVE`].
async fn mcp_get(
    axum::extract::State(state): axum::extract::State<SharedState>,
    ctx: Option<axum::Extension<RequestCtx>>,
    headers: HeaderMap,
) -> Response {
    // Origin first — same DNS-rebinding defense as POST, auth-independent.
    if !origin_allowed(&state, &headers, Plane::Aggregate).await {
        return refuse_origin();
    }
    // Honor MCP-Protocol-Version exactly like POST (400 on an unsupported value).
    if let Some(v) = hget(&headers, "mcp-protocol-version") {
        if !SUPPORTED_VERSIONS.contains(&v) {
            return bad_request(&format!("unsupported MCP-Protocol-Version: {v}"));
        }
    }
    // A stream requires a valid session: 400 missing / 404 unknown — the same
    // session errors POST returns. We open a stream only past this gate, so a
    // session-less GET is a clean 4xx, never 405 (the old MVP placeholder).
    match hget(&headers, "mcp-session-id") {
        None => return bad_request("missing MCP-Session-Id"),
        Some(sid) if !session_exists(sid) => {
            return (StatusCode::NOT_FOUND, "unknown or terminated session").into_response();
        }
        Some(_) => {}
    }

    // Subscribe this session to the northbound `tools/list_changed` broadcast.
    // The receiver is owned by the stream below; dropping the stream (client
    // disconnect / DELETE) drops the receiver — no leak.
    //
    // Each nudge, and a lag alike (a burst outran a slow client: one
    // catch-up is correct, a list_changed is idempotent and the client
    // re-lists the current aggregate), is one JSON-RPC notification with no
    // id. A change of the aggregate's composition — a server connecting or
    // going — changes whose resources are listed too, so it says both; an
    // upstream's own `resources/list_changed` says that alone.
    let rx = state.mcp.subscribe_tools_changed();
    let tools = BroadcastStream::new(rx).flat_map(|_| {
        stream::iter([
            Ok::<SseFrame, Infallible>(SseFrame::default().data(tools_list_changed_notification())),
            Ok(SseFrame::default().data(resources::resources_list_changed_notification())),
        ])
    });
    let rx = state.mcp.subscribe_resources_changed();
    let resources = BroadcastStream::new(rx)
        .map(|_| Ok(SseFrame::default().data(resources::resources_list_changed_notification())));
    let live = stream::select(tools, resources);
    // Nothing is emitted on open (the spec's "starting with nothing"); the first
    // frame is the first change. `stream::empty()` chained keeps the type a plain
    // notification stream.
    let stream = stream::empty::<Result<SseFrame, Infallible>>().chain(live);
    // The key it was opened with revoked — disabled, rotated, deleted,
    // expired — ends it (client-apps design §1.6, review W2-2): a stream that
    // outlived its credential tells the client nothing more. No frame says
    // so: MCP has no notification for it, and the client's next POST meets
    // the gate's refusal.
    let served_at = ctx.as_ref().and_then(|axum::Extension(ctx)| ctx.served_at);
    let watch = ctx.and_then(|axum::Extension(ctx)| {
        crate::devices::watch(&state, &ctx.principal, ctx.revocation_mark)
    });
    let stream = crate::devices::until_revoked(stream, watch, |_| None);
    // And it ends when the server it came in on stops, likewise without a
    // frame.
    let stream = crate::server::until_stopped(&state.stops, served_at, stream, None);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE))
        .into_response()
}

/// The hand-built `notifications/tools/list_changed` JSON-RPC notification body
/// (§6) — a serialized string, the SSE frame's `data`. Hand-built JSON (not an
/// rmcp wire type) keeps `rmcp` confined to the `mcp` module per §19; a
/// notification has **no `id`** (it's not a request).
fn tools_list_changed_notification() -> String {
    json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }).to_string()
}

// ---------------------------------------------------------------------------
// DELETE /mcp — terminate a session
// ---------------------------------------------------------------------------

async fn mcp_delete(headers: HeaderMap) -> Response {
    match hget(&headers, "mcp-session-id") {
        None => bad_request("missing MCP-Session-Id"),
        Some(sid) => {
            if SESSIONS.lock().unwrap().remove(sid).is_some() {
                StatusCode::NO_CONTENT.into_response()
            } else {
                (StatusCode::NOT_FOUND, "unknown session").into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_rpc(status: StatusCode, session: Option<&str>, body: Value) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(sid) = session {
        builder = builder.header("MCP-Session-Id", sid);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
        .unwrap()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn hget<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// Accept missing or allowing `application/json` (or `*/*` / `application/*`).
/// Lenient on purpose: we only return JSON, and over-strict negotiation would
/// reject otherwise-valid clients.
fn accepts_json(h: &HeaderMap) -> bool {
    match hget(h, "accept") {
        None => true,
        Some(a) => {
            a.contains("application/json") || a.contains("*/*") || a.contains("application/*")
        }
    }
}

fn refuse_origin() -> Response {
    (StatusCode::FORBIDDEN, "origin not allowed").into_response()
}

/// Non-browser MCP clients (Claude Code, curl) omit `Origin` → allowed. A present
/// `Origin` is allowed for loopback — and, on the aggregate plane only, since
/// origins §4.2, for an agent's own origin (`http://<id>.<suffix>[:<port>]` for
/// an agent that declares `run.service`), so an agent's UI can call `/mcp` from
/// the browser with a bearer its backend gave it. Everything else is refused:
/// this is the DNS-rebinding defense, and a page on `evil.example` is exactly
/// what it is for.
///
/// **The port is part of the origin.** `http://localhost:9999` is another
/// process on this box, not this gateway, and a page it serves is as foreign as
/// one on `evil.example` — so a port, when the `Origin` carries one, must be
/// lmgw's own ([`own_port`](crate::agents::service::own_port)). An `Origin`
/// with no port at all names port 80 and is left to the host test, which is
/// where it was before.
///
/// **`/mcp/admin` keeps the loopback list** (§4.2): the agent shape exists so
/// an agent's own page can reach the aggregate plane with the bearer its
/// backend holds, and the self-admin plane is not something an agent's page is
/// ever the right caller for. It is a second lock on a route already behind an
/// owner credential, and it costs an agent nothing it was meant to have.
///
/// The agent shape costs one lookup by id, on a request that carries an
/// `Origin` at all — a browser's request, not the hot path lmgw's own clients
/// take.
async fn origin_allowed(state: &SharedState, h: &HeaderMap, plane: Plane) -> bool {
    let Some(origin) = hget(h, "origin") else {
        return true;
    };
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("");
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            // ipv6 [host]:port
            Some((host, after)) => (host, after.strip_prefix(':')),
            None => (rest, None),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let snap = state.snapshot();
    if let Some(port) = port {
        let own = crate::agents::service::own_port(&snap.settings.bind_addr);
        match (port.parse::<u16>().ok(), own) {
            (Some(p), Some(own)) if p == own => {}
            // A port that is not lmgw's, or one of the two that cannot be
            // read at all: nothing here is the gateway's own origin.
            _ => return false,
        }
    }
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    if plane == Plane::Admin {
        return false;
    }
    match crate::agents::service::origin_label(&snap.settings, host) {
        Some(label) => crate::agents::service::is_service_agent(state, &label).await,
        None => false,
    }
}

fn session_exists(sid: &str) -> bool {
    SESSIONS.lock().unwrap().contains_key(sid)
}

fn new_session(session: Session) -> String {
    let sid = crate::web::rand_hex32();
    SESSIONS.lock().unwrap().insert(sid.clone(), session);
    sid
}

/// What the `initialize` of the session this request carries said; the
/// default (no name, no apps host) without one. Read from the header rather
/// than threaded through [`ToolPlane`]: the session id is on every request
/// that has one.
fn session_of(headers: &HeaderMap) -> Session {
    hget(headers, "mcp-session-id")
        .and_then(|sid| SESSIONS.lock().unwrap().get(sid).cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake tool plane for the golden dispatch tests (§16): one tool
    /// `time__now`, a happy-path call, and a not-found for anything else — no
    /// live MCP server, no `McpManager`.
    struct FakePlane;

    #[async_trait]
    impl ToolPlane for FakePlane {
        async fn list_tools(&self) -> Vec<Value> {
            vec![json!({
                "name": "time__now",
                "description": "current time",
                "inputSchema": { "type": "object", "properties": {} }
            })]
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Option<Map<String, Value>>,
        ) -> Result<Value, CallError> {
            if name == "time__now" {
                Ok(json!({
                    "content": [ { "type": "text", "text": "2026-06-30T00:00:00Z" } ],
                    "isError": false
                }))
            } else {
                Err(CallError::ToolNotFound(name.to_string()))
            }
        }
    }

    fn body(v: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&v).unwrap())
    }

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("accept", "application/json".parse().unwrap());
        h
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn initialize_returns_capabilities_and_session() {
        let plane = FakePlane;
        let resp = dispatch(
            &plane,
            &json_headers(),
            &body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25" } })),
            Plane::Aggregate,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key("mcp-session-id"));
        let v = body_json(resp).await;
        assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
        assert!(v["result"]["capabilities"]["tools"]["listChanged"]
            .as_bool()
            .unwrap());
    }

    /// Resources and the MCP Apps extension are the aggregate plane's
    /// (client-apps design §7.2); the admin plane offers tools only, and a
    /// plane that serves no resources answers `resources/*` as unknown.
    #[tokio::test]
    async fn only_the_aggregate_plane_offers_resources() {
        let plane = FakePlane;
        for (which, offers) in [(Plane::Aggregate, true), (Plane::Admin, false)] {
            let resp = dispatch(
                &plane,
                &json_headers(),
                &body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } })),
                which,
            )
            .await;
            let v = body_json(resp).await;
            let caps = &v["result"]["capabilities"];
            assert_eq!(caps["tools"]["listChanged"], true, "{v}");
            assert_eq!(caps["resources"].is_object(), offers, "{v}");
            assert_eq!(
                caps["extensions"]["io.modelcontextprotocol/ui"]["mimeTypes"]
                    == json!(["text/html;profile=mcp-app"]),
                offers,
                "{v}"
            );
        }
        let h = session_headers(&plane).await;
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" })),
            Plane::Admin,
        )
        .await;
        assert_eq!(body_json(resp).await["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_list_returns_the_aggregate() {
        // initialize first to mint a session, then list with it.
        let plane = FakePlane;
        let init = dispatch(
            &plane,
            &json_headers(),
            &body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })),
            Plane::Aggregate,
        )
        .await;
        let sid = init
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let mut h = json_headers();
        h.insert("mcp-session-id", sid.parse().unwrap());
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })),
            Plane::Aggregate,
        )
        .await;
        let v = body_json(resp).await;
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "time__now");
    }

    #[tokio::test]
    async fn tools_call_happy_path_and_not_found() {
        let plane = FakePlane;
        let init = dispatch(
            &plane,
            &json_headers(),
            &body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })),
            Plane::Aggregate,
        )
        .await;
        let sid = init
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let mut h = json_headers();
        h.insert("mcp-session-id", sid.parse().unwrap());

        // Happy path: exposed name routes, result passes through verbatim.
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "time__now", "arguments": {} } })),
            Plane::Aggregate,
        )
        .await;
        let v = body_json(resp).await;
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["content"][0]["text"], "2026-06-30T00:00:00Z");

        // Unknown tool → JSON-RPC method-not-found (-32601), HTTP 200.
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "nope", "arguments": {} } })),
            Plane::Aggregate,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], -32601);
    }

    /// Mint a session through `initialize` and return headers carrying it (for
    /// the malformed-`tools/call` tests below).
    async fn session_headers(plane: &dyn ToolPlane) -> HeaderMap {
        let init = dispatch(
            plane,
            &json_headers(),
            &body(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })),
            Plane::Aggregate,
        )
        .await;
        let sid = init
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let mut h = json_headers();
        h.insert("mcp-session-id", sid.parse().unwrap());
        h
    }

    /// §14 normalization: a `tools/call` with no `name` (or a non-string one) is
    /// **invalid params** (`-32602`), not tool-not-found (`-32601`) — the request
    /// is malformed, not a lookup miss.
    #[tokio::test]
    async fn tools_call_missing_name_is_invalid_params() {
        let plane = FakePlane;
        let h = session_headers(&plane).await;

        // Absent name.
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "arguments": {} } })),
            Plane::Aggregate,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], -32602, "missing name → -32602: {v}");

        // Non-string name.
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": 42, "arguments": {} } })),
            Plane::Aggregate,
        )
        .await;
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], -32602, "non-string name → -32602: {v}");
    }

    /// §14 normalization: a present-but-non-object `arguments` is invalid params
    /// (`-32602`), not silently dropped to `None`. Absent/null `arguments` is
    /// still fine (→ no args).
    #[tokio::test]
    async fn tools_call_nonobject_arguments_is_invalid_params() {
        let plane = FakePlane;
        let h = session_headers(&plane).await;

        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "time__now", "arguments": [1, 2, 3] } })),
            Plane::Aggregate,
        )
        .await;
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], -32602, "array arguments → -32602: {v}");

        // Absent arguments is allowed (the tool runs with no args).
        let resp = dispatch(
            &plane,
            &h,
            &body(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "time__now" } })),
            Plane::Aggregate,
        )
        .await;
        let v = body_json(resp).await;
        assert_eq!(v["result"]["isError"], false, "absent args is valid: {v}");
    }
}
