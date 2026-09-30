# MCP Gateway (lmgw) — Design

**Date:** 2026-06-29
**Status:** Draft v2 — revised after an adversarial design review

> Companion to [2026-06-09-llm-api-gateway-design.md](2026-06-09-llm-api-gateway-design.md).
> Section refs like “§8” point at that document; this spec adds the **MCP plane**
> alongside the existing **LLM plane** in the same binary, reusing its patterns.
> **v2** folds in an Opus design review: sampling must declare the client
> capability or it’s dead on arrival (§8); the “in-process logging is free” claim
> was wrong (§8); the northbound MUST-list + no-batch (§6); stats-pollution fixes
> (§10); 64-char tool names + result content types (§7); the lazy-list contract
> (§9); Origin validation (§13); and a northbound-first de-risking spike (§17.0).

## 1. Summary

lmgw gains a second front door: **MCP**. Just as the LLM plane exposes one stable
OpenAI/Anthropic endpoint and fans out to many model upstreams, the MCP plane
exposes **one MCP server** to clients (Claude Desktop/Code, Cursor, the lmgw Chat
tab) and fans out to many **registered MCP servers** (filesystem, github, gmail,
context7, custom). lmgw is the MCP **server** northbound and an MCP **client**
southbound — an aggregating proxy.

The payoff for building this *inside* lmgw rather than running a standalone
aggregator (mcp-proxy, mcgravity, …) is the synergy with the LLM plane:

- **Sampling served by our own models.** When an upstream MCP server issues a
  `sampling/createMessage`, lmgw answers it **in-process through its own egress
  adapters** (§7) — it is both the MCP host *and* the model provider. No external
  aggregator can do this. *(In the MVP — see §2.)*
- **Unified observability.** Every `tools/call` lands in the same request log +
  live SSE feed + dashboard as LLM traffic (§10). MCP calls are normally a black
  box; here they are not.
- **One auth front door, secrets stay home.** Clients authenticate to lmgw once
  with an existing gateway API key (§13); lmgw injects the real per-server tokens
  from the `0600` SQLite. Same “don’t hardcode upstream keys” rule, now for tools.

## 2. Locked decisions

| Decision | Choice |
|---|---|
| MVP scope | **Aggregator + sampling synergy** — tools aggregation *and* sampling-served-by-our-models in the first cut |
| Southbound client | **Official `rmcp` crate** (`RoleClient`): handles transports, handshake, notifications |
| Northbound server | **Hand-rolled** JSON-RPC dispatcher over Axum (lmgw’s no-framework style), **reusing `rmcp`’s `model` types** for the schema |
| Northbound transport | **Streamable HTTP** (`POST /mcp` + `GET /mcp` SSE). stdio-bridge subcommand deferred |
| Southbound transports | **stdio** (subprocess) **+ Streamable HTTP / legacy SSE** (remote) |
| stdio execution | **Podman-isolated by default** (`podman run --rm -i <image>`); bare subprocess is opt-in |
| Tool namespacing | Per-server **`__` prefix** (e.g. `github__search`); reversible on call routing |
| Config vs live state | Server **definitions** in `Snapshot` (atomic-swap); **live connections** in `AppState.mcp` (`McpManager`), reconciled on reload |
| Persistence | SQLite (`mcp_servers`, `mcp_tool_overrides`); tool-call logs reuse `request_logs` |
| Secret storage | Plaintext in SQLite, `0600` (consistent with §13) |

### Non-goals (this iteration)
- Resources / prompts aggregation, roots, elicitation (plumbing-cheap follow-ups).
- Per-key / per-client exposure scoping and named server “bundles”.
- Chat tab / Workflows agentic tool-loop (lmgw consuming its own MCP tools).
- **MCP-over-completions**: injecting MCP tools into `/v1/chat/completions` &
  `/v1/messages` and running the tool loop gateway-side.
- Northbound server-initiated sampling (asking the *client* to sample).
- At-rest secret encryption (same posture as the LLM plane).

## 3. Architecture overview

The MCP plane lives in the same process and `Arc<AppState>` as the LLM plane. Two
roles, one aggregating hop:

```
                ┌──────────────────────────── lmgw (one process) ───────────────────────────┐
                │                                                                            │
 MCP client ──HTTP──▶  /mcp  (hand-rolled JSON-RPC dispatch)                                 │
 (Claude Code,         initialize · tools/list · tools/call · ping       Arc<AppState>       │
  Cursor, Chat tab)        │                                                                 │
                           │     aggregate (cached, __-prefixed)                             │
                           ▼                                                                 │
                      McpManager  ──(rmcp RoleClient peers)──┬─▶ stdio server (podman run -i)│
                       reconcile(Snapshot)                   ├─▶ stdio server (podman run -i)│
                       status poll (§ reuse 5s tick)         └─▶ remote server (HTTP / SSE)  │
                           ▲                                                                 │
                           │  ClientHandler::create_message (sampling)                       │
                           └──────────────▶ egress::for_protocol(...).build_chat(...)  ──────┼─▶ model upstream
                                            (in-process, logged like any request)            │   (local or cloud)
                └────────────────────────────────────────────────────────────────────────────┘
```

**Config vs live connections.** This is the central structural call. `Snapshot`
is immutable, cheap to clone, atomically swapped (`ArcSwap`) — right for *server
definitions*, wrong for *live sessions* (long-lived stdio pipes, HTTP session
ids, cached tool lists). So definitions go in `Snapshot.mcp_servers`; connections
go in a new `AppState.mcp: McpManager`, **reconciled** on `reload_snapshot()` —
exactly the `RouterManager` / `HfManager` split (§3 crate layout, §8).

## 4. Concepts map onto the existing gateway

| LLM plane (today) | MCP plane (new) |
|---|---|
| `Upstream` (protocol, base_url, key, headers) | `McpServer` (transport, command/url, env/headers) |
| `ModelAlias` → upstream model | exposed tool name → server tool |
| `expose_prefix` / `expose_all` passthrough | per-server `tool_prefix` namespacing |
| `hidden_passthrough_models` | `mcp_tool_overrides` (hide / rename) |
| `Snapshot::resolve(alias) → Route` | `Snapshot::resolve_tool(name) → (server, tool)` |
| `RouterManager` (Podman llama-server lifecycle) | `McpManager` (Podman/stdio + HTTP lifecycle) |
| `request_logs` + live SSE feed (§10) | same table/feed, `ingress_proto = "mcp"` |
| gateway API keys hold real provider keys (§13) | same keys; lmgw holds GitHub/Gmail/etc. tokens |

## 5. Southbound — `rmcp` client connections

One `rmcp` `RoleClient` peer per enabled server, owned by `McpManager`. Verified
`rmcp` surface (docs.rs, 2026-06):

- **Connect:** `let running = GatewayClientHandler{..}.serve(transport).await?;`
  yields a `RunningService<RoleClient, _>`; `running.peer()` → `Peer<RoleClient>`.
- **List:** `peer.list_all_tools().await -> Vec<Tool>` (pages internally).
- **Call:** `peer.call_tool(CallToolRequestParams::new(name).with_arguments(obj)).await
  -> CallToolResult`.
- **Transports (pluggable):**
  - stdio → `TokioChildProcess` (feature `transport-child-process`). The child is
    a `tokio::process::Command`; for Podman isolation the command **is**
    `podman run --rm -i --quiet <image> <server-entrypoint…>` with env as `-e`
    flags. rmcp speaks JSON-RPC over the child’s stdin/stdout.
  - remote → `StreamableHttpClientTransport` (streamable HTTP) and the legacy
    HTTP+SSE client transport. *(Exact feature-flag names: verify, §20.)*

**Why bare subprocess is still supported:** some servers are trusted local tools
where a container is overkill; `container_image = NULL` ⇒ spawn `command`/`args`
directly. Default path sets `container_image` and synthesizes the `podman run`
argv.

### 5.1 Podman-isolated stdio — gotchas
- **Keep the container warm**: one long-lived `-i` process per server (not per
  call); reap via `idle_seconds`. First call on a cold image pays pull+start —
  surface that on the status badge, never hide it.
- **stdout must carry only the MCP stream**: `--quiet`, `--log-level=error`, no
  `-t`; pre-pull the image. Any stray stdout byte corrupts JSON-RPC framing.
- **SELinux bind mounts**: servers that mount host dirs (e.g. filesystem) need
  `:Z`/`:z` on their `-v` flags — via a per-server `extra_run_args`, mirroring
  `RouterSettings.extra_run_args`. Never `:Z` a broad path.

## 6. Northbound — hand-rolled `/mcp`

A JSON-RPC 2.0 dispatcher mounted top-level (sibling to `/v1`), behind the
existing `auth_mw` (§13). It is **more than method dispatch** — a *compliant*
minimal Streamable HTTP server (rev `2025-11-25`) must get a fixed set of MUSTs
right; none need rmcp’s server crate, but skipping them is the difference between
“passes a unit test” and “Claude Code actually connects.”

**`POST /mcp`** — body is a **single** JSON-RPC request, notification, or response.
**No batching** — batch support was removed in MCP 2025-06-18; do not accept it.
- Require `Accept: application/json, text/event-stream`.
- A JSON-RPC **request** → return either a single `application/json` result *or* an
  SSE stream. **MVP returns `application/json` only** (always spec-legal for a
  request), deferring SSE responses + the `GET` channel.
- A JSON-RPC **notification/response** → return **`202 Accepted`, no body**.
- Dispatch:
  - `initialize` → validate `MCP-Protocol-Version` (advertise `2025-11-25`; `400`
    on unsupported; absent header ⇒ assume `2025-03-26`), return capabilities
    (`tools: { listChanged: true }`; resources/prompts later), assign + return
    `MCP-Session-Id`.
  - `tools/list` → the cached aggregate (§7).
  - `tools/call` → `McpManager::call(name, args)` with the owning server’s
    `timeout_ms`; map `CallToolResult` (incl. `isError`) back out.
  - `ping`, `notifications/initialized` → ack.
  - Any non-`initialize`/non-`ping` request **before** initialization completes →
    JSON-RPC error (lifecycle ordering MUST).
- **`MCP-Session-Id`** (note casing): assigned on the `initialize` result; the
  client echoes it on every later request; `400` if required-but-missing; `404`
  once the session is terminated; `DELETE /mcp` ends it.
- **`Origin` validation (security MUST):** reject a mismatched `Origin` with `403`
  to block DNS-rebinding — independent of `auth_mw`, and with auth-off-on-LAN the
  *only* such defense (§13).

**`GET /mcp`** — server→client SSE channel for push (`tools/list_changed`).
~~Optional in MVP, but must still answer `405` when no stream is offered.~~
**Resolved (M5):** the stream is implemented. A **valid session** (`MCP-Session-Id`
present + known) → `200` SSE stream emitting JSON-RPC notifications (no `id`),
pushing `notifications/tools/list_changed` when the aggregate composition changes
(server connect/disconnect/reap, or an upstream `on_tool_list_changed`). A
session-less GET returns the **same session errors as POST** (`400` missing /
`404` unknown) — the `405` placeholder is gone now that a stream is genuinely
offered. The notification source is a `tokio::sync::broadcast` owned by
`McpManager` (separate from the admin telemetry feed); each open stream subscribes
one receiver, dropped on disconnect/`DELETE` (no leak). Because M3 recomputes the
aggregate per read there is **no cache to invalidate** — the push only tells
clients to re-`tools/list`.

**Reuse, don’t re-derive.** Depend on `rmcp`’s `model` types (`Tool`,
`CallToolResult`, `CreateMessageResult`, JSON-RPC envelopes) for serde even though
the dispatch is hand-rolled — one schema, two code paths.

**Sessions.** `HashMap<SessionId, SessionState>` keyed by `MCP-Session-Id`,
created on `initialize`, dropped on disconnect / `DELETE`. Northbound sessions are
independent of southbound sessions — the gateway owns both ends, never tunnels ids.

## 7. Tool aggregation & namespacing

`McpManager` maintains a cached aggregate tool list, rebuilt on connect and on any
`on_tool_list_changed`:

```
exposed_name = match server.tool_prefix {
    ""  => tool.name,                       // bare; first-wins on collision, logged
    p   => format!("{p}__{}", tool.name),   // e.g. github__search
}
```

- **Store the reverse map explicitly** `exposed_name → (server_id,
  upstream_tool_name)`; **never re-derive it by splitting on `__`** at call time — a
  bare server may legitimately expose a tool literally named `x__y`, so splitting is
  ambiguous.
- **64-character name ceiling.** Client tool-name validation (e.g. Claude Code) is
  `^[a-zA-Z0-9_-]{1,64}$`. A prefix + long tool name can exceed 64 → such tools are
  **skipped from the aggregate with a logged, surfaced warning** (no silent
  truncation; the user sees which were dropped and can shorten the prefix). `__`
  stays the separator (`.`/`/` from SEP-986 aren’t universally accepted yet).
- `mcp_tool_overrides` applies **hide** and **rename** — analogous to
  `hidden_passthrough_models` + the model hide/unhide UI.
- Bare-server collisions resolve **first-by-server-name**, deterministic, logged —
  same spirit as `Snapshot::resolve_passthrough`.
- Tool `inputSchema` is forwarded verbatim; lmgw does not validate args (the owning
  server does).
- **Results pass through unchanged.** `CallToolResult.content` may carry text,
  **image, audio, `resource`/`resource_link` blocks**, plus `structuredContent` and
  the `isError` flag — all rmcp `model` types, forwarded verbatim. MVP must not
  assume text-only results. Large results are **not** truncated (no hidden cap); the
  only bound is client/memory, surfaced as a normal error if hit.

## 8. Sampling synergy — the MVP differentiator

`GatewayClientHandler` implements `rmcp`’s `ClientHandler`. Two things must be true
for an upstream server to ever sample us:

1. **We must *declare* the sampling capability at connect time.** Sampling is a
   **client** capability in MCP — an upstream may issue `sampling/createMessage`
   only if our `InitializeRequest` advertised `capabilities.sampling`. rmcp sources
   this from `ClientHandler::get_info() -> ClientInfo`, which **defaults to no
   sampling**. Forgetting to override it means `create_message` is never called and
   the whole differentiator is **dead on arrival**. Because the declaration is set
   at connect, the per-server `allow_sampling` toggle is **reconnect-affecting**
   (it changes `ClientInfo`), not a hot flag.
2. **We answer `create_message` from our own models.**

```rust
fn get_info(&self) -> ClientInfo {
    ClientInfo {
        capabilities: ClientCapabilities {
            sampling: self.allow_sampling.then(Default::default),  // ← the DOA-avoiding line
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn create_message(
    &self,
    params: CreateMessageRequestParams,    // messages, modelPreferences, systemPrompt,
    _cx: RequestContext<RoleClient>,        // maxTokens, temperature, stopSequences …
) -> Result<CreateMessageResult, McpError> {
    let state = self.state.upgrade().ok_or(internal)?;            // Weak → Arc
    let alias = self.sampling_alias.clone()
        .or_else(|| nonempty(state.snapshot().settings.sampling_alias.clone()))
        .ok_or(internal)?;
    let route = state.snapshot().resolve(&alias)?;               // concrete Route
    let ir = ir_from_create_message(&params, &alias);           // → crate::ir::ChatRequest
    let completion = sample_once(&state, &route, &ir, sampling_deadline).await?;
    Ok(create_message_result_from(completion))
}
```

**Reality check on “in-process reuse” (corrected from v1).** There is **no shared
in-process call+record helper today**, and the existing callers do *not* all use
`build_chat`:
- `web/workflows.rs::llm_classify` hand-rolls a **raw `reqwest` POST** to
  `{base}/chat/completions` for the OpenAI path (the local llama-server case);
  `egress…build_chat(...)` is used only on the non-OpenAI fallback branch.
- `web/chat.rs` uses `build_chat(...)` + `proxy::drive_upstream(...)`.
- `proxy::record` + `LogParams` are **private to `proxy.rs`**; only
  `drive_upstream`/`StreamOutcome` are `pub(crate)`. Each in-process caller copies
  its own log boilerplate (`record_llm_call`, `record_chat_call`).

So sampling does **not** get a `request_logs` row “for free.” Part of this work is
to **extract a shared `pub(crate)` in-process call+record helper** in `proxy.rs`
(promote `record`/`LogParams`, or add `sample_once`) and migrate workflows + chat
onto it — one path instead of three copies. Then sampling logs like any request,
as `ingress_proto = "mcp-sampling"` with **real token counts** (it *is* LLM traffic
and should count toward stats, unlike `tools/call` rows — §10).

**Re-entrancy / self-starvation (the real deadlock).** Upstream server →
`create_message` → resolve `sampling_alias`. If that alias points at a **local
llama.cpp model with `models_max = 1`** and the agent that triggered the originating
`tools/call` already holds that single slot, the sampling sub-call blocks on a GPU
slot the caller owns → self-starvation. Mitigations, all in-spec: (a) recommend (and
default the UI hint to) a **dedicated small or cloud `sampling_alias`**, not the same
single-slot local model as the calling context; (b) give the sampling sub-call **its
own deadline**, separate from the §9 tool-call `timeout_ms`, so it fails loudly
instead of hanging; (c) document it.

- **Model selection.** Resolve order: per-server `sampling_alias` → global
  `Settings.sampling_alias`. MCP `modelPreferences` hints are *advisory*: MVP picks
  the configured alias and may loosely match hints against the alias list; full
  weighting is later.
- **Billable traffic an upstream can trigger.** Per no-hidden-caps: don’t silently
  cap it — surface it (it logs) and gate with the visible per-server `allow_sampling`
  toggle (default on), not an invisible limit.

Other `ClientHandler` hooks:
- `on_tool_list_changed` → invalidate aggregate cache, emit northbound
  `tools/list_changed` (now). Needs a new telemetry `Event` variant (§9, §12).
- **Cancellation (MVP behavior, stated):** when a northbound client drops a
  `tools/call` (HTTP disconnect), MVP is *best-effort drop* — it does **not** yet
  send `notifications/cancelled` southbound, so a long tool may run orphaned until
  its `timeout_ms`. Full cancel propagation is a follow-up.
- `on_progress`, `on_logging_message`, `list_roots`, `create_elicitation`,
  resources/prompts → later.

## 9. Lifecycle & reconciliation

`McpManager` (in `AppState`, like `RouterManager`):

```rust
pub struct McpManager {
    conns: RwLock<HashMap<i64, McpConn>>,   // keyed by mcp_servers.id
    state: Weak<AppState>,                  // for the sampling handler (cycle break)
}
struct McpConn {
    running: Option<RunningService<RoleClient, GatewayClientHandler>>, // None = lazy/not-started
    tools: Vec<Tool>,
    status: McpStatus,                      // Connecting | Ready | Stopped | Error(detail)
    last_used: Instant,
}
```

- **`reconcile(&snapshot)`** — diff desired (enabled servers) vs live: start new,
  stop removed, restart changed (config hash mismatch). Called after every
  `reload_snapshot()`.
- **Lazy start** (`autostart = false`): connect on first use. **`tools/list`
  contract (specified):** a first `tools/list` triggers lazy connects, waits a
  **bounded, surfaced per-server budget**, and returns whatever is `Ready` —
  partial, not blocking indefinitely on `podman pull`. A server slower than the
  budget connects in the background; **(M5, resolved)** its connect then fires a
  `tools/list_changed` nudge over `GET /mcp`, so a subscribed client re-lists and
  picks up its tools without a manual refresh (the MVP “appears only on the next
  `tools/list`” caveat is lifted for clients on the GET stream).
- **Idle reap** (`idle_seconds > 0`): **(M5, resolved)** a background sweep on the
  existing 5s tick stops `Ready` connections idle past `idle_seconds` (→ `Stopped`,
  row kept, container released, badge tooltip “idle-reaped; reconnects on next
  use”) — mirrors llama-server `sleep-idle-seconds`. **Default `0` = never reap** (a
  true no-op), so each used stdio server holds a resident Podman container until
  app exit (the right default on a single workstation — “warm-keep” in §5.1 and
  idle-reap are the two ends of this one knob). The reap does **not** fight the
  tick: `poll_statuses` only retries `Error` autostart conns, never `Stopped`, so a
  reaped server stays `Stopped` until next use (lazy `tools/list`/`call`) or an
  explicit config reload (`reconcile` re-Starts a `Stopped` autostart server).
- **Per-stdio concurrency:** one `-i` child is a single JSON-RPC stream; rmcp
  serializes requests over it, so concurrent northbound `tools/call`s to the *same*
  stdio server queue behind each other. High-concurrency servers want the HTTP
  transport. (Not a hidden cap — a transport property, surfaced here.)
- **Status polling** rides the 5s tick in `server::spawn_background_tasks`. This
  needs a **new telemetry `Event` variant** (e.g. `Event::Mcp(status)`): the SSE
  `events()` match in `web/mod.rs` handles only `Request`/`Router` today (and the
  embed router’s status is refreshed-but-not-broadcast), so “same path as the
  router badge” means *adding* an event type, not just a new caller.
- **Timeouts**: every `call_tool` is wrapped in the server’s `timeout_ms`; a hung
  server fails that call, not the gateway.

## 10. Data model (`migrations/0011_mcp_servers.sql`)

```
mcp_servers(
  id INTEGER PK,
  name TEXT UNIQUE NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  transport TEXT NOT NULL,            -- 'stdio' | 'http' | 'sse'
  -- stdio:
  command TEXT,                       -- 'podman' (isolated) or the bare entrypoint
  args TEXT NOT NULL DEFAULT '[]',    -- JSON array
  env TEXT NOT NULL DEFAULT '{}',     -- JSON object (secrets; masked in UI)
  cwd TEXT,
  container_image TEXT,               -- non-NULL ⇒ Podman-isolated; argv synthesized
  extra_run_args TEXT NOT NULL DEFAULT '[]',  -- JSON; GPU/:Z/etc. like RouterSettings
  -- http/sse:
  url TEXT,
  headers TEXT NOT NULL DEFAULT '[]', -- JSON [[name,value]] (auth tokens)
  -- common:
  tool_prefix TEXT NOT NULL DEFAULT '',
  timeout_ms INTEGER NOT NULL DEFAULT 60000,
  autostart INTEGER NOT NULL DEFAULT 1,
  idle_seconds INTEGER NOT NULL DEFAULT 0,
  allow_sampling INTEGER NOT NULL DEFAULT 1,
  sampling_alias TEXT,                -- per-server override (NULL ⇒ global)
  created_at TEXT DEFAULT (datetime('now')),
  updated_at TEXT DEFAULT (datetime('now')))

mcp_tool_overrides(
  server_id INTEGER NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
  tool_name TEXT NOT NULL,            -- upstream (un-prefixed) name
  hidden INTEGER NOT NULL DEFAULT 0,
  rename TEXT,                        -- optional exposed-name override
  PRIMARY KEY (server_id, tool_name))
```

- `Settings` (the hot-path JSON blob, §9 of main doc) gains
  `sampling_alias: String` (empty = none configured).
- **Tool-call logging reuses `request_logs`** with `ingress_proto = "mcp"` (the
  table already carries non-protocol values like `"workflow"`/`"chat"`, so the
  column is migration-free). Server name → `upstream_name`, tool → a **new nullable
  `mcp_tool` column** (decided — cleaner UI filtering than overloading
  `upstream_model`; promoted from §20). **Two stats-pollution fixes are mandatory:**
  1. **Errors.** MCP tool failures return as JSON-RPC errors inside **HTTP 200**
     (and a “tool failed” `CallToolResult` is a *successful* response with
     `isError: true`). The dashboard’s `RowView.ok = status < 400 &&
     error_kind.is_none()` (`web/mod.rs`) would render those green — so **map tool /
     JSON-RPC errors to a non-200 `status` in the log row** and set `error_kind`.
  2. **Tokens.** `prompt/completion_tokens` are meaningless for `tools/call`: leave
     them **NULL** and **exclude `ingress_proto = 'mcp'` from the token / error-rate
     aggregates** in `telemetry.stats()`, or the counters skew.
  Sampling rows (`ingress_proto = "mcp-sampling"`, §8) are the exception — real LLM
  traffic with real tokens, and **should** count toward stats.
- `Snapshot` gains `mcp_servers: HashMap<i64, McpServer>` and
  `mcp_tool_overrides`, loaded in `store::load_snapshot`.

## 11. Config types (`config.rs`)

```rust
/// Persisted to the `transport TEXT` column via hand-written `as_str`/`parse`
/// (lowercase) — matching the `Protocol`/`UpstreamKind` convention used in
/// `store.rs` (row mapper `.unwrap_or(default)`), not serde rename, so persistence
/// stays symmetric with the rest of the store.
#[derive(Clone, Serialize, Deserialize)]
pub enum McpTransport { Stdio, Http, Sse }

#[derive(Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    // stdio
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub container_image: Option<String>,   // Some ⇒ Podman-isolated
    pub extra_run_args: Vec<String>,
    // http/sse
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    // common
    pub tool_prefix: String,
    pub timeout_ms: u64,
    pub autostart: bool,
    pub idle_seconds: i64,
    pub allow_sampling: bool,
    pub sampling_alias: Option<String>,
}

impl McpServer {
    /// Final argv for a stdio server. Podman-isolated when `container_image` set.
    pub fn stdio_argv(&self) -> (String, Vec<String>) { /* synthesize podman run -i … */ }
}
```

`Snapshot::resolve_tool(exposed: &str) -> Option<(&McpServer, String)>` is the MCP
analog of `resolve` — applies prefix stripping + overrides to map an exposed name
back to a server + upstream tool name.

## 12. Web UI (HTMX + Askama) — new “MCP” tab

> **Superseded (2026-08-29).** The htmx/Askama UI is gone; this surface is now
> the SPA's **MCP servers** page (`/mcp-servers`, `crates/lmgw-ui/src/pages/mcp.rs`)
> over `GET /api/mcp-servers` + `POST /api/op/mcp_server_set`. The behaviour
> described below still holds.

Sibling to Upstreams/Models/Local/Embed, mirroring `web/admin.rs` patterns:

- **Servers list** — status badge (Ready/Connecting/Stopped/Error), transport,
  tool count, enable/disable, **Test connection** (connect + `list_tools`, like
  upstream “test”). Live badges via SSE.
- **Add / edit server** — transport-specific form. stdio: command/args/env/cwd +
  `container_image` + `extra_run_args` (with the `:Z` hint inline). http/sse:
  url + headers. Common: prefix, timeout, autostart, idle, allow-sampling,
  sampling-alias (datalist of model aliases).
- **Tool browser** (per server) — discovered tools/resources/prompts; toggle
  hidden, set rename — like the model hide/unhide + passthrough catalog browser.
- **Connect panel** — the `/mcp` URL + copy-paste client config snippets (Claude
  Desktop/Code `mcpServers`, Cursor), echoing the dashboard’s LLM Connect panel.
- **Tool-call activity** — reuses the logs table/feed filtered to `mcp`.

## 13. Auth & secrets

- `/mcp` is gated by the same `auth_mw` and gateway API keys as `/v1` (§13 main).
  Auth off by default on trusted LAN; binding `0.0.0.0` should prompt enabling it.
- **`Origin` validation (MUST, not optional).** `/mcp` rejects a mismatched
  `Origin` with `403` (DNS-rebinding defense, §6). With auth-off-on-LAN as the
  default, this is the *only* such protection, so enforce it **regardless of the
  auth toggle**.
- Per-server secrets (env tokens, headers) are plaintext in the `0600` SQLite,
  masked in the UI, never logged — consistent with the LLM plane.
- Per-key → allowed-server scoping is deferred (non-goal §2).

## 14. Error handling

- Northbound errors are returned as **JSON-RPC error objects** (code + message),
  not OpenAI/Anthropic shapes — `/mcp` is a JSON-RPC surface. Map: unknown tool →
  `-32601`-style method/tool-not-found; upstream/transport failure → internal
  error with detail preserved in the log; per-call timeout → internal error
  “server timed out”.
- A southbound server crash marks its `McpConn` `Error(detail)`, surfaces on the
  badge, and fails in-flight calls cleanly. Reconnect uses **bounded exponential
  backoff** — a crash-looping server (bad image, missing token) must not be
  `podman run` on every single call; cap the retry rate and surface the failing
  state instead of hot-looping.
- Sampling errors propagate back to the requesting server as `McpError` and are
  logged like any failed LLM request.

## 15. Module / file layout

```
crates/lmgw-core/src/
  mcp/
    mod.rs        // McpManager, McpConn, reconcile, aggregation, call routing
    handler.rs    // GatewayClientHandler: create_message (sampling) + notifications
    ingress.rs    // hand-rolled northbound JSON-RPC dispatch + session map
  web/mcp.rs      // MCP tab: servers CRUD, tool browser, Connect panel, activity
  config.rs       // + McpServer, McpTransport, Snapshot fields, resolve_tool
  store.rs        // + mcp_servers / mcp_tool_overrides CRUD + load_snapshot
  state.rs        // + AppState.mcp: McpManager
  server.rs       // + .route("/mcp", post(..).get(..)) under auth_mw; reconcile on boot
```

`Cargo.toml`: add `rmcp` with features `client`, `transport-child-process`,
`transport-streamable-http-client`, and `client-side-sse` (legacy HTTP+SSE client).
Pin a version and wrap rmcp types behind the `mcp` module boundary so version churn
stays contained. *(Confirm exact flag names against the pinned version — §20.)*

## 16. Testing strategy (TDD)

- **Aggregation/namespacing:** prefix application, hide/rename overrides, bare
  collision determinism — pure-function tests on a fixture tool set.
- **Northbound dispatch:** golden JSON-RPC for `initialize` (capabilities +
  session id), `tools/list`, `tools/call` happy path + tool-not-found, against a
  fake `McpManager`.
- **Sampling handler:** `CreateMessageRequestParams` → IR mapping + `Completion`
  → `CreateMessageResult` round-trip; end-to-end with a `wiremock` model upstream
  asserting a `request_logs` row is written (proves the observability claim).
- **stdio argv synthesis:** snapshot tests of `podman run -i` construction
  (isolated vs bare; env, `:Z`, extra args) — mirrors the preset snapshot tests.
- **Reconcile:** desired-vs-live diff (start/stop/restart) with a mock peer
  runner, mirroring the Podman command-construction tests (§15 main).
- **Live integration (gated):** spin a reference stdio server (e.g.
  `@modelcontextprotocol/server-everything`) in Podman, list + call a tool,
  trigger a sampling request, assert it’s served by a configured alias.

## 17. Build milestones

0. **Northbound spike (de-risk first):** hand-rolled `/mcp`
   `initialize` → `tools/list` → `tools/call` over Streamable HTTP serving **one
   hard-coded stub tool**, validated against a real **Claude Code / Cursor** client.
   This is the only library-unbacked surface and carries the most MUST-rules (§6);
   if a real client can’t list+call the stub, fall back to rmcp’s
   `transport-streamable-http-server` before investing in the rest.
1. **Config plane:** `mcp_servers` migration + `McpServer`/`Snapshot` +
   store CRUD; MCP tab list/add/edit (no connections yet).
2. **Southbound connect:** `McpManager` + `rmcp` stdio (Podman) + HTTP; connect,
   `list_tools`, status badges, reconcile on reload, Test-connection.
3. **Northbound tools:** hand-rolled `/mcp` `initialize`/`tools/list`/`tools/call`
   + sessions; aggregation + `__` prefix + hide/rename; tool-call logging into the
   unified feed; Connect panel.
4. **Sampling synergy:** `GatewayClientHandler::create_message` → in-process
   egress; per-server allow-sampling + sampling-alias; logged + observable.
5. **Polish (done):** idle reap, `tools/list_changed` push (`GET /mcp` SSE), error
   normalization, docs/Connect snippets, gated live integration test.

## 18. MVP cut line

**In:** stdio-Podman + HTTP/SSE southbound via `rmcp` · hand-rolled `/mcp`
(`initialize`, `tools/list`, `tools/call`, `ping`) · `__` namespacing +
hide/rename · servers CRUD + status + Connect panel · **sampling served by our
models** · unified tool-call logging.
**Deferred:** resources/prompts, roots/elicitation, key-scoped exposure +
bundles, Chat/Workflows agentic loop, MCP-over-completions injection, northbound
server-initiated sampling.

## 19. Risks

- **rmcp version churn / non-exhaustive structs** — pin a version; confine rmcp
  types to the `mcp` module so an upgrade is a localized change.
- **Streamable HTTP spec compliance** — POST-returns-SSE and session/protocol
  header negotiation have edge cases; a JSON-only minimal server is partial. Test
  against real clients (Claude Code, Cursor) early.
- **Podman cold-start + stdout cleanliness** — first-call latency; any stray
  stdout breaks framing (§5.1).
- **Handler ↔ AppState cycle** — `McpManager` holds the handler which needs
  `AppState`; broken with `Weak<AppState>`, upgraded per sampling call.
- **Sampling cost amplification** — an upstream can drive billable LLM calls;
  mitigated by the visible per-server toggle + logging, not a hidden cap.
- **Tool-name collisions / oversized aggregate catalogs** — many servers ×
  many tools can bloat a client’s tool list; prefix + hide help, bundles later.
- **Sampling DOA if capability undeclared** — `create_message` is never called
  unless `ClientInfo` advertises `sampling` (§8); trivially easy to miss.
- **Sampling self-starvation** — a `sampling_alias` pointing at the same single-slot
  local model as the calling agent can deadlock; dedicated alias + own deadline (§8).
- **Stats pollution** — MCP tool errors are HTTP 200 / `isError`; without the §10
  status-mapping + stats-exclusion fix, the error rate and token counters skew.
- **Hand-rolled compliance** — the §6 MUST-list (Accept / 202 / session / version /
  GET-SSE-or-session-error / Origin / no-batch) is the real risk surface; spike it
  first (§17.0). *(M5 replaced the GET `405` placeholder with the real SSE stream.)*

## 20. Open verification items (resolve during implementation, not blocking)
*Resolved by the design review (folded into §5/§6/§8/§10): rmcp signatures &
feature-flag names; `application/json`-only POST responses are spec-legal for MVP;
advertise `2025-11-25`; batching is removed; the new `mcp_tool` column +
stats-exclusion; and the sampling-capability declaration.*

Still open:
- **`MCP-Session-Id` reconnect / resumability** across Claude Code / Cursor (do they
  reconnect with `Last-Event-ID`; how strict on `404`-after-terminate).
- `podman run -i` **quiet-stdout** flags that fully silence non-MCP output on this
  Fedora+NVIDIA host; image pre-pull strategy on first connect.
- `modelPreferences` → alias mapping policy beyond “use the configured default.”
- Bounded **lazy-connect budget** (§9) and reconnect **backoff** params (§14).
- Exact rmcp `ClientCapabilities`/`ClientInfo` shape for the §8 `get_info` override
  on the pinned version.

### Implementation notes (resolved during the build)
*Factual record of what the §15/§20 open items landed on — not a rewrite of the
design.*

- **rmcp pinned to `2.0.0`** (`default-features = false`). Enabled features:
  `client` (the `RoleClient` core), `transport-child-process` (stdio/Podman), and
  `transport-streamable-http-client-reqwest`. That last flag **transitively**
  pulls in `transport-streamable-http-client` + `client-side-sse` — the SSE flag
  is only the *response parser* the streamable-HTTP client uses, **not** a
  standalone legacy SSE client transport.
- **No standalone legacy SSE client transport in rmcp 2.0.** The MCP rev
  2024-11-05 “HTTP+SSE” client (separate `GET` stream + `endpoint` event) is gone;
  the streamable-HTTP client also parses `text/event-stream` responses, so both
  the `http` and `sse` `McpTransport` variants route through one
  `StreamableHttpClientTransport` (see `mcp/mod.rs::connect`). The MCP-tab form
  notes this.
- **Sampling types are SEP-2577-deprecated** in rmcp 2.0.0 with **no successor
  API** — `create_message`/`CreateMessageRequestParams`/`CreateMessageResult` are
  still the only way the protocol delivers `sampling/createMessage`, so the
  sampling module (`mcp/handler.rs`) uses them under a scoped
  `#![expect(deprecated)]` that self-reports if a future rmcp un-deprecates or
  removes them.
- **`on_tool_list_changed` signature** (M5): `fn on_tool_list_changed(&self,
  context: NotificationContext<RoleClient>)` — overridden to forward a
  `tools/list_changed` nudge onto the manager's northbound broadcast.
- **`MCP-Session-Id`** is still an in-process module static (`HashSet`); reconnect
  / `Last-Event-ID` resumability across clients remains an open item above (the
  GET stream is fresh-per-connection, no event replay).
- **Northbound notification channel** is a dedicated `tokio::sync::broadcast<()>`
  on `McpManager` (buffer `TOOLS_CHANGED_BUFFER = 64`, a visible const), kept
  **separate** from the admin-UI telemetry `Event` feed per §9/§19.