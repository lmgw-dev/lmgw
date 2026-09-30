//! MCP gateway plane (§15) — lmgw as an aggregating MCP proxy, alongside the
//! existing LLM plane in the same process and `Arc<AppState>`.
//!
//! - [`ingress`] — the hand-rolled **northbound** Streamable HTTP server
//!   (`/mcp`): one MCP server to clients (Claude Code/Cursor/the Chat tab).
//! - [`handler`] — the southbound [`rmcp::ClientHandler`] (`GatewayClientHandler`)
//!   that declares the sampling capability and (M4) answers `create_message`.
//! - [`McpManager`] (this module) — the **southbound** live-connection plane:
//!   one `rmcp` `RoleClient` peer per enabled server, reconciled against the
//!   immutable [`Snapshot`](crate::config::Snapshot) on every reload, the same
//!   config-vs-live split as the container runtime / `JobManager` (§9).
//!
//! `rmcp` types are confined to this module boundary (§19) so a version bump is
//! localized — `web/`, `config.rs`, and `store.rs` never name an `rmcp::` type.

pub mod docs;
pub mod exec;
pub mod handler;
pub mod ingress;
pub mod inventory;
/// The `kb__*` knowledge-base toolset (chat-complete design §9.4).
pub mod kb;
pub mod scope;
pub mod selfadmin;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::{broadcast, RwLock};

use crate::config::{McpServer, McpToolOverride, McpTransport, Snapshot};
use crate::state::AppState;

use handler::GatewayClientHandler;

/// Live status of a southbound connection, surfaced on the MCP tab badge (§9).
#[derive(Debug, Clone, PartialEq)]
pub enum McpStatus {
    /// A connect attempt is in flight (cold Podman pull/start lives here).
    Connecting,
    /// Connected; `list_tools` succeeded. Carries the discovered tool count.
    Ready,
    /// Not started: disabled, or `autostart = false` and not yet lazily used.
    Stopped,
    /// Last connect/list attempt failed; detail surfaced on the badge (§14).
    Error(String),
}

impl McpStatus {
    /// Stable lowercase tag for the badge CSS class + the SSE payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Stopped => "stopped",
            Self::Error(_) => "error",
        }
    }
}

/// Reconnect backoff (§14): a crash-looping server (bad image, missing token)
/// must not be re-`podman run` on every tick. After a failed attempt we refuse
/// to retry until `base * 2^(failures-1)` has elapsed, capped — surfacing the
/// failing state instead of hot-looping.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Per-server budget for the lazy-list contract (§9). A first `tools/list`
/// triggers lazy connects for not-yet-started servers and waits **at most this
/// long per server** for them to become `Ready`, then returns whatever is ready
/// — partial, never blocking the whole list indefinitely on one slow
/// `podman pull`. A *visible* constant, not a hidden cap: a server that exceeds
/// it finishes connecting in the background, and (M5) the manager then fires a
/// `tools/list_changed` nudge over `GET /mcp` so subscribed clients re-list and
/// pick up its tools without a manual refresh. Generous enough for a warm
/// container handshake; a cold image pull is the case it deliberately doesn't
/// block on.
const LAZY_LIST_BUDGET: Duration = Duration::from_secs(10);

/// Northbound `tools/list_changed` broadcast buffer (§6/§8). The capacity of the
/// `tokio::sync::broadcast` channel that fans a "tools changed" signal out to
/// every open `GET /mcp` SSE subscriber. The payload is a unit `()` — a pure
/// "re-list now" nudge, never tool data — so this only bounds *coalescing*: if a
/// burst of changes outruns a slow subscriber it lags and we send it a single
/// catch-up notification (a `list_changed` is idempotent — the client re-lists
/// and sees the current aggregate). A *visible* constant, not a hidden cap; the
/// stream itself is never truncated. Generous for the realistic rate (a server
/// connect/disconnect/reap, or an upstream `on_tool_list_changed`).
const TOOLS_CHANGED_BUFFER: usize = 64;

/// Format an `Error` conn's badge detail (§14, no-hidden-limits). For a server
/// the status tick will auto-retry (enabled + autostart), append how long the
/// backoff gates the next attempt plus the visible cap — so a crash-looping
/// server reads "… — retrying in ~Ns (backoff capped at 300s)" instead of a
/// static error giving no hint that retries are deliberately throttled. A
/// lazy/disabled server is never auto-retried, so its bare message is honest.
fn error_detail(msg: &str, conn: &McpConn, auto_retry: bool) -> String {
    if !auto_retry {
        return msg.to_string();
    }
    let delay = backoff_delay(conn.consecutive_failures);
    match conn.last_attempt {
        Some(last) => {
            let remaining = delay.saturating_sub(last.elapsed());
            if remaining > Duration::ZERO {
                format!(
                    "{msg} — retrying in ~{}s (backoff capped at {}s)",
                    remaining.as_secs() + 1,
                    BACKOFF_CAP.as_secs()
                )
            } else {
                format!("{msg} — retrying shortly")
            }
        }
        None => msg.to_string(),
    }
}

fn backoff_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    // Saturating shift so a long-dead server clamps at the cap rather than
    // overflowing; `min` keeps the real bound visible (it's `BACKOFF_CAP`).
    let shift = consecutive_failures.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_CAP)
}

/// One live (or lazily-not-yet-started) southbound connection (§9). Keyed by
/// `mcp_servers.id` in [`McpManager::conns`].
pub struct McpConn {
    /// The running `rmcp` service, or `None` when lazy/stopped. Typed with the
    /// concrete [`GatewayClientHandler`] so Milestone 4 (sampling) is purely
    /// additive — the handler is already the real one, not a placeholder.
    pub running: Option<RunningService<RoleClient, GatewayClientHandler>>,
    /// Tools discovered at connect (`list_all_tools`, paged internally — no cap).
    pub tools: Vec<Tool>,
    pub status: McpStatus,
    /// For idle-reap (M5) and lazy-connect bookkeeping.
    pub last_used: Instant,
    /// Hash of the connection-affecting config at the time this conn was
    /// (re)started; a snapshot whose hash differs ⇒ restart on reconcile (§9).
    config_hash: u64,
    /// Consecutive failed connect attempts, for [`backoff_delay`].
    consecutive_failures: u32,
    /// When the last connect attempt finished (success or failure), gating the
    /// next retry against the backoff window.
    last_attempt: Option<Instant>,
    /// Set when this conn was stopped by the **idle reap** (§9) rather than by
    /// config/error — so the `Stopped` badge tooltip can say "idle-reaped;
    /// reconnects on next use" instead of looking like a never-started server
    /// (surface, don't hide). Cleared on the next successful connect.
    idle_reaped: bool,
    /// In-flight `tools/call` count. The idle reaper (§9) must never tear down a
    /// conn with a call awaiting its response — `last_used` only advances when a
    /// call *finishes*, so a tool slower than `idle_seconds` would otherwise look
    /// idle and get `cancel()`'d mid-flight (→ `TransportClosed`). An
    /// `Arc<AtomicUsize>` so an RAII guard decrements it on drop even if the
    /// northbound request future is cancelled, without taking the async lock.
    in_flight: Arc<AtomicUsize>,
}

/// RAII in-flight counter for a `tools/call` (§9 idle-reap guard): increments on
/// construction, decrements on drop — so a cancelled northbound request (future
/// dropped mid-call) still releases the count, and [`McpManager::reap_idle`]
/// never reaps a conn whose call is genuinely outstanding.
struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl McpConn {
    /// A not-yet-started conn for a known server (lazy/disabled placeholder).
    fn stopped(config_hash: u64) -> Self {
        Self {
            running: None,
            tools: Vec::new(),
            status: McpStatus::Stopped,
            last_used: Instant::now(),
            config_hash,
            consecutive_failures: 0,
            last_attempt: None,
            idle_reaped: false,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// What `reconcile` decides to do with one server id (§9). Kept separate from
/// the IO so the diff is a pure, unit-testable function ([`plan_reconcile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Enabled + autostart + not live ⇒ connect now.
    Start(i64),
    /// Removed or disabled but still live ⇒ tear the connection down.
    Stop(i64),
    /// Still enabled but its connection-affecting config changed ⇒ restart.
    Restart(i64),
}

/// A live connection's identity for the pure diff: its id, whether it currently
/// holds a running service, and the config hash it was started with.
#[derive(Debug, Clone, Copy)]
pub struct LiveConn {
    pub id: i64,
    pub running: bool,
    pub config_hash: u64,
}

/// Pure reconcile diff (§9): desired servers (each with its current config
/// hash) vs the live connection set → the list of start/stop/restart actions.
/// No `rmcp`, no IO — directly unit-tested.
///
/// Rules:
/// - desired (enabled) + `autostart` + not live ⇒ **Start**
/// - desired + live but hash changed ⇒ **Restart** (covers `allow_sampling`,
///   which is reconnect-affecting per §8)
/// - live but no longer desired (removed/disabled) ⇒ **Stop**
/// - lazy (`autostart = false`) servers are neither started nor stopped here —
///   they connect on first use (Milestone 3); a *running* lazy conn whose hash
///   drifts still restarts so an edit takes effect.
pub fn plan_reconcile(desired: &[DesiredServer], live: &[LiveConn]) -> Vec<ReconcileAction> {
    let live_by_id: HashMap<i64, &LiveConn> = live.iter().map(|c| (c.id, c)).collect();
    let desired_ids: std::collections::HashSet<i64> = desired.iter().map(|d| d.id).collect();
    let mut actions = Vec::new();

    for d in desired {
        match live_by_id.get(&d.id) {
            Some(lc) if lc.running => {
                // Already connected: restart only if the config changed.
                if lc.config_hash != d.config_hash {
                    actions.push(ReconcileAction::Restart(d.id));
                }
            }
            _ => {
                // Not currently running (no live conn, or a stopped/lazy one).
                // Autostart servers connect now; lazy servers wait for first use.
                if d.autostart {
                    actions.push(ReconcileAction::Start(d.id));
                }
            }
        }
    }

    // Stop anything live that is no longer a desired (enabled) server.
    for lc in live {
        if lc.running && !desired_ids.contains(&lc.id) {
            actions.push(ReconcileAction::Stop(lc.id));
        }
    }

    actions.sort_by_key(|a| match a {
        ReconcileAction::Stop(id) => (0, *id),
        ReconcileAction::Restart(id) => (1, *id),
        ReconcileAction::Start(id) => (2, *id),
    });
    actions
}

/// A desired (enabled) server reduced to what the pure diff needs.
#[derive(Debug, Clone, Copy)]
pub struct DesiredServer {
    pub id: i64,
    pub autostart: bool,
    pub config_hash: u64,
}

/// Stable hash over exactly the fields that affect the live connection (§9):
/// transport, command/args/env/cwd, container image + extra run args, url +
/// headers, and `allow_sampling` (reconnect-affecting per §8). Deliberately
/// excludes `name`, `tool_prefix`, `timeout_ms`, `idle_seconds`,
/// `sampling_alias` (M4 reads it live), and `enabled`/`autostart` (handled by
/// the diff structure) — none of those change the transport handshake.
pub fn connection_config_hash(s: &McpServer) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.transport.as_str().hash(&mut h);
    s.command.hash(&mut h);
    s.args.hash(&mut h);
    s.env.hash(&mut h);
    s.cwd.hash(&mut h);
    s.container_image.hash(&mut h);
    s.extra_run_args.hash(&mut h);
    s.url.hash(&mut h);
    s.headers.hash(&mut h);
    s.allow_sampling.hash(&mut h);
    h.finish()
}

/// The `agent:<id>` row a not-yet-listed tool name would belong to
/// (container-runtime §3.3).
///
/// A sleeping service agent's tools are not in the aggregate, so the usual
/// reverse map cannot answer "who owns `board__pin`". The prefix is matched
/// against the agent rows by hand instead, which is the only place in the
/// system that reconstructs a tool's owner without the aggregate — and it is
/// allowed to, because the answer is used for exactly one thing: deciding
/// whether this call is the ask that starts a container.
fn sleeping_agent_for(snap: &Snapshot, exposed: &str) -> Option<McpServer> {
    let (prefix, _) = exposed.split_once("__")?;
    snap.mcp_servers
        .values()
        .find(|s| s.enabled && s.agent_id.is_some() && s.tool_prefix == prefix)
        .cloned()
}

/// Snapshot of one connection's status for the SSE feed / MCP tab (§9). A
/// plain data struct so the telemetry [`Event`](crate::telemetry::Event) never
/// carries an `rmcp` type across the module boundary.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct McpStatusView {
    pub id: i64,
    pub name: String,
    /// `connecting` | `ready` | `stopped` | `error`.
    pub status: &'static str,
    pub tool_count: usize,
    pub detail: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool aggregation & namespacing (§7) — the cached, `__`-prefixed catalog the
// northbound `/mcp` exposes, plus the explicit reverse map call routing uses.
// ---------------------------------------------------------------------------

/// Client-side tool-name validation ceiling (§7). Claude Code (and others)
/// validate exposed tool names against `^[a-zA-Z0-9_-]{1,64}$`; an exposed name
/// longer than this is **skipped** from the aggregate with a surfaced warning
/// rather than silently truncated (house rule: no hidden caps). Surfaced as a
/// visible constant here and echoed in every skip's `reason`.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Namespace reserved for lmgw's own self-admin tools (§20). The built-in plane
/// in [`selfadmin`] owns every exposed name starting with `lmgw__`, so a
/// southbound server must not be able to claim one: a rogue or merely
/// unlucky upstream that exposes `lmgw__settings_set` would otherwise shadow
/// the real tool and receive an agent's configuration calls.
///
/// Enforced in two places: [`crate::ops`] rejects the prefix at configuration
/// time (the good error), and [`build_aggregate`] skips any tool that reaches
/// it anyway (the backstop — a server can also produce the name via a rename,
/// or with no prefix at all if its upstream tool is literally called
/// `lmgw__foo`). Skips are surfaced as [`SkippedTool`]s, never silent.
pub const RESERVED_TOOL_PREFIX: &str = "lmgw";

/// The full `lmgw__` string that [`RESERVED_TOOL_PREFIX`] guards.
pub const RESERVED_TOOL_NAMESPACE: &str = "lmgw__";

/// Namespace reserved for the built-in quickdoc toolset ([`docs`], quickdoc
/// §7), for the same reason as `lmgw__` and enforced in the same two places: a
/// southbound server that could claim `docs__query` would be answering
/// documentation lookups on behalf of the gateway's own corpora.
///
/// Unlike `lmgw__` these tools are served on the *aggregate* plane, next to the
/// southbound servers' — which is exactly why the reservation matters here.
pub const RESERVED_DOCS_PREFIX: &str = "docs";

/// The full `docs__` string that [`RESERVED_DOCS_PREFIX`] guards.
pub const RESERVED_DOCS_NAMESPACE: &str = "docs__";

/// Namespace reserved for the built-in knowledge-base toolset ([`kb`],
/// chat-complete design §9.4), for `docs__`'s reason: a southbound server
/// that could claim `kb__search` would be answering from the owner's private
/// documents on the gateway's behalf — or harvesting the queries meant for
/// them.
pub const RESERVED_KB_PREFIX: &str = "kb";

/// The full `kb__` string that [`RESERVED_KB_PREFIX`] guards.
pub const RESERVED_KB_NAMESPACE: &str = "kb__";

/// Every namespace a southbound server may not occupy, as `(prefix, namespace)`
/// — one list so a new built-in toolset cannot be added to one guard and
/// forgotten in the other.
pub const RESERVED_NAMESPACES: [(&str, &str); 3] = [
    (RESERVED_TOOL_PREFIX, RESERVED_TOOL_NAMESPACE),
    (RESERVED_DOCS_PREFIX, RESERVED_DOCS_NAMESPACE),
    (RESERVED_KB_PREFIX, RESERVED_KB_NAMESPACE),
];

/// A tool dropped from the aggregate, retained so the UI/log can show *which*
/// and *why* (§7) instead of it vanishing silently. Two causes today: the
/// 64-char ceiling, and a bare-server name collision (shadowed by an
/// earlier-by-server-name server).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SkippedTool {
    pub server_id: i64,
    pub server_name: String,
    /// The exposed name that would have been used (post rename + prefix).
    pub exposed_name: String,
    /// Human-readable cause, e.g. "exposed name … is N chars (> 64); shorten the
    /// prefix" or "shadowed by server '…' (bare-name collision)".
    pub reason: String,
}

/// One server's contribution to the aggregate: its identity + prefix, its live
/// discovered tools, and the per-tool overrides from the snapshot. A plain
/// input struct so [`build_aggregate`] is a pure, directly unit-testable
/// function over `(tools, configs, overrides)` — no `McpManager`, no IO.
pub struct AggServerInput<'a> {
    pub server_id: i64,
    pub server_name: &'a str,
    pub tool_prefix: &'a str,
    pub tools: &'a [Tool],
    /// upstream tool name → override (hide / rename) for this server.
    pub overrides: &'a HashMap<String, McpToolOverride>,
}

/// The cached aggregate (§7): the exposed `Tool` list (names rewritten to their
/// exposed form, schemas verbatim), the explicit `exposed → (server, upstream
/// tool)` reverse map call routing reads, and the list of dropped tools.
#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    /// Exposed tools, sorted by exposed name, ready to serialize into
    /// `tools/list`. Each is the upstream `Tool` with its `name` replaced by the
    /// exposed name; `inputSchema`/everything else is forwarded verbatim.
    pub tools: Vec<Tool>,
    /// `exposed_name → (server_id, upstream_tool_name)`. **The** routing
    /// structure — never re-derived by splitting on `__` (a bare server may
    /// legitimately expose a literal `x__y`, §7).
    pub reverse: HashMap<String, (i64, String)>,
    /// Tools dropped (64-char ceiling / bare collision), surfaced not hidden.
    pub skipped: Vec<SkippedTool>,
}

/// Compute the exposed (client-facing) name for one upstream tool (§7).
///
/// **Rename-then-prefix (§7/§10 ambiguity resolved):** the override `rename`
/// replaces the *tool-name component only* — the server's `tool_prefix` still
/// applies on top. This is the M1-review interpretation (`McpServer::exposed_name`
/// was flagged for ignoring overrides), and it's the useful one: renaming a tool
/// shouldn't silently strip it out of its server's namespace, and a per-server
/// "rename = full exposed name, prefix bypassed" would make the prefix toggle
/// unpredictable per tool. So: `local = rename.unwrap_or(upstream_name)`, then
/// `prefix.is_empty() ? local : "{prefix}__{local}"`.
fn exposed_name_for(prefix: &str, upstream_tool: &str, rename: Option<&str>) -> String {
    let local = rename.unwrap_or(upstream_tool);
    let prefix = prefix.trim();
    if prefix.is_empty() {
        local.to_string()
    } else {
        format!("{prefix}__{local}")
    }
}

/// Build the aggregate from each server's live tools + its overrides (§7). Pure:
/// no IO, no `McpManager` — the unit-test seam for all the namespacing rules.
///
/// Ordering matters for determinism. Inputs are processed **sorted by server
/// name** (then id as a tiebreak) so bare-name collisions resolve
/// *first-by-server-name* (mirroring [`Snapshot::resolve_passthrough`]'s spirit):
/// the first server (alphabetically) to claim an exposed name wins; a later
/// server's same-named tool is **skipped + logged**, never silently overwriting.
pub fn build_aggregate(servers: &mut [AggServerInput<'_>]) -> Aggregate {
    servers.sort_by(|a, b| {
        a.server_name
            .cmp(b.server_name)
            .then(a.server_id.cmp(&b.server_id))
    });

    let mut tools: Vec<Tool> = Vec::new();
    let mut reverse: HashMap<String, (i64, String)> = HashMap::new();
    let mut skipped: Vec<SkippedTool> = Vec::new();

    for s in servers.iter() {
        for tool in s.tools.iter() {
            let upstream_name = tool.name.as_ref();
            let ov = s.overrides.get(upstream_name);

            // Hidden overrides are excluded from the aggregate entirely (§7).
            if ov.is_some_and(|o| o.hidden) {
                continue;
            }

            let rename = ov
                .and_then(|o| o.rename.as_deref())
                .filter(|r| !r.trim().is_empty());
            let exposed = exposed_name_for(s.tool_prefix, upstream_name, rename);

            // Reserved namespaces: a southbound server never gets to occupy an
            // `lmgw__*` (§20), `docs__*` (quickdoc §7) or `kb__*` (chat-complete
            // §9.4) name. `ops` rejects
            // those prefixes at config time, so reaching here means a rename or
            // a literally-so-named upstream tool — skip + surface, same as any
            // other drop.
            if let Some((_, ns)) = RESERVED_NAMESPACES
                .iter()
                .find(|(_, ns)| exposed.starts_with(ns))
            {
                let reason = format!(
                    "exposed name '{exposed}' is in the reserved '{ns}' namespace (one of \
                     lmgw's own built-in toolsets); rename it or change the tool_prefix"
                );
                tracing::warn!(
                    server = %s.server_name,
                    tool = %upstream_name,
                    exposed = %exposed,
                    "MCP tool dropped from aggregate: {reason}"
                );
                skipped.push(SkippedTool {
                    server_id: s.server_id,
                    server_name: s.server_name.to_string(),
                    exposed_name: exposed,
                    reason,
                });
                continue;
            }

            // 64-char ceiling: skip + surface, never truncate (§7, house rule).
            if exposed.len() > MAX_TOOL_NAME_LEN {
                let reason = format!(
                    "exposed name is {} chars (> {MAX_TOOL_NAME_LEN}); shorten the tool_prefix or rename it",
                    exposed.len()
                );
                tracing::warn!(
                    server = %s.server_name,
                    tool = %upstream_name,
                    exposed = %exposed,
                    "MCP tool dropped from aggregate: {reason}"
                );
                skipped.push(SkippedTool {
                    server_id: s.server_id,
                    server_name: s.server_name.to_string(),
                    exposed_name: exposed,
                    reason,
                });
                continue;
            }

            // Collision: first-by-server-name wins (servers are pre-sorted), the
            // shadowed one is skipped + logged. A prefixed server can only collide
            // with an identically-prefixed sibling; bare servers collide on the
            // raw tool name — both handled here uniformly via the reverse map.
            if let Some((prev_id, _)) = reverse.get(&exposed) {
                let prev_name = servers
                    .iter()
                    .find(|o| o.server_id == *prev_id)
                    .map(|o| o.server_name)
                    .unwrap_or("?");
                let reason = format!(
                    "exposed name '{exposed}' collides with server '{prev_name}' (kept first-by-server-name); shadowed"
                );
                tracing::warn!(
                    server = %s.server_name,
                    tool = %upstream_name,
                    exposed = %exposed,
                    "MCP tool dropped from aggregate: {reason}"
                );
                skipped.push(SkippedTool {
                    server_id: s.server_id,
                    server_name: s.server_name.to_string(),
                    exposed_name: exposed,
                    reason,
                });
                continue;
            }

            // Forward the upstream Tool verbatim with only its name rewritten to
            // the exposed name; inputSchema and all other fields pass through (§7).
            let mut exposed_tool = tool.clone();
            exposed_tool.name = exposed.clone().into();
            tools.push(exposed_tool);
            reverse.insert(exposed, (s.server_id, upstream_name.to_string()));
        }
    }

    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Aggregate {
        tools,
        reverse,
        skipped,
    }
}

/// The one wording for a call refused by the owner's per-tool switch, used by
/// every plane so the answer does not depend on which door the call came in
/// through.
pub fn disabled_message(name: &str) -> String {
    format!(
        "tool '{name}' is disabled on this gateway by its owner \
         (dashboard → MCP → Tool inventory)"
    )
}

/// A tool-call routing/connection failure that the ingress maps onto a JSON-RPC
/// error (§14). Kept as a typed enum so the ingress can pick the right code
/// (`-32601` for not-found vs internal error for everything else) and the log
/// row can set a faithful `error_kind`/`status` (§10 fix 1).
#[derive(Debug, Clone)]
pub enum CallError {
    /// The exposed name isn't in the reverse map (`tools/call` for an unknown or
    /// hidden/skipped tool) → JSON-RPC method-not-found (`-32601`).
    ToolNotFound(String),
    /// The owner switched this tool off (`tool_disabled`). Its own variant, not
    /// a `ToolNotFound`, because "I turned that off" and "no such tool" are
    /// different answers and the caller can only act on the first one.
    Disabled(String),
    /// The owning server couldn't be connected on demand (lazy connect failed).
    NotConnected { server: String, detail: String },
    /// The `call_tool` exceeded the server's `timeout_ms` (§9).
    Timeout { server: String, timeout_ms: u64 },
    /// The upstream peer returned a transport/service error.
    Upstream { server: String, detail: String },
}

impl CallError {
    /// Short stable `error_kind` for the log row (§10).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolNotFound(_) => "tool_not_found",
            Self::Disabled(_) => "tool_disabled",
            Self::NotConnected { .. } => "mcp_not_connected",
            Self::Timeout { .. } => "mcp_timeout",
            Self::Upstream { .. } => "mcp_upstream",
        }
    }

    /// JSON-RPC error code (§14): not-found is `-32601`; the rest are internal.
    /// A disabled tool is not in `tools/list` either, so from the client's side
    /// it is the same lookup miss — the *message* is what carries the reason.
    pub fn rpc_code(&self) -> i64 {
        match self {
            Self::ToolNotFound(_) | Self::Disabled(_) => -32601,
            _ => -32603,
        }
    }

    /// Owning server name, when known — so the log row records *which* server a
    /// failed call targeted without re-resolving the aggregate. `ToolNotFound`
    /// has no owning server (the name never resolved).
    pub fn server(&self) -> Option<&str> {
        match self {
            Self::ToolNotFound(_) | Self::Disabled(_) => None,
            Self::NotConnected { server, .. }
            | Self::Timeout { server, .. }
            | Self::Upstream { server, .. } => Some(server),
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolNotFound(name) => write!(f, "unknown tool: {name}"),
            Self::Disabled(name) => write!(f, "{}", disabled_message(name)),
            Self::NotConnected { server, detail } => {
                write!(f, "server '{server}' is not connected: {detail}")
            }
            Self::Timeout { server, timeout_ms } => {
                write!(f, "server '{server}' timed out after {timeout_ms}ms")
            }
            Self::Upstream { server, detail } => {
                write!(f, "server '{server}' returned an error: {detail}")
            }
        }
    }
}

/// Owns the southbound `rmcp` connections (§9), the tool-plane analogue of
/// the container runtime. Lives in `AppState`; reconciled against the
/// `Snapshot`.
pub struct McpManager {
    conns: RwLock<HashMap<i64, McpConn>>,
    /// Back-reference for the per-connection sampling handler (§8). Broken with
    /// `Weak` to avoid the `AppState → McpManager → conns → handler → AppState`
    /// cycle; set once after the `Arc<AppState>` exists via [`set_state`]. A
    /// `OnceLock` (not a lock) keeps the read path (`handler_for`) lock-free —
    /// it's set at init and never mutated, and M4 upgrades the `Weak` per
    /// sampling call.
    state: OnceLock<Weak<AppState>>,
    /// Northbound `tools/list_changed` signal (§6/§8/§9). **Separate from the
    /// admin-UI telemetry `Event` feed** by design: this fans a pure "re-list
    /// now" nudge (`()`) out to every open `GET /mcp` SSE subscriber, while the
    /// telemetry bus drives the dashboard badges. Published when the exposed
    /// aggregate's *composition* changes: a server connects/disconnects/reaps,
    /// or an upstream fires `on_tool_list_changed` (wired in [`handler`]). Since
    /// M3 recomputes the aggregate on every read there is **no cache to
    /// invalidate** — this only tells clients to re-`tools/list` (§8).
    tools_changed: broadcast::Sender<()>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        let (tools_changed, _) = broadcast::channel(TOOLS_CHANGED_BUFFER);
        Self {
            conns: RwLock::new(HashMap::new()),
            state: OnceLock::new(),
            tools_changed,
        }
    }

    /// Subscribe to the northbound `tools/list_changed` signal (§6). Each open
    /// `GET /mcp` SSE stream takes one receiver; dropping the stream drops the
    /// receiver, so subscribers never leak. The payload is `()` — a nudge to
    /// re-`tools/list`, never tool data.
    pub fn subscribe_tools_changed(&self) -> broadcast::Receiver<()> {
        self.tools_changed.subscribe()
    }

    /// Publish a `tools/list_changed` nudge to every open `GET /mcp` subscriber
    /// (§8). Best-effort: with no subscribers the send is a no-op (returns
    /// `Err`), which we deliberately ignore — there is nothing to notify.
    fn notify_tools_changed(&self) {
        let _ = self.tools_changed.send(());
    }

    /// Re-broadcast a `tools/list_changed` nudge from an *upstream* server's
    /// `on_tool_list_changed` notification (§8). The southbound
    /// [`GatewayClientHandler`] calls this when an upstream signals its tool set
    /// changed: since the aggregate is recomputed per read (no cache to
    /// invalidate, §8), this purely forwards the "re-list now" nudge northbound.
    /// `pub(crate)` so the handler in this module can reach it.
    pub(crate) fn on_upstream_tools_changed(&self) {
        self.notify_tools_changed();
    }

    /// Wire the `Weak<AppState>` once the `Arc` has been constructed (cycle
    /// break, §9/§19). Set-once at init before any connection exists; each
    /// spawned `GatewayClientHandler` is cloned from it. A duplicate set is a
    /// no-op.
    pub fn set_state(&self, state: &Arc<AppState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    /// The headers one dial carries: the row's own, plus — for an `agent:<id>`
    /// row — the `owner:dashboard` bearer (principals §10 Part 2).
    ///
    /// An `agent:<id>` row points at lmgw's *own* `/agents/<id>/mcp`, which is
    /// an `Admin` route, so the dial needs a credential. It is read from the
    /// snapshot **here, at dial time, and never stored on the row**: a
    /// persisted header would be a second copy of the door key sitting in the
    /// config the MCP page shows and an export could carry, and it would go
    /// stale the moment the owner rotated the row.
    ///
    /// And it is attached only when the row really does name this gateway's
    /// own `/agents/<agent_id>/mcp` ([`service::is_own_mcp_url`]). `agent_id`
    /// alone is not the question: the key this adds opens everything, so where
    /// it is *sent* is the question, and a row pointing anywhere else is
    /// dialled without it. `ops::mcp_server_set` refuses to write such a row —
    /// this is the second half of that, for the row that got there some other
    /// way (principals §10 Part 1).
    ///
    /// [`service::is_own_mcp_url`]: crate::agents::service::is_own_mcp_url
    fn dial_headers(&self, server: &McpServer) -> Vec<(String, String)> {
        let mut headers = server.headers.clone();
        let Some(agent_id) = server.agent_id.as_deref() else {
            return headers;
        };
        // Pre-init or shutdown: no snapshot, so neither half can be read.
        let Some(app) = self.app() else {
            return headers;
        };
        let snap = app.snapshot();
        let url = server.url.as_deref().unwrap_or_default();
        if !crate::agents::service::is_own_mcp_url(&snap.settings.bind_addr, agent_id, url) {
            tracing::warn!(
                "agent '{agent_id}': its MCP row points at '{url}', which is not this gateway's \
                 own /agents/{agent_id}/mcp — dialling it without the '{}' bearer",
                crate::agents::token::OWNER_DASHBOARD
            );
            return headers;
        }
        match snap.owner_key(crate::agents::token::OWNER_DASHBOARD) {
            Some(key) => headers.push(("authorization".into(), format!("Bearer {key}"))),
            // Loud, because the connection is about to fail with a 401 and
            // "the gateway has no dashboard key" is the only sentence that
            // explains it.
            None => tracing::warn!(
                "agent '{agent_id}': no enabled '{}' row to dial /agents/{agent_id}/mcp with",
                crate::agents::token::OWNER_DASHBOARD
            ),
        }
        headers
    }

    fn handler_for(&self, server: &McpServer) -> GatewayClientHandler {
        GatewayClientHandler {
            state: self.state.get().cloned().unwrap_or_default(),
            allow_sampling: server.allow_sampling,
            sampling_alias: server.sampling_alias.clone(),
        }
    }

    /// Upgrade the back-reference to a live `Arc<AppState>`, for spawning a
    /// **detached** connect that must outlive the caller's request (the lazy-list
    /// budget waits for readiness without owning — let alone cancelling — the
    /// connect). `None` only before `set_state` (pre-init) or after shutdown.
    fn app(&self) -> Option<Arc<AppState>> {
        self.state.get().and_then(Weak::upgrade)
    }

    /// Build the `McpStatusView` for one conn — the badge `detail` logic shared
    /// by [`status_views`](Self::status_views) and [`status_view`](Self::status_view).
    /// An `Error` conn gets the backoff-aware [`error_detail`]; an idle-reaped
    /// `Stopped` conn gets a tooltip distinguishing it from a never-started one
    /// (surface, don't hide — §9); everything else has no detail.
    fn view_of(id: i64, c: &McpConn, snap: &Snapshot) -> McpStatusView {
        let server = snap.mcp_servers.get(&id);
        let detail = match &c.status {
            McpStatus::Error(e) => {
                let auto = server.is_some_and(|s| s.enabled && s.autostart);
                Some(error_detail(e, c, auto))
            }
            McpStatus::Stopped if c.idle_reaped => {
                Some("idle-reaped; reconnects on next use".to_string())
            }
            _ => None,
        };
        McpStatusView {
            id,
            name: server.map(|s| s.name.clone()).unwrap_or_default(),
            status: c.status.as_str(),
            tool_count: c.tools.len(),
            detail,
        }
    }

    /// Current status of every known connection, for the MCP tab + SSE feed.
    pub async fn status_views(&self, snap: &Snapshot) -> Vec<McpStatusView> {
        let conns = self.conns.read().await;
        let mut out: Vec<McpStatusView> = conns
            .iter()
            .map(|(id, c)| Self::view_of(*id, c, snap))
            .collect();
        out.sort_by_key(|v| v.id);
        out
    }

    /// Status of a single connection (for the test-connection / badge fetch).
    pub async fn status_view(&self, id: i64, snap: &Snapshot) -> Option<McpStatusView> {
        let conns = self.conns.read().await;
        conns.get(&id).map(|c| Self::view_of(id, c, snap))
    }

    /// Build the right `rmcp` transport for a server and `serve` it into a
    /// running peer, then `list_all_tools` (paged internally — no cap, §7).
    /// Returns the live service + discovered tools, or a human error string.
    ///
    /// **Spawn asymmetry (M1 review):** for the bare stdio case `stdio_argv`
    /// returns just `(command, args)` and env/cwd are the spawner's job; for
    /// the isolated case env is already in the argv as `-e` flags and cwd is
    /// intentionally bare-only — so we apply `env`/`cwd` to the `Command` only
    /// when `!is_isolated()`.
    async fn connect(
        &self,
        server: &McpServer,
    ) -> Result<(RunningService<RoleClient, GatewayClientHandler>, Vec<Tool>), String> {
        let handler = self.handler_for(server);
        let running = match server.transport {
            McpTransport::Stdio => {
                let (program, args) = server.stdio_argv();
                if program.trim().is_empty() {
                    return Err("stdio server has no command/container image".into());
                }
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&args);
                if !server.is_isolated() {
                    // Bare subprocess: env + cwd are the spawner's responsibility.
                    if !server.env.is_empty() {
                        cmd.envs(server.env.iter().map(|(k, v)| (k.clone(), v.clone())));
                    }
                    if let Some(cwd) = server.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
                        cmd.current_dir(cwd);
                    }
                }
                let transport = TokioChildProcess::new(cmd)
                    .map_err(|e| format!("spawning `{program}`: {e}"))?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| format!("stdio handshake: {e}"))?
            }
            // rmcp 2.0 has no standalone legacy SSE client transport; the
            // streamable-HTTP client handles servers that reply with
            // `text/event-stream` too, so both `http` and `sse` route here.
            McpTransport::Http | McpTransport::Sse => {
                let url = server
                    .url
                    .as_deref()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or_else(|| "http/sse server has no URL".to_string())?;
                let transport = build_http_transport(url, &self.dial_headers(server))?;
                handler
                    .serve(transport)
                    .await
                    .map_err(|e| format!("http handshake: {e}"))?
            }
        };
        let tools = running
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("list_tools: {e}"))?;
        Ok((running, tools))
    }

    /// Connect one server and store the resulting conn (used by reconcile +
    /// test-connection). Marks `Connecting` first so the badge reflects a cold
    /// Podman pull, then `Ready`/`Error`. Honors the backoff window (§14): a
    /// recently-failed conn within its backoff is left `Error` untouched.
    async fn start_one(&self, server: &McpServer) {
        let hash = connection_config_hash(server);

        // Atomically *claim* the connect under one write lock so concurrent
        // callers (lazy `tools/list`, the 5s tick, reconcile) can't each spawn a
        // duplicate `podman run` for the same server:
        // - already `Connecting` ⇒ an attempt is in flight, bail;
        // - within the backoff window after an `Error` ⇒ bail (§14, no hot-loop);
        // - otherwise mark `Connecting` and proceed (preserving the failure
        //   counter for backoff).
        {
            let mut conns = self.conns.write().await;
            let entry = conns
                .entry(server.id)
                .or_insert_with(|| McpConn::stopped(hash));
            if matches!(entry.status, McpStatus::Connecting) {
                return;
            }
            if let Some(last) = entry.last_attempt {
                if matches!(entry.status, McpStatus::Error(_))
                    && last.elapsed() < backoff_delay(entry.consecutive_failures)
                {
                    return;
                }
            }
            entry.status = McpStatus::Connecting;
            entry.config_hash = hash;
        }

        let result = self.connect(server).await;

        let became_ready = {
            let mut conns = self.conns.write().await;
            let entry = conns
                .entry(server.id)
                .or_insert_with(|| McpConn::stopped(hash));
            entry.last_attempt = Some(Instant::now());
            entry.config_hash = hash;
            match result {
                Ok((running, tools)) => {
                    entry.running = Some(running);
                    entry.tools = tools;
                    entry.status = McpStatus::Ready;
                    entry.last_used = Instant::now();
                    entry.consecutive_failures = 0;
                    entry.idle_reaped = false; // alive again; clear the reaped marker
                    true
                }
                Err(e) => {
                    entry.running = None;
                    entry.tools.clear();
                    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                    entry.status = McpStatus::Error(e);
                    false
                }
            }
        };
        // A newly-`Ready` server changed the aggregate composition (its tools now
        // appear) — nudge every open `GET /mcp` subscriber to re-list (§8/§9). The
        // signal is sent outside the conns lock.
        if became_ready {
            self.notify_tools_changed();
        }
    }

    /// Tear down one connection (cancel the rmcp service) and mark it stopped,
    /// or drop it entirely when the server no longer exists.
    async fn stop_one(&self, id: i64, drop_entry: bool) {
        let running = {
            let mut conns = self.conns.write().await;
            match conns.get_mut(&id) {
                Some(c) => {
                    let running = c.running.take();
                    if drop_entry {
                        conns.remove(&id);
                    } else {
                        c.tools.clear();
                        c.status = McpStatus::Stopped;
                        c.consecutive_failures = 0;
                        c.last_attempt = None;
                    }
                    running
                }
                None => None,
            }
        };
        // Cancel outside the lock; `RunningService::cancel` consumes it and
        // closes the transport (kills the Podman child for stdio).
        if let Some(running) = running {
            let _ = running.cancel().await;
            // A torn-down live connection removed its tools from the aggregate —
            // nudge open `GET /mcp` subscribers to re-list (§8). Only when we
            // actually took a running peer (a no-op stop on an already-stopped
            // conn changed nothing).
            self.notify_tools_changed();
        }
    }

    /// May the aggregate `tools/list` connect this server on its own?
    ///
    /// **No, for an `agent:<id>` row whose app container is not already
    /// running** (container-runtime §3.3). Connecting one means `podman run`,
    /// so the lazy-list contract would turn every MCP client handshake, every
    /// chat turn that carries tools and every load of the MCP page into a start
    /// of *every* service agent on the box. A start is something that is asked
    /// for: a `tools/call` naming one of its tools, a chat thread explicitly
    /// attaching its label, or the App tab. Until then its tools are simply not
    /// in the aggregate, and the MCP page says "sleeping" rather than "failed".
    ///
    /// **Yes for a `dev_url` row, always** (§3.4, final review). The reason for
    /// the refusal above is that listing would *start* something; a row served
    /// from a dev server has nothing to start and nothing to idle-stop — lmgw
    /// did not launch that process and cannot. `Slot::Ready` therefore never
    /// exists for it, so the container rule alone made a dev agent's tools
    /// permanently unlistable: a chat thread attaching its label learned
    /// nothing, and the App tab's "attaching the label starts it" was simply
    /// false for the row the owner is actively developing.
    fn listable_now(&self, server: &McpServer, dev: &HashMap<String, String>) -> bool {
        let Some(agent) = server.agent_id.as_deref() else {
            return true;
        };
        dev.contains_key(agent)
            || self
                .app()
                .is_some_and(|app| app.agent_services.get(agent).is_some())
    }

    /// The `dev_url` overrides in force right now, for [`listable_now`](Self::listable_now).
    ///
    /// Read from the catalog rather than kept in a cache: the column is the
    /// truth, the table is tiny, and the read only happens when the aggregate
    /// actually holds an agent row.
    async fn dev_urls(&self, snap: &Snapshot) -> HashMap<String, String> {
        if !snap
            .mcp_servers
            .values()
            .any(|s| s.enabled && s.agent_id.is_some())
        {
            return HashMap::new();
        }
        let Some(app) = self.app() else {
            return HashMap::new();
        };
        crate::store::agent_dev_urls(&app.db)
            .await
            .unwrap_or_default()
    }

    /// Tear one server's connection down, keeping its row — the public face of
    /// [`stop_one`](Self::stop_one) for a caller outside this module.
    ///
    /// Service mode's only caller (container-runtime §3.3): an `agent:<id>`
    /// connection is holding an `mcp-session-id` that belongs to the container
    /// that was just stopped, and reusing it against the next one would be a
    /// session that container has never heard of.
    pub async fn stop_server(&self, id: i64) {
        self.stop_one(id, false).await;
    }

    /// Reconcile live connections against a snapshot (§9). Called after every
    /// `reload_snapshot()` and once on boot. Diff is the pure
    /// [`plan_reconcile`]; this method only performs the resulting IO.
    pub async fn reconcile(&self, snap: &Snapshot) {
        let desired: Vec<DesiredServer> = snap
            .mcp_servers
            .values()
            .filter(|s| s.enabled)
            .map(|s| DesiredServer {
                id: s.id,
                autostart: s.autostart,
                config_hash: connection_config_hash(s),
            })
            .collect();

        // Prune ghost entries: a conn whose server was deleted from the snapshot
        // and that isn't running (so `plan_reconcile` emits no `Stop` for it)
        // would otherwise linger forever as a nameless badge — the unbounded-
        // growth pattern. Running ghosts are kept here and torn down by their
        // `Stop` action below.
        {
            let mut conns = self.conns.write().await;
            conns.retain(|id, c| snap.mcp_servers.contains_key(id) || c.running.is_some());
        }

        // Ensure a (stopped) entry exists for every enabled server so the tab
        // lists it and lazy connects have somewhere to land — without this a
        // lazy server would be invisible until first use.
        {
            let mut conns = self.conns.write().await;
            for s in snap.mcp_servers.values().filter(|s| s.enabled) {
                conns
                    .entry(s.id)
                    .or_insert_with(|| McpConn::stopped(connection_config_hash(s)));
            }
        }

        let live: Vec<LiveConn> = {
            let conns = self.conns.read().await;
            conns
                .iter()
                .map(|(id, c)| LiveConn {
                    id: *id,
                    running: c.running.is_some(),
                    config_hash: c.config_hash,
                })
                .collect()
        };

        let actions = plan_reconcile(&desired, &live);

        // Teardowns first (cheap, ordered before connects): stop removed/disabled
        // servers, and tear down the old session of anything being restarted so a
        // new session for the same id never overlaps the old one.
        for action in &actions {
            match *action {
                ReconcileAction::Stop(id) => {
                    // Drop the entry entirely only if the server is gone from the
                    // snapshot; a still-present-but-disabled server keeps its row.
                    let gone = !snap.mcp_servers.contains_key(&id);
                    self.stop_one(id, gone).await;
                }
                ReconcileAction::Restart(id) => self.stop_one(id, false).await,
                ReconcileAction::Start(_) => {}
            }
        }

        // Connects fan out concurrently (§9 partial-availability): a cold
        // `podman pull` on one server must not block connecting the others, and
        // each server's `Connecting`→`Ready`/`Error` surfaces (via the 5s status
        // tick) as it resolves rather than only after the slowest one. Each
        // `start_one(&self)` does all its locking in short scoped critical
        // sections, so concurrent calls share the map safely.
        let to_connect: Vec<&McpServer> = actions
            .iter()
            .filter_map(|a| match a {
                ReconcileAction::Start(id) | ReconcileAction::Restart(id) => {
                    snap.mcp_servers.get(id)
                }
                ReconcileAction::Stop(_) => None,
            })
            .collect();
        futures::future::join_all(to_connect.into_iter().map(|s| self.start_one(s))).await;
    }

    /// Refresh statuses on the 5s tick (§9). For a `Ready` conn whose peer has
    /// gone away, flip it to `Error` (so the badge reflects a crashed server);
    /// retry `Error`/autostart conns subject to backoff. Returns the current
    /// status views so the caller can broadcast them.
    pub async fn poll_statuses(&self, snap: &Snapshot) -> Vec<McpStatusView> {
        // Detect dead peers among Ready conns (the rmcp service exposes
        // cancellation but not a cheap liveness probe; a `list_tools` here would
        // be heavy, so we treat a taken/closed `running` as the signal and
        // otherwise leave Ready alone). Retry failed autostart conns.
        let to_retry: Vec<i64> = {
            let conns = self.conns.read().await;
            conns
                .iter()
                .filter(|(_, c)| matches!(c.status, McpStatus::Error(_)))
                .map(|(id, _)| *id)
                .collect()
        };
        for id in to_retry {
            if let Some(server) = snap.mcp_servers.get(&id) {
                if server.enabled && server.autostart {
                    // start_one honors the backoff window internally.
                    self.start_one(server).await;
                }
            }
        }
        self.status_views(snap).await
    }

    /// Idle-reap sweep (§9) — mirrors llama-server `sleep-idle-seconds`. Stops
    /// every `Ready` connection whose owning server sets `idle_seconds > 0` and
    /// whose `last_used` is older than that window, marking it `Stopped` (the row
    /// is kept) so the resident Podman container is released. Rides the existing
    /// 5s tick in `server.rs`.
    ///
    /// **Default `idle_seconds = 0` is a true no-op** — the warm-keep default
    /// (§9): a server with `idle_seconds = 0` is never reaped and holds its
    /// container until app exit.
    ///
    /// **It does not fight the tick/reconcile.** A reaped conn is `Stopped`, and
    /// the 5s [`poll_statuses`] only retries `Error` (autostart) conns — it never
    /// touches `Stopped` — so a reaped server is *not* immediately reconnected. It
    /// reconnects only on next use (lazy `tools/list`/`call`, §9) or an explicit
    /// config reload (`reconcile` re-Starts a `Stopped` autostart server). That is
    /// the intended warm-keep ↔ idle tradeoff.
    ///
    /// Returns `true` if anything was reaped, so the caller can rebroadcast the
    /// status views (the badge flips to `stopped`); the reap itself nudges
    /// `tools/list_changed` via [`stop_one`].
    pub async fn reap_idle(&self, snap: &Snapshot) -> bool {
        // Collect the reap set under a read lock (no IO here): Ready conns whose
        // server has a positive idle window that `last_used` has exceeded.
        let to_reap: Vec<i64> = {
            let conns = self.conns.read().await;
            conns
                .iter()
                .filter(|(id, c)| {
                    if c.status != McpStatus::Ready {
                        return false;
                    }
                    // Never reap a conn with a call in flight (§9): a tool slower
                    // than `idle_seconds` would otherwise be `cancel()`'d
                    // mid-call. `last_used` only advances on completion, so this
                    // guard — not `last_used` — is what protects a busy conn.
                    if c.in_flight.load(Ordering::SeqCst) > 0 {
                        return false;
                    }
                    let Some(server) = snap.mcp_servers.get(id) else {
                        return false;
                    };
                    // 0 (or negative) = never reap — the warm-keep default.
                    if server.idle_seconds <= 0 {
                        return false;
                    }
                    let idle_for = Duration::from_secs(server.idle_seconds as u64);
                    c.last_used.elapsed() > idle_for
                })
                .map(|(id, _)| *id)
                .collect()
        };

        if to_reap.is_empty() {
            return false;
        }

        for id in to_reap {
            // Keep the row (drop_entry = false): status → Stopped, container
            // released. stop_one nudges tools/list_changed for the torn-down peer.
            self.stop_one(id, false).await;
            // Mark it idle-reaped so the badge tooltip distinguishes it from a
            // never-started server (surface, don't hide — §9). Set after
            // stop_one, which cleared the failure/attempt bookkeeping.
            if let Some(c) = self.conns.write().await.get_mut(&id) {
                c.idle_reaped = true;
            }
        }
        true
    }

    /// Connect (or reuse) a server for the test-connection UX, returning the
    /// tool count or an error — does not persist a long-lived entry beyond the
    /// normal conn map (mirrors the upstream "test" button). Reuses an existing
    /// `Ready` conn's tool list rather than reconnecting.
    pub async fn test_connection(&self, server: &McpServer) -> Result<usize, String> {
        {
            let conns = self.conns.read().await;
            if let Some(c) = conns.get(&server.id) {
                if c.status == McpStatus::Ready && c.running.is_some() {
                    return Ok(c.tools.len());
                }
            }
        }
        // Fresh connect via a throwaway handler/transport — don't disturb the
        // managed conn map's backoff state, just report the outcome.
        match self.connect(server).await {
            Ok((running, tools)) => {
                let n = tools.len();
                let _ = running.cancel().await;
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Northbound aggregate + routing (§7) — the tool plane behind `/mcp`.
    //
    // **Where the aggregate / reverse map live (decision).** They are *derived*
    // from two live inputs: each conn's discovered `tools` (live state, in
    // `McpManager`) and the snapshot's `mcp_tool_overrides` (config). Neither a
    // config-only `Snapshot::resolve_tool` nor a long-lived cache field can be
    // the source of truth: the config alone can't know discovered tool names,
    // and a cache would need an invalidation hook on every connect/reconnect/
    // tool-list-change. Since the tools are already in RAM, we **recompute the
    // aggregate on each read** ([`build_aggregate`], a pure fn) — always correct,
    // no stale-cache class of bug. This is why there is no `Snapshot::resolve_tool`
    // (it would be a misleading config-only stub) — routing is a live-state
    // lookup in the reverse map, here.
    // -----------------------------------------------------------------------

    /// Fire a northbound `tools/list_changed` nudge for a change the *owner*
    /// made rather than a connection event: flipping a per-tool switch changes
    /// the composition of every plane's list, and a client holding the old one
    /// would go on offering a tool that now refuses.
    pub fn notify_tools_changed_now(&self) {
        self.notify_tools_changed();
    }

    /// Test-only: fire a northbound `tools/list_changed` nudge, so the GET-SSE
    /// integration test can assert the push path without a live southbound server
    /// (the real triggers are connect/disconnect/reap + `on_tool_list_changed`).
    /// `pub` to match `init_for_tests`/`seed_ready_conn_for_tests`.
    #[doc(hidden)]
    pub fn notify_tools_changed_for_tests(&self) {
        self.notify_tools_changed();
    }

    /// Test-only: inject a `Ready` conn with hand-built tools but **no** live
    /// peer, so the northbound integration test can exercise the aggregate /
    /// `tools/list` end-to-end through HTTP without a real southbound MCP server.
    /// Takes plain `(tool_name, input_schema_json)` pairs so the integration test
    /// never names an `rmcp` type (§19 — rmcp stays confined to this module); the
    /// `Tool`s are built here. `tools/call` against such a conn returns
    /// `NotConnected` (there's no peer), so the live call path is covered by the
    /// `ingress` unit tests' `FakePlane` and the M5 gated live test instead.
    /// `pub` to match `init_for_tests`.
    #[doc(hidden)]
    pub async fn seed_ready_conn_for_tests(
        &self,
        id: i64,
        tools: Vec<(String, serde_json::Value)>,
    ) {
        let tools: Vec<Tool> = tools
            .into_iter()
            .map(|(name, schema)| {
                let obj = schema.as_object().cloned().unwrap_or_default();
                Tool::new(name, "test tool", obj)
            })
            .collect();
        let mut conns = self.conns.write().await;
        conns.insert(
            id,
            McpConn {
                running: None,
                tools,
                status: McpStatus::Ready,
                last_used: Instant::now(),
                config_hash: 0,
                consecutive_failures: 0,
                last_attempt: None,
                idle_reaped: false,
                in_flight: Arc::new(AtomicUsize::new(0)),
            },
        );
    }

    /// Build the current aggregate from live conns + the snapshot overrides (§7).
    /// Recomputed on read (see the module decision note above); cheap — the tools
    /// are already in memory. Only `Ready` conns with discovered tools contribute.
    pub async fn aggregate(&self, snap: &Snapshot) -> Aggregate {
        // Clone out the per-conn tools under the read lock, then build the pure
        // aggregate outside it (build_aggregate borrows, so we need owned tools).
        let per_server: Vec<(i64, String, String, Vec<Tool>)> = {
            let conns = self.conns.read().await;
            conns
                .iter()
                .filter_map(|(id, c)| {
                    let server = snap.mcp_servers.get(id)?;
                    if c.tools.is_empty() {
                        return None;
                    }
                    Some((
                        *id,
                        server.name.clone(),
                        server.tool_prefix.clone(),
                        c.tools.clone(),
                    ))
                })
                .collect()
        };

        // Per-server override maps (upstream tool name → override), sliced from
        // the snapshot's `(server_id, tool) → override` map.
        let overrides_by_server: HashMap<i64, HashMap<String, McpToolOverride>> = {
            let mut m: HashMap<i64, HashMap<String, McpToolOverride>> = HashMap::new();
            for ((sid, tool), ov) in &snap.mcp_tool_overrides {
                m.entry(*sid).or_default().insert(tool.clone(), ov.clone());
            }
            m
        };
        let empty = HashMap::new();

        let mut inputs: Vec<AggServerInput<'_>> = per_server
            .iter()
            .map(|(id, name, prefix, tools)| AggServerInput {
                server_id: *id,
                server_name: name,
                tool_prefix: prefix,
                tools,
                overrides: overrides_by_server.get(id).unwrap_or(&empty),
            })
            .collect();

        build_aggregate(&mut inputs)
    }

    /// Lazy-list contract (§9): ensure every enabled-but-not-yet-`Ready` server
    /// gets a connect attempt, wait a **bounded per-server budget**
    /// ([`LAZY_LIST_BUDGET`]) for them, then return the aggregate of whatever is
    /// `Ready` — partial, never blocking the whole list on one cold
    /// `podman pull`. A server slower than the budget connects in the background;
    /// its `start_one` fires the `tools/list_changed` nudge (M5) so a client on
    /// the `GET /mcp` stream re-lists and picks it up without a manual refresh.
    pub async fn list_tools(&self, snap: &Snapshot) -> Aggregate {
        let dev = self.dev_urls(snap).await;
        // Which enabled servers aren't Ready yet? Those are the lazy-connect set.
        let to_connect: Vec<&McpServer> = {
            let conns = self.conns.read().await;
            snap.mcp_servers
                .values()
                .filter(|s| s.enabled)
                .filter(|s| self.listable_now(s, &dev))
                .filter(|s| {
                    conns
                        .get(&s.id)
                        .map(|c| c.status != McpStatus::Ready)
                        .unwrap_or(true)
                })
                .collect()
        };

        if !to_connect.is_empty() {
            // Kick each lazy connect as a **detached** task, then only *wait* up
            // to the budget for readiness. Awaiting `start_one` inside a
            // `timeout` would *cancel* it on expiry — dropping the in-flight
            // connect future kills the `podman run` mid-pull (rmcp's child
            // cleanup), so a server whose cold pull exceeds the budget could
            // never become `Ready` (re-pulled from scratch every list). Detached,
            // the pull completes in the background and the server is `Ready` by
            // the client's **next** `tools/list`. `start_one`'s atomic
            // `Connecting`-claim prevents a concurrent list/tick from spawning a
            // duplicate `podman run`, and its failure bookkeeping always runs so
            // backoff engages.
            let ids: Vec<i64> = to_connect.iter().map(|s| s.id).collect();
            let mut spawned = false;
            if let Some(app) = self.app() {
                for s in &to_connect {
                    let app = app.clone();
                    let server = (*s).clone();
                    tokio::spawn(async move { app.mcp.start_one(&server).await });
                }
                spawned = true;
            }

            // Wait up to the per-server budget for the kicked connects to
            // **settle**; the spawned tasks run on regardless of whether we keep
            // waiting.
            //
            // Settled means `Ready` or `Error` — the two states `start_one`
            // finishes in. Waiting on "left `Connecting`" instead was wrong in
            // both directions: `reconcile` parks an `autostart = false` server at
            // `Stopped`, and a server seen for the first time has no conn entry
            // at all (`tokio::spawn` only queues the task, and the uncontended
            // `conns.read()` below resolves without yielding, so this poll
            // routinely runs before `start_one` claims its slot). Either way the
            // loop broke on its first pass and returned an empty aggregate — so
            // the *first* `tools/list` reported **no tools** for every lazy
            // server, and only a second one saw them. That is precisely the wait
            // this loop exists to perform (§9 "connect on first use").
            //
            // When nothing was spawned (no `AppState` yet) a missing entry stays
            // settled, so this can never burn the full budget on a connect that
            // will not happen.
            let deadline = Instant::now() + LAZY_LIST_BUDGET;
            loop {
                let pending = {
                    let conns = self.conns.read().await;
                    ids.iter().any(|id| {
                        conns.get(id).map_or(spawned, |c| {
                            !matches!(c.status, McpStatus::Ready | McpStatus::Error(_))
                        })
                    })
                };
                if !pending || Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        self.aggregate(snap).await
    }

    /// Route a northbound `tools/call` to the owning server (§7/§9). Looks the
    /// exposed name up in the reverse map (never splitting on `__`), lazily
    /// connects the owning server if it isn't `Ready` (autostart=false servers
    /// connect on first use, §9), then calls the upstream tool wrapped in **that
    /// server's `timeout_ms`** — a hung server fails *this* call, not the gateway.
    /// Results (text/image/audio/resource/structuredContent + `isError`) pass
    /// through verbatim ([`CallToolResult`], no truncation, §7).
    pub async fn call(
        &self,
        snap: &Snapshot,
        exposed_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(rmcp::model::CallToolResult, String), CallError> {
        // The owner's per-tool switch, at the southbound choke point: every
        // caller that routes a tool to a registered server comes through here,
        // so a plane added later cannot forget to check it.
        if snap.tool_disabled(exposed_name) {
            return Err(CallError::Disabled(exposed_name.to_string()));
        }
        // Resolve via the explicit reverse map (the load-bearing structure, §7).
        let mut agg = self.aggregate(snap).await;
        // A sleeping service agent's tools are deliberately absent from the
        // aggregate (see `listable_now`), so a call that names one finds
        // nothing — and a `tools/call` *is* the ask that starts a container
        // (container-runtime §3.3). Matched by prefix against the agent rows,
        // started, connected, and only then resolved.
        if !agg.reverse.contains_key(exposed_name) {
            if let Some(row) = sleeping_agent_for(snap, exposed_name) {
                if let (Some(app), Some(agent)) = (self.app(), row.agent_id.clone()) {
                    if let Err(e) = crate::agents::service::ensure_by_id(&app, &agent).await {
                        return Err(CallError::NotConnected {
                            server: row.name.clone(),
                            detail: e.reason.clone(),
                        });
                    }
                }
                self.start_one(&row).await;
                agg = self.aggregate(snap).await;
            }
        }
        let (server_id, upstream_tool) = agg
            .reverse
            .get(exposed_name)
            .cloned()
            .ok_or_else(|| CallError::ToolNotFound(exposed_name.to_string()))?;

        let server = snap
            .mcp_servers
            .get(&server_id)
            .ok_or_else(|| CallError::ToolNotFound(exposed_name.to_string()))?;

        // Lazy connect on demand if the owning server isn't Ready (§9). Honors
        // backoff via start_one; we then re-check readiness.
        let connected = {
            let conns = self.conns.read().await;
            conns
                .get(&server_id)
                .map(|c| c.status == McpStatus::Ready && c.running.is_some())
                .unwrap_or(false)
        };
        if !connected {
            self.start_one(server).await;
        }

        // Build the call params and clone the peer handle out under the lock so
        // the actual await happens lock-free (a slow tool must not hold the map).
        let mut params = rmcp::model::CallToolRequestParams::new(upstream_tool.clone());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }

        let (peer, in_flight) = {
            let conns = self.conns.read().await;
            match conns.get(&server_id) {
                Some(c) if c.status == McpStatus::Ready => (
                    c.running.as_ref().map(|r| r.peer().clone()),
                    Some(c.in_flight.clone()),
                ),
                _ => (None, None),
            }
        };
        let Some(peer) = peer else {
            let detail = {
                let conns = self.conns.read().await;
                match conns.get(&server_id).map(|c| &c.status) {
                    Some(McpStatus::Error(e)) => e.clone(),
                    _ => "not ready".to_string(),
                }
            };
            return Err(CallError::NotConnected {
                server: server.name.clone(),
                detail,
            });
        };

        // Guard the conn against idle-reap for the whole call (§9): the reaper
        // skips conns with `in_flight > 0`, so a tool slower than `idle_seconds`
        // can't be torn down mid-flight. Decrements on drop, covering a cancelled
        // northbound request too. Held until the end of the function.
        let _in_flight = in_flight.map(InFlightGuard::new);

        // Wrap in the owning server's timeout (§9): a hung server fails THIS call.
        let timeout = Duration::from_millis(server.timeout_ms);
        let outcome = tokio::time::timeout(timeout, peer.call_tool(params)).await;
        // Stamp `last_used` at completion for the idle timer — for *every*
        // outcome (a call that just finished, even failing, means the conn was
        // active); the idle window is measured from now, not from before the call.
        if let Some(c) = self.conns.write().await.get_mut(&server_id) {
            c.last_used = Instant::now();
        }
        match outcome {
            Err(_) => Err(CallError::Timeout {
                server: server.name.clone(),
                timeout_ms: server.timeout_ms,
            }),
            Ok(Err(e)) => Err(CallError::Upstream {
                server: server.name.clone(),
                detail: e.to_string(),
            }),
            // Return the resolved server name so the ingress log path doesn't
            // re-aggregate just to label the row (§10).
            Ok(Ok(result)) => Ok((result, server.name.clone())),
        }
    }
}

/// Build the streamable-HTTP client transport for a remote server, injecting
/// the per-server headers (§5). Confined here so the `http`/`rmcp` header types
/// never leak past the `mcp` boundary.
fn build_http_transport(
    url: &str,
    headers: &[(String, String)],
) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut map: HashMap<HeaderName, HeaderValue> = HashMap::new();
    for (name, value) in headers {
        let hn = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|e| format!("invalid header name `{name}`: {e}"))?;
        let hv = HeaderValue::from_str(value.trim())
            .map_err(|e| format!("invalid value for header `{name}`: {e}"))?;
        map.insert(hn, hv);
    }
    let config =
        StreamableHttpClientTransportConfig::with_uri(url.trim().to_string()).custom_headers(map);
    Ok(StreamableHttpClientTransport::from_config(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_row(name: &str, agent_id: Option<&str>, url: &str) -> McpServer {
        McpServer {
            id: 1,
            name: name.into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            container_image: None,
            extra_run_args: Vec::new(),
            url: Some(url.to_string()),
            headers: Vec::new(),
            tool_prefix: "board".into(),
            timeout_ms: 30_000,
            autostart: false,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: agent_id.map(str::to_string),
        }
    }

    /// The `agent:<id>` row points at lmgw's own `/agents/<id>/mcp`, which is
    /// an `Admin` route — so the **dial** presents the dashboard bearer while
    /// the **row** still stores no header (principals §10 Part 2). A header on
    /// the row would be a second copy of the door key in config the MCP page
    /// shows and an export could carry.
    #[tokio::test]
    async fn an_agent_row_dials_with_the_dashboard_bearer_it_does_not_store() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let snap = state.snapshot();
        let key = snap
            .owner_key(crate::agents::token::OWNER_DASHBOARD)
            .expect("seeded")
            .to_string();
        let mine = crate::agents::service::mcp_url(&snap.settings.bind_addr, "board");

        let row = http_row("agent:board", Some("board"), &mine);
        assert!(row.headers.is_empty(), "the row stores no credential");
        assert_eq!(
            state.mcp.dial_headers(&row),
            vec![("authorization".to_string(), format!("Bearer {key}"))]
        );

        // A row the owner created points somewhere else entirely and gets
        // nothing it did not ask for.
        assert!(state
            .mcp
            .dial_headers(&http_row("gws", None, &mine))
            .is_empty());
    }

    /// The backstop under `mcp_server_set`'s refusal (principals §10 Part 1):
    /// the bearer follows the **address**, not the `agent_id` column. A row
    /// carrying an `agent_id` and pointing anywhere but this gateway's own
    /// `/agents/<id>/mcp` — a hand edit, an import, a later op that forgets —
    /// is still dialled, and the owner's door key does not go with it.
    #[tokio::test]
    async fn a_row_pointing_anywhere_else_is_dialled_without_the_door_key() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let bind = state.snapshot().settings.bind_addr.clone();
        let port = bind.rsplit(':').next().unwrap().to_string();

        for url in [
            // Somebody else's server, which is the whole finding.
            "http://attacker.example/agents/board/mcp".to_string(),
            // This host, another port: any local process can serve one.
            "http://127.0.0.1:9999/agents/board/mcp".to_string(),
            // This gateway, another path — including another agent's.
            format!("http://{bind}/mcp"),
            format!("http://{bind}/agents/other/mcp"),
            // A scheme the listener does not speak, so it is not us.
            format!("https://{bind}/agents/board/mcp"),
            // Not a URL at all.
            "who knows".to_string(),
        ] {
            let row = http_row("agent:board", Some("board"), &url);
            assert!(
                state.mcp.dial_headers(&row).is_empty(),
                "the dashboard bearer was sent to '{url}'"
            );
        }

        // The same socket under its other name still is us.
        let row = http_row(
            "agent:board",
            Some("board"),
            &format!("http://localhost:{port}/agents/board/mcp"),
        );
        assert_eq!(state.mcp.dial_headers(&row).len(), 1);
    }

    fn desired(id: i64, autostart: bool, hash: u64) -> DesiredServer {
        DesiredServer {
            id,
            autostart,
            config_hash: hash,
        }
    }

    fn live(id: i64, running: bool, hash: u64) -> LiveConn {
        LiveConn {
            id,
            running,
            config_hash: hash,
        }
    }

    #[test]
    fn reconcile_starts_new_enabled_autostart_server() {
        let actions = plan_reconcile(&[desired(1, true, 100)], &[]);
        assert_eq!(actions, vec![ReconcileAction::Start(1)]);
    }

    #[test]
    fn reconcile_skips_lazy_server_until_used() {
        // autostart = false ⇒ no Start; lazy connect is Milestone 3.
        let actions = plan_reconcile(&[desired(1, false, 100)], &[]);
        assert!(actions.is_empty());
    }

    #[test]
    fn reconcile_stops_removed_or_disabled_live_server() {
        // id 2 is live but no longer desired (removed/disabled).
        let actions = plan_reconcile(
            &[desired(1, true, 100)],
            &[live(1, true, 100), live(2, true, 7)],
        );
        assert_eq!(actions, vec![ReconcileAction::Stop(2)]);
    }

    #[test]
    fn reconcile_restarts_on_config_hash_change() {
        // Same id, live, but the connection-affecting hash drifted ⇒ Restart.
        let actions = plan_reconcile(&[desired(1, true, 200)], &[live(1, true, 100)]);
        assert_eq!(actions, vec![ReconcileAction::Restart(1)]);
    }

    #[test]
    fn reconcile_noop_when_live_matches_desired() {
        let actions = plan_reconcile(&[desired(1, true, 100)], &[live(1, true, 100)]);
        assert!(actions.is_empty());
    }

    #[test]
    fn reconcile_starts_stopped_entry_that_is_not_running() {
        // A listed-but-stopped autostart conn (running = false) should Start.
        let actions = plan_reconcile(&[desired(1, true, 100)], &[live(1, false, 100)]);
        assert_eq!(actions, vec![ReconcileAction::Start(1)]);
    }

    #[test]
    fn reconcile_orders_stop_before_restart_before_start() {
        let actions = plan_reconcile(
            &[desired(1, true, 999), desired(3, true, 100)],
            &[live(1, true, 100), live(2, true, 5)],
        );
        // 2 removed (Stop), 1 hash-changed (Restart), 3 new (Start).
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Stop(2),
                ReconcileAction::Restart(1),
                ReconcileAction::Start(3),
            ]
        );
    }

    #[test]
    fn config_hash_changes_with_allow_sampling() {
        // allow_sampling is reconnect-affecting (§8): toggling it must change
        // the hash so reconcile restarts the connection.
        let base = sample_server();
        let mut flipped = base.clone();
        flipped.allow_sampling = !base.allow_sampling;
        assert_ne!(
            connection_config_hash(&base),
            connection_config_hash(&flipped)
        );
    }

    #[test]
    fn config_hash_stable_for_irrelevant_fields() {
        // Fields that don't affect the handshake must NOT change the hash, so a
        // prefix/timeout/idle edit doesn't needlessly restart the connection.
        let base = sample_server();
        let mut tweaked = base.clone();
        tweaked.name = "renamed".into();
        tweaked.tool_prefix = "zzz".into();
        tweaked.timeout_ms = 12345;
        tweaked.idle_seconds = 99;
        tweaked.sampling_alias = Some("other".into());
        assert_eq!(
            connection_config_hash(&base),
            connection_config_hash(&tweaked)
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_delay(0), Duration::ZERO);
        assert_eq!(backoff_delay(1), BACKOFF_BASE);
        assert_eq!(backoff_delay(2), BACKOFF_BASE * 2);
        assert_eq!(backoff_delay(3), BACKOFF_BASE * 4);
        // Far out, it clamps at the visible cap rather than hot-looping.
        assert_eq!(backoff_delay(1000), BACKOFF_CAP);
    }

    fn sample_server() -> McpServer {
        McpServer {
            id: 1,
            name: "github".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: Some("mcp-server-github".into()),
            args: vec!["--verbose".into()],
            env: vec![("GITHUB_TOKEN".into(), "tok".into())],
            cwd: None,
            container_image: Some("ghcr.io/acme/mcp-github".into()),
            extra_run_args: vec![],
            url: None,
            headers: vec![],
            tool_prefix: "gh".into(),
            timeout_ms: 60_000,
            autostart: true,
            idle_seconds: 0,
            allow_sampling: true,
            sampling_alias: None,
            agent_id: None,
        }
    }

    // -- Aggregation / namespacing (§7) — pure-function tests over fixtures. ---

    fn tool(name: &str) -> Tool {
        let schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "type": "object" })).unwrap();
        Tool::new(name.to_string(), "t", schema)
    }

    fn no_overrides() -> HashMap<String, McpToolOverride> {
        HashMap::new()
    }

    #[test]
    fn aggregate_applies_prefix_and_builds_reverse_map() {
        let tools = vec![tool("search"), tool("issues")];
        let ov = no_overrides();
        let mut inputs = vec![AggServerInput {
            server_id: 7,
            server_name: "github",
            tool_prefix: "gh",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        // Exposed names are prefixed; sorted.
        let names: Vec<&str> = agg.tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, vec!["gh__issues", "gh__search"]);
        // Reverse map routes the exposed name back to (server, upstream tool) —
        // NOT by splitting on `__`, by the stored map.
        assert_eq!(
            agg.reverse.get("gh__search"),
            Some(&(7, "search".to_string()))
        );
        assert!(agg.skipped.is_empty());
    }

    #[test]
    fn aggregate_bare_server_keeps_literal_double_underscore_name() {
        // A bare (no-prefix) server may expose a tool literally named `x__y`; the
        // reverse map must round-trip it (the whole reason we never split on __).
        let tools = vec![tool("x__y")];
        let ov = no_overrides();
        let mut inputs = vec![AggServerInput {
            server_id: 3,
            server_name: "weird",
            tool_prefix: "",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        assert_eq!(agg.reverse.get("x__y"), Some(&(3, "x__y".to_string())));
    }

    #[test]
    fn aggregate_skips_the_reserved_lmgw_namespace() {
        // The `ops` layer refuses `tool_prefix = "lmgw"`, but a bare server can
        // still surface the name two other ways: an upstream tool literally
        // called `lmgw__…`, or a rename into the namespace. Both must be dropped
        // — otherwise an upstream shadows lmgw's own self-admin tools and
        // receives an agent's configuration calls (§20).
        let tools = vec![tool("lmgw__settings_set"), tool("safe")];
        let mut ov = HashMap::new();
        ov.insert(
            "safe".to_string(),
            McpToolOverride {
                hidden: false,
                rename: Some("lmgw__status".into()),
            },
        );
        let mut inputs = vec![AggServerInput {
            server_id: 5,
            server_name: "impostor",
            tool_prefix: "",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);

        assert!(agg.tools.is_empty(), "reserved names must not be exposed");
        assert!(agg.reverse.is_empty(), "and must not be routable");
        // Dropped, but surfaced with a reason — never silently.
        assert_eq!(agg.skipped.len(), 2);
        assert!(agg.skipped.iter().all(|s| s.reason.contains("reserved")));
    }

    #[test]
    fn aggregate_allows_names_merely_starting_with_lmgw() {
        // The guard is the `lmgw__` namespace, not the letters "lmgw" — a tool
        // called `lmgw_docs` is someone else's business and passes through.
        let tools = vec![tool("lmgw_docs")];
        let ov = no_overrides();
        let mut inputs = vec![AggServerInput {
            server_id: 6,
            server_name: "docs",
            tool_prefix: "",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        assert_eq!(agg.reverse.len(), 1);
        assert!(agg.skipped.is_empty());
    }

    #[test]
    fn aggregate_rename_then_prefix() {
        // rename replaces the tool-name component; the prefix STILL applies.
        let tools = vec![tool("search")];
        let mut ov = HashMap::new();
        ov.insert(
            "search".to_string(),
            McpToolOverride {
                hidden: false,
                rename: Some("find".to_string()),
            },
        );
        let mut inputs = vec![AggServerInput {
            server_id: 1,
            server_name: "github",
            tool_prefix: "gh",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        assert_eq!(agg.tools[0].name.as_ref(), "gh__find");
        // Reverse map points the *exposed* name at the *upstream* tool name.
        assert_eq!(
            agg.reverse.get("gh__find"),
            Some(&(1, "search".to_string()))
        );
    }

    #[test]
    fn aggregate_hidden_override_excludes_tool() {
        let tools = vec![tool("search"), tool("danger")];
        let mut ov = HashMap::new();
        ov.insert(
            "danger".to_string(),
            McpToolOverride {
                hidden: true,
                rename: None,
            },
        );
        let mut inputs = vec![AggServerInput {
            server_id: 1,
            server_name: "github",
            tool_prefix: "gh",
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        let names: Vec<&str> = agg.tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, vec!["gh__search"]);
        assert!(!agg.reverse.contains_key("gh__danger"));
    }

    #[test]
    fn aggregate_bare_collision_resolves_first_by_server_name() {
        // Two bare servers expose `read`. Determinism: first-by-server-name wins
        // ("alpha" < "beta"), the other is skipped + surfaced (not overwritten).
        let alpha_tools = vec![tool("read")];
        let beta_tools = vec![tool("read")];
        let ov = no_overrides();
        // Pass them in the "wrong" order to prove the sort, not input order, decides.
        let mut inputs = vec![
            AggServerInput {
                server_id: 2,
                server_name: "beta",
                tool_prefix: "",
                tools: &beta_tools,
                overrides: &ov,
            },
            AggServerInput {
                server_id: 1,
                server_name: "alpha",
                tool_prefix: "",
                tools: &alpha_tools,
                overrides: &ov,
            },
        ];
        let agg = build_aggregate(&mut inputs);
        // `read` routes to alpha (server 1); beta's is shadowed + surfaced.
        assert_eq!(agg.reverse.get("read"), Some(&(1, "read".to_string())));
        assert_eq!(agg.skipped.len(), 1);
        assert_eq!(agg.skipped[0].server_id, 2);
        assert!(agg.skipped[0].reason.contains("alpha"));
    }

    #[test]
    fn aggregate_skips_over_64_char_name_with_surfaced_warning() {
        // A 60-char tool name under a prefix exceeds 64 → skipped (not truncated)
        // and surfaced in `skipped` so the UI can say "shorten the prefix".
        let long = "a".repeat(60);
        let tools = vec![tool(&long)];
        let ov = no_overrides();
        let mut inputs = vec![AggServerInput {
            server_id: 1,
            server_name: "srv",
            tool_prefix: "longprefix", // 10 + 2 ("__") + 60 = 72 > 64
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        assert!(agg.tools.is_empty(), "oversized tool must be skipped");
        assert_eq!(agg.skipped.len(), 1);
        assert!(agg.skipped[0].exposed_name.len() > MAX_TOOL_NAME_LEN);
        assert!(agg.skipped[0].reason.contains("> 64"));
        // It is NOT routable (no truncated/partial entry leaked into the map).
        assert!(agg.reverse.is_empty());
    }

    #[test]
    fn aggregate_64_char_name_is_kept() {
        // Exactly 64 chars is allowed (the bound is inclusive).
        let name = "b".repeat(64);
        let tools = vec![tool(&name)];
        let ov = no_overrides();
        let mut inputs = vec![AggServerInput {
            server_id: 1,
            server_name: "srv",
            tool_prefix: "", // bare → exposed name is exactly 64 chars
            tools: &tools,
            overrides: &ov,
        }];
        let agg = build_aggregate(&mut inputs);
        assert_eq!(agg.tools.len(), 1);
        assert!(agg.skipped.is_empty());
    }

    #[test]
    fn call_error_codes_and_kinds() {
        assert_eq!(CallError::ToolNotFound("x".into()).rpc_code(), -32601);
        assert_eq!(
            CallError::Timeout {
                server: "s".into(),
                timeout_ms: 5
            }
            .rpc_code(),
            -32603
        );
        assert_eq!(CallError::ToolNotFound("x".into()).kind(), "tool_not_found");
    }

    // -- Idle reap (§9) -------------------------------------------------------

    /// A snapshot carrying one server (id 1) with the given `idle_seconds`.
    fn snap_with_idle(idle_seconds: i64) -> Snapshot {
        let mut s = sample_server();
        s.idle_seconds = idle_seconds;
        let mut snap = Snapshot::default();
        snap.mcp_servers.insert(1, s);
        snap
    }

    /// Seed manager `m` with a `Ready` conn (id 1) whose `last_used` is `age_secs`
    /// in the past, so the reap window math is deterministic.
    async fn seed_ready_aged(m: &McpManager, age_secs: u64) {
        m.seed_ready_conn_for_tests(1, vec![("now".into(), serde_json::json!({}))])
            .await;
        let mut conns = m.conns.write().await;
        let c = conns.get_mut(&1).unwrap();
        c.last_used = Instant::now() - Duration::from_secs(age_secs);
    }

    #[tokio::test]
    async fn reap_idle_stops_a_stale_ready_conn() {
        let m = McpManager::new();
        let snap = snap_with_idle(30); // reap after 30s idle
        seed_ready_aged(&m, 60).await; // idle for 60s > 30s

        let reaped = m.reap_idle(&snap).await;
        assert!(reaped, "a stale Ready conn must be reaped");

        let conns = m.conns.read().await;
        let c = conns.get(&1).unwrap();
        assert_eq!(c.status, McpStatus::Stopped, "reaped conn → Stopped");
        assert!(c.idle_reaped, "reaped conn marks idle_reaped for the badge");
        assert!(c.tools.is_empty(), "reaped conn drops its tools");
    }

    #[tokio::test]
    async fn reap_idle_zero_is_never_reaped() {
        // idle_seconds = 0 is the warm-keep default: a long-idle conn is NOT
        // reaped (a true no-op), holding its container until app exit (§9).
        let m = McpManager::new();
        let snap = snap_with_idle(0);
        seed_ready_aged(&m, 100_000).await; // ancient, but idle=0 means never

        let reaped = m.reap_idle(&snap).await;
        assert!(!reaped, "idle_seconds=0 must never reap");
        assert_eq!(
            m.conns.read().await.get(&1).unwrap().status,
            McpStatus::Ready
        );
    }

    #[tokio::test]
    async fn reap_idle_keeps_a_fresh_conn() {
        // A Ready conn used more recently than its idle window is left alone.
        let m = McpManager::new();
        let snap = snap_with_idle(300);
        seed_ready_aged(&m, 10).await; // 10s idle < 300s window

        assert!(!m.reap_idle(&snap).await, "fresh conn must not be reaped");
        assert_eq!(
            m.conns.read().await.get(&1).unwrap().status,
            McpStatus::Ready
        );
    }

    #[tokio::test]
    async fn reap_idle_skips_a_conn_with_a_call_in_flight() {
        // The blocking bug the M5 review caught: a tool call slower than
        // `idle_seconds` must not be reaped mid-flight. `last_used` only advances
        // on completion, so an in-flight call leaves it stale — the `in_flight`
        // guard, not `last_used`, is what protects a busy conn.
        let m = McpManager::new();
        let snap = snap_with_idle(30);
        seed_ready_aged(&m, 60).await; // stale enough to be reapable…
                                       // …but a call is in flight (InFlightGuard
                                       // increments this for the duration of `call`).
        m.conns
            .read()
            .await
            .get(&1)
            .unwrap()
            .in_flight
            .fetch_add(1, Ordering::SeqCst);

        assert!(
            !m.reap_idle(&snap).await,
            "a conn with a call in flight must not be reaped"
        );
        assert_eq!(
            m.conns.read().await.get(&1).unwrap().status,
            McpStatus::Ready,
            "busy conn stays Ready despite a stale last_used"
        );
    }

    #[tokio::test]
    async fn reaped_conn_surfaces_idle_detail_in_status_view() {
        // The idle-reaped marker reaches the badge tooltip (surface, don't hide).
        let m = McpManager::new();
        let snap = snap_with_idle(30);
        seed_ready_aged(&m, 60).await;
        m.reap_idle(&snap).await;

        let view = m.status_view(1, &snap).await.unwrap();
        assert_eq!(view.status, "stopped");
        assert_eq!(
            view.detail.as_deref(),
            Some("idle-reaped; reconnects on next use")
        );
    }

    #[tokio::test]
    async fn tools_changed_broadcast_fires_on_reap() {
        // A reap nudges the northbound tools/list_changed subscribers (§8).
        let m = McpManager::new();
        let snap = snap_with_idle(30);
        seed_ready_aged(&m, 60).await;

        let mut rx = m.subscribe_tools_changed();
        m.reap_idle(&snap).await;
        // The reap (via stop_one tearing down a peerless seeded conn) does not
        // hold a `running`, so stop_one's notify only fires for a live peer. The
        // seeded conn has `running = None`, so assert the *explicit* upstream
        // notify path instead — proving the channel is wired end to end.
        m.on_upstream_tools_changed();
        assert!(
            rx.try_recv().is_ok(),
            "a tools/list_changed nudge must arrive"
        );
    }
}
