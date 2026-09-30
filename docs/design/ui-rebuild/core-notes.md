# lmgw-core internals brief — for the new `/api` (JSON+SSE) admin plane

Condensed map of the machinery a new Leptos-facing `/api` layer would wrap.
Every claim below is `file.rs:line` — jump in rather than re-reading the
crate. Written to prime API design only; no redesign proposals here.

---

## 1. `state.rs` — `AppState` / `SharedState`

- `AppState` (`state.rs:21-41`): fields — `db: SqlitePool`, `snapshot:
  ArcSwap<Snapshot>` (private), `telemetry: TelemetryBus`, `router:
  RouterManager` (chat), `embed_router: RouterManager`, `audio: AudioManager`,
  `audio_catalog: Mutex<Option<CatalogSnapshot>>`, `hf: HfManager`, `mcp:
  McpManager`, `http: reqwest::Client`, `catalog: CatalogCache`,
  `started_at: Instant`, `data_dir: PathBuf`.
- `SharedState = Arc<AppState>` (`state.rs:43`).
- Config is **not** read from the DB on the hot path. `AppState::snapshot()`
  (`state.rs:120-122`) is a cheap `ArcSwap::load_full()` returning
  `Arc<Snapshot>`. Every mutation goes: write to SQLite via `store::*` →
  `state.reload_snapshot()` (`state.rs:139-144`), which reloads from the DB,
  atomically swaps the snapshot, and reconciles live MCP connections
  (`self.mcp.reconcile(&snap)`). **A new `/api` write handler must call
  `reload_snapshot()` after every DB write**, exactly like `ops.rs` does — the
  snapshot never updates itself.
- `AppState::init` (`state.rs:57-88`) opens the DB, loads the snapshot, wires
  up the three `RouterManager`/`AudioManager` instances (one config dir each:
  `llama-server`, `llama-embed`, `audiocpp`), and breaks the
  `McpManager` ↔ `AppState` `Arc` cycle via a `Weak` handed to
  `mcp.set_state(&app)`.
- `init_for_tests` (`state.rs:91-117`) — in-memory DB variant; useful for a
  future `/api` integration test harness.
- Two managed llama-server containers exist (chat + embed) plus one audio.cpp
  container — never conflate them; each has its own `RouterSettings`/
  `AudioSettings` and its own `RouterManager`/`AudioManager`.

## 2. `config.rs` — domain model + `Snapshot`

Everything here is `Serialize + Deserialize`, so most types can be reused
directly as API DTOs (secrets need manual redaction — see §7 traps).

- `Protocol` (`openai|anthropic|gemini`, `config.rs:15-38`), `UpstreamKind`
  (`generic|llama_server|audio_cpp`, `config.rs:42-66`).
- `Upstream` (`config.rs:69-102`): `id, name, protocol, kind, base_url,
  api_key: Option<String>, extra_headers: Vec<(String,String)>, timeout_ms,
  enabled, expose_all, expose_prefix, supports_responses`.
- `ModelAlias` (`config.rs:117-124`): `id, alias, upstream_id,
  upstream_model_id, param_overrides: Params, enabled`.
- `LocalModel` (chat GGUF, `config.rs:128-148`): `id, model_id, gguf_path,
  params: LlamaParams, args: Vec<String>, idle_seconds, enabled, public`.
  `hoist_promoted_args()` (`config.rs:193-289`) migrates legacy freeform
  `args` entries into typed `LlamaParams` fields on every load — a `/api` read
  handler gets the already-hoisted form for free via `store::list_local_models`.
- `LlamaParams` (`config.rs:447-533`) — the full llama-server param surface;
  every field `Option`/`bool`, `None`/`false` = flag omitted. Fields: `ctx_size,
  n_gpu_layers, threads, batch_size, ubatch_size, parallel, flash_attn,
  cache_type_k, cache_type_v, cache_ram, jinja, chat_template_file,
  reasoning_format, reasoning, reasoning_budget, reasoning_preserve,
  reasoning_effort, chat_template_kwargs: serde_json::Map, temp, top_p, top_k,
  min_p, repeat_penalty, presence_penalty, seed, mmproj_path, no_mmproj,
  draft_gguf_path, spec_type, spec_draft_n_max, spec_draft_n_min,
  spec_draft_ngl, fit, fit_ctx`. This is the struct
  `LocalModelPatch` in `ops.rs` mirrors field-for-field (flattened, sparse).
- `EmbedModel` (`config.rs:377-393`): `id, model_id, gguf_path, pooling:
  Option<String>, ctx_size: Option<i64>, args, idle_seconds, enabled`.
- `AudioModel` (`config.rs:401-431`): `id, model_id, family, path, task, mode,
  load_options/session_options/voice_presets: serde_json::Map,
  default_voice_preset: Option<Value>, enabled`.
- `ApiKey` (`config.rs:536-542`): `id, name, key_hash` (hex sha256, never
  the raw key), `enabled`.
- `McpTransport` (`stdio|http|sse`, `config.rs:557-585`), `McpServer`
  (`config.rs:600-624`): `id, name, enabled, transport, command, args, env:
  Vec<(String,String)>, cwd, container_image, extra_run_args, url, headers,
  tool_prefix, timeout_ms, autostart, idle_seconds, allow_sampling,
  sampling_alias`.
- `SelfAdmin` (`off|read_only|full`, `config.rs:689-726`) — gates the
  `lmgw__*` self-admin MCP tool plane (`allows_read`/`allows_write` helpers).
  **Not settable through the tools it gates** (an agent can't widen its own
  grant); same principle should probably apply to any `/api` settings route.
- `Settings` (`config.rs:744-838`) — one JSON blob, whole struct: `bind_addr,
  auth_enabled, retention_days, retention_max_rows, router: RouterSettings,
  embed_router: RouterSettings, audio: AudioSettings, hf_token,
  update_check_enabled, update_token, sampling_alias, self_admin,
  self_admin_token, responses_max_tool_calls, responses_timeout_seconds,
  responses_store, responses_retention_hours, responses_max_chains,
  max_body_mb`.
- `RouterSettings` (`config.rs:889-929`): `image, container_name,
  listen_port, models_dir, extra_run_args: Vec<String>, models_max: u32,
  public_prefix, auto_start`. Two instances (chat default port 9292, embed
  default 9293, `public_prefix: "embed"`, `models_max: 0`).
- `AudioSettings` (`config.rs:953-979`): `image, container_name,
  listen_port (9294), models_dir, backend, device, threads, lazy_load,
  extra_run_args, public_prefix ("audio"), auto_start`.
- `Snapshot` (`config.rs:1036-1049`): `upstreams: HashMap<i64,Upstream>,
  aliases: HashMap<String,ModelAlias>` (keyed lowercased alias),
  `local_models: Vec<LocalModel>, api_keys, settings: Settings,
  hidden_passthrough: HashSet<(i64,String)>, mcp_servers:
  HashMap<i64,McpServer>, mcp_tool_overrides`.
  Key methods: `resolve(alias)` (`:1054-1075`, alias routing precedence),
  `router_upstream()` (`:1079-1097`, synthesized upstream for public locals),
  `exposed_models()` (`:1178-1198`, statically-known `/v1/models` entries —
  names and sources only; the model-capabilities design layers `capabilities`
  and `notes` on top of each one in `capabilities::exposed::exposed_entries`),
  `verify_api_key()` (`:1202-1207`).
- `Route`/`ExposedModel` (`:1014-1032`) are resolution results, not persisted
  rows — unlikely to be DTOs directly but shape what `/v1/models` reports.

## 3. `telemetry.rs` — event bus + live stats

- `RequestSummary` (`telemetry.rs:15-32`): `log_id, ts (RFC3339 UTC),
  ingress_proto, requested_alias, upstream_name, upstream_model, egress_proto,
  status: u16, ttfb_ms, total_ms, prompt_tokens, completion_tokens, streamed,
  error_kind, error_msg` — this **is** the row shape for a live-log SSE
  stream; already `Serialize`.
- `Event` enum (`telemetry.rs:35-43`), three variants:
  - `Request(RequestSummary)` — one per finished request/tool call.
  - `Router(RouterStatus)` — chat container status changes only (embed/audio
    containers are polled but not separately broadcast, `server.rs:206-208`).
  - `Mcp(Vec<McpStatusView>)` — full southbound MCP status list on any change.
- `StatsView` (`telemetry.rs:47-57`, `Serialize`): `total_requests,
  total_errors, prompt_tokens, completion_tokens, req_last_minute,
  err_last_minute, active_requests: i64`. Trailing-60s window recomputed on
  each `stats()` call (`:176-188`).
- `TelemetryBus` (`telemetry.rs:69-72`): a `tokio::sync::broadcast::Sender
  <Event>` (buffer 256) + a `Mutex<Stats>`. `subscribe()` (`:124-126`) hands
  out a `Receiver<Event>` — this is exactly what a new `/api/events` SSE
  handler would call, mirroring `web/mod.rs:581-637`'s existing `/events`.
  A lagged receiver silently drops frames (`Err(_) => {}` in `web/mod.rs:630`);
  a JSON SSE layer should probably do the same or resync from a snapshot.
- `counts_in_token_stats()` (`:93-98`) / the `RESPONSES_TOOL_PROTO` /
  `ADMIN_TOOL_PROTO` / `"mcp"` exclusions (`:100-113`) — tool-call rows get a
  log row + SSE frame but never move `active`/`total_requests`/error-rate
  counters. Relevant if a new UI shows both a "live feed" and "stats tiles"
  fed from the same events — they intentionally disagree in row count.

## 4. `store.rs` — SQLite persistence (sqlx)

Cold path only; nothing here should run per-request from the proxy, but it is
exactly what a new `/api` CRUD/list layer should call.

- `open()` (`store.rs:20-44`): WAL mode, FK on, `0600` perms (SQLite file
  holds plaintext secrets). `open_in_memory()` (`:47-55`) for tests.
- `load_snapshot()` (`:153-218`) — the one function that builds a `Snapshot`
  from all tables; called on boot and by `reload_snapshot()`.
- Generic `get_kv`/`set_kv` (`:245-263`) — namespaced key/value blobs outside
  the hot-path `Settings` (e.g. `workflow:mail`); a future `/api` feature that
  needs its own small persisted blob can reuse this instead of a migration.
- CRUD sections, each with `NewX` struct (insert/update payload) + `list_x`/
  `get_x`/`insert_x`/`update_x`/`delete_x`, all `async fn(&SqlitePool, ...) ->
  DbResult<T>` (`DbResult<T> = Result<T, GatewayError>`, `:17`):
  - Upstreams: `:269-389` (`NewUpstream`, `update_upstream` has an
    `update_key: bool` flag so a masked-key PATCH doesn't clobber the stored
    secret — same pattern needed in `/api`).
  - Aliases: `:395-451`.
  - Local models: `:457-521`.
  - Embed models: `:527-608`.
  - Audio models: `:614-729` (4 JSON columns: load/session options, voice
    presets, default voice preset).
  - HF downloads (`HfModelRow`, `:738-858`): `id, repo, file, dest_path,
    target ("chat"|"embed"|"audio"), etag, size_bytes, status
    (queued|downloading|done|failed|update_available), error,
    downloaded_at`. `upsert_hf_model` re-queues on conflict
    (`repo,file,target` unique) (`:798-817`).
  - API keys: `:864-879` (insert/delete only — no update; rotate by
    delete+insert).
  - Hidden passthrough models: `:885-911` (per `(upstream_id, model_id)`).
  - MCP servers: `:917-1031`.
  - Request logs (`RequestLogRow`, `:1037-1164`): same fields as
    `RequestSummary` plus `client_key, mcp_tool`. `query_logs` takes a
    `LogFilter { alias, upstream_name, errors_only, limit, before_id }`
    (`:1130-1156`) — `before_id` is the pagination cursor (id-descending).
  - Chat threads/messages (`:1170-1343`) — the existing "Chat" tab's own
    persisted conversations (`kind: "chat"|"admin"`), distinct from the
    OpenAI-Responses `responses` table below. `ChatMessageRow.ir_messages`
    carries the full agentic turn (tool calls) as JSON for Admin Chat.
  - Stored `/v1/responses` conversations (`StoredResponse`/`ResponseChain`,
    `:1349-1520`): `id, chain_id, previous_response_id, model, status, body
    (full response JSON), input_items, messages (IR JSON), pending
    (awaiting-approval tool calls), input_tokens, output_tokens`.
    `list_response_chains` (`:1463-1507`) aggregates per chain (head id via
    correlated subquery, byte size, `awaiting_approval`).
  - `count_responses`, `gc_responses` (chain-aware retention,
    `:1536-1570`), `prune_logs` (age + row-count, `:1572-1594`) — background
    GC (`server.rs:151-184`) already calls these on an hourly tick; a
    `/api` "run GC now" action would call the same functions (see
    `web/responses.rs::gc_now`, not read in depth but wired at
    `web/mod.rs:191`).

## 5. `ops.rs` — the protocol-agnostic admin surface (THE reusable layer)

**This module is the intended foundation for `/api`.** It is already "one
function per admin action, `(&SharedState, typed patch) -> Result<Value,
String>`" (`ops.rs:1-24`) — precisely the shape a JSON handler wraps 1:1. It
backs the `lmgw__*` self-admin MCP tools (dispatch table in
`mcp/selfadmin.rs:1026-1113`) and nothing else consumes it yet.

Conventions (`ops.rs:12-21`, apply directly to `/api` DTO design):
- **Patches are sparse**: every field `Option`; `None` = "leave as is".
  `update` loads current row, overlays supplied fields, writes back.
- **Secrets never round-trip**: `REDACTED = "<set>"` (`ops.rs:39`) stands in
  for API keys / HF & update tokens / MCP `env`+`headers` values on read.
  Because `update` merges against the *stored* row, a read→write roundtrip of
  a redacted value can never persist the placeholder as the real secret.
- Every mutating patch struct derives `#[serde(default, deny_unknown_fields)]`
  — an unknown field is a hard error, not silently dropped (`ops.rs:503-508`
  et al.). A JSON `/api` layer should keep this: a typo'd field name must
  surface, not vanish.
- Errors are **plain `String`**, not `GatewayError` — `ops.rs` has no HTTP
  status opinion. A JSON handler must classify these itself (see §7 traps).

### Reads (all `Result<Value, String>`, all cheap/snapshot-backed)
| fn | params | notes |
|---|---|---|
| `status` (`:176-216`) | — | version, uptime, request stats, 3 container states, MCP statuses, object counts |
| `models` (`:225-340`) | `kind: all\|alias\|local\|embed\|audio`, `search` | statically-known names only; `expose_all` upstream catalogs reported as a note, not enumerated (needs live upstream HTTP — see `catalog::upstream_models`, `catalog.rs:48`) |
| `upstreams` (`:343-366`) | — | `api_key` reported as `Some(REDACTED)`/`None`, never the value |
| `mcp_servers` (`:370-406`) | — | config + live `status`/`tool_count` joined from `McpManager::status_views` |
| `logs` (`:410-450`) | `limit, errors_only, alias, before_id` | **no server-side ceiling on `limit`** beyond the caller's own value — matches the house "no hidden caps" rule |
| `settings` (`:453-495`) | — | full settings, tokens redacted |

### Mutations (action-verb patches: `create\|update\|delete\|enable\|disable`, some add `test`)
| fn | patch struct | actions | side effects |
|---|---|---|---|
| `upstream_set` (`:524-634`) | `UpstreamPatch` (`:509-522`) | CUD+enable/disable | `store::*` + `reload_snapshot()` + `state.catalog.invalidate(id)` |
| `model_set` (`:673-764`) | `AliasPatch` (`:643-652`) | CUD+enable/disable | `upstream` field accepts name **or** numeric id (`resolve_upstream`, `:655-671`) |
| `local_model_get` (`:1140-1242`) | `id` or `model_id` | read | returns full stored record **plus** `preset_section` (exact rendered INI) and static `problems[]` (path existence, spec/drafter consistency) — read-only but does GGUF header + running-build checks |
| `local_model_check` (`:1251-1319`) | `model_id?` | read | same static checks as `apply`, over all models or one; omits clean models unless one was named |
| `local_model_set` (`:1321-1481`) | `LocalModelPatch` (`:785-849`, full `LlamaParams` surface flattened + `clear: Option<String>` field-name list + `extra_args`) | CUD+enable/disable | validates paths stay under `models_dir` (no `..`), runs `config_warnings`/`model_warnings` (advisory, not fatal), does **not** apply to the container — caller must separately call `container action=apply` |
| `llama_flags` (`:1489-1517`) | `search?` | read | parses `llama-server --help` from the **running container** (`RouterManager::help`, blocking on container being up) |
| `hf_repo` (`:1568-1624`) | `repo, search?` | read | lists a HF repo's GGUFs classified `weights\|mmproj\|drafter`, live HTTP to HF |
| `hf_add` (`:1633-1739`) | `repo, file?, quant?, target, companions` | mutating (starts downloads) | resolves weights by name or quant label, auto-fetches same-dir `mmproj`/`drafter` companions when `companions=true`, delegates to `web::hf::queue_files` |
| `hf_downloads` (`:1742-1769`) | — | read | tracked rows + live byte progress merged from `HfManager::active()` |
| `hf_set` (`:1772-1796`) | `action: redownload\|delete\|check_updates`, `id?`, `target` | mutating | |
| `mcp_server_set` (`:1852-2021`) | `McpServerPatch` (`:1808-1828`) | CUD+enable/disable+**test** | list fields (`args/env/extra_run_args/headers`) are newline/`KEY=VALUE`/`Name: Value` delimited **strings**, not JSON arrays (small-model-callable schema constraint, `mcp/selfadmin.rs:11-17`); rejects `tool_prefix` = reserved `"lmgw"`; rejects a `url` that is this gateway's own `/mcp` (`reject_self_loop`, `:135-168`) |
| `container` (`:2028-2145`) | `target: chat\|embed\|audio`, `action: status\|start\|stop\|restart\|apply` | mutating (except status) | `apply` regenerates the config file from DB and hot-reloads (restarts container if running); for `chat` it also runs the same static pre-flight as `local_model_check`; for `embed`/`audio` it also re-syncs the managed upstream row (`ensure_embed_upstream`/`ensure_audio_upstream`) |
| `settings_set` (`:2176-2217`) | `SettingsPatch` (`:2167-2174`) | mutating | **deliberately narrow** — only `auth_enabled, retention_days, retention_max_rows, max_body_mb, sampling_alias, update_check_enabled`. `self_admin`, `bind_addr`, `hf_token`/`update_token`, and all container settings are excluded on purpose (`:2149-2160` — self-widening gate, restart-only field, secrets-in-log, and "dashboard owns container definitions" respectively). **A new `/api` settings endpoint needs its own broader write path if the Leptos UI is meant to replace the dashboard's Settings page** — this patch alone won't cover it. |

Helpers: `patch_from_args::<T>()` (`:2221-2226`, `Map<String,Value> ->
T`), `check_mode(mode, needs_write)` (`:2230-2242`, the `SelfAdmin` gate with
a self-documenting refusal string) — both directly reusable by an `/api`
dispatcher.

### Not in `ops.rs` — live in `crate::modelinfo` instead
Wired into the tool plane by `mcp/selfadmin.rs:1056-1069`, called with
`state: &SharedState` the same way:
- `modelinfo::gguf_files(state, search)` (`modelinfo.rs:151-211`) — every
  GGUF under the chat models dir + size + `used_by` model ids + filename-based
  `role_guess`.
- `modelinfo::model_inspect(state, gguf_path, probe: bool)` (`:379+`) — GGUF
  header metadata (architecture, context length, quant, chat template
  presence, MTP layers, mmproj/drafter role) plus, if `probe`, a ~10s runtime
  check against the *running* llama-server container for whether it knows the
  architecture at all (`probe_runtime`, `:227-...`; **read-only in name but
  spawns a real subprocess inside the container** — declines under
  `self_admin=read_only`, `:245-250`).
- `modelinfo::local_model_plan(state, gguf_path, probe: bool)` (`:560+`) —
  derives a ready-to-apply `LocalModelPatch`-shaped parameter set from GGUF
  metadata + sibling files; if a model row already uses that GGUF, also
  diffs current vs. planned and returns an update patch.
- `modelinfo::local_model_test(state, model_id)` (`:811-900+`) — **the only
  op that actually loads a model**: posts a 1-token completion to the live
  router with a 300s timeout, and on failure tails the container log for
  `error|failed|unknown model|exiting` lines and pattern-matches a hint (e.g.
  "unknown model architecture" → build-too-old). Always requires
  `self_admin=full` (mutating).

## 6. `router.rs` — llama-server preset + Podman lifecycle

- `RouterState` (`running|stopped|not_created|unknown`, `router.rs:404-420`),
  `RouterStatus { state, detail: Option<String> }` (`:422-435`, `Serialize`,
  `PartialEq` — this is the `Event::Router` payload and the container-status
  DTO shape for `/api`).
- `RouterManager` (`:438-450`) owns: a `CommandRunner` (trait, `:344-347`,
  real impl shells to `podman` via `tokio::process`, `:349-365` — swappable
  for tests), the container's `config_dir`, a cached `RouterStatus`, and a
  cached `llama-server --help` text (cleared on restart, `forget_help`,
  `:502-504`).
- `render_preset(&[LocalModel]) -> String` (`:240-260`) / `render_embed_preset`
  (`:265-294`) are pure — the exact INI a `/api` "preview preset" endpoint
  could expose without touching the container.
- `podman_run_args()` (`:373-400`) — pure, unit-tested; shows exactly what
  gets mounted (`config_dir:/config:ro`, `models_dir:/models:ro`) and how
  `--models-preset`/`--models-max` are passed. No `-m` ⇒ router mode.
- Lifecycle: `start/stop/restart` (`:564-599`, shell to `podman run|stop|
  restart`), `apply_config`/`apply_rendered` (`:604-616`, `:631-638`, write
  file then restart-if-running), `refresh_status` (`:641-665`, `podman
  inspect --format '{{json .State}}'`, parsed by `parse_inspect_state`
  `:670-699` — **shared with `AudioManager`**, same JSON shape).
- **All of `start/stop/restart/apply_*` shell out and block on the podman CLI
  round-trip** — no timeout wrapper visible in this file, so a `/api` handler
  calling these directly should not assume sub-second latency, especially
  `restart` (container health-check dependent).
- `help()` (`:467-498`) runs `podman exec <container> llama-server --help`
  — **requires the chat container to already be running**; fails with a
  named error otherwise. `llama_flags`/`config_warnings` depend on this and
  degrade gracefully (empty warnings) when it fails.
- Embed router settings default to `models_max: 0` (never evict) vs. chat's
  `models_max: 1` (single-GPU swap) — a real behavioral asymmetry a UI should
  probably surface, not just a config difference.

## 7. `server.rs` — axum assembly, auth, body limits

- `build_router()` (`server.rs:18-70`):
  - `json_api` (`:22-38`) = the OpenAI/Anthropic `/v1/*` JSON routes
    (`chat/completions, responses, responses/{id}, completions, embeddings,
    messages, count_tokens, models`), wrapped in `body_limit_mw` (per-request,
    reads `max_body_mb` from the **current snapshot**, so editing the setting
    takes effect on the next request with no restart) and axum's own
    `DefaultBodyLimit::disable()` (so `body_limit_mw` is the only ceiling).
  - `media_api` (`:43-52`) = `/v1/audio/*` + `/v1/tasks/*` — **body limit
    unconditionally disabled**, no `max_body_mb` bound at all (audio files).
  - `api = json_api.merge(media_api).layer(auth_mw)` (`:54-56`) — gateway-key
    auth (`Authorization: Bearer` or `x-api-key`), only when
    `settings.auth_enabled`.
  - `mcp` (northbound `/mcp`, `/mcp/admin`) also behind `auth_mw` (`:60-61`).
  - **`crate::web::routes()` (the existing dashboard) is merged with *no*
    `auth_mw` layer at all** (`:66`) — the whole HTML UI + its
    JSON-returning-HTML-partial endpoints are unauthenticated regardless of
    `auth_enabled`, relying entirely on `bind_addr` being loopback. **This is
    the load-bearing trap for `/api`**: if the new plane is mounted the same
    way (merged outside `api`), it inherits zero auth by default; if it's
    nested under `/v1` or given its own `auth_mw` layer, that's a deliberate
    departure from how the dashboard behaves today and should be a conscious
    decision, not an accident.
  - `CorsLayer::permissive()` applies to everything (`:68`).
- `auth_mw` (`:235-263`) — only gates on `settings.auth_enabled`; when off,
  every request gets a default `RequestCtx` with no `client_key`. Protocol
  shape of the 401 body depends on the `anthropic-version` header.
- `body_limit_mw` (`:279-304`) / `body_limit_response` (`:308-316`) /
  `body_error` (`:323-330`) — the `max_body_mb=0` ⇒ unlimited convention; a
  `/api` layer wanting the same visible-cap behavior should reuse this
  pattern rather than reinvent axum's silent default.
- `spawn_background_tasks()` (`:90-229`) — five `tokio::spawn` loops started
  once at boot: (1) resume interrupted HF downloads, (2) reconcile MCP
  connections + broadcast initial status, (3) auto-start containers per
  `auto_start` settings, (4) hourly log/response GC, (5) 5s poll: refresh all
  3 container statuses (only chat's changes are broadcast as `Event::Router`;
  embed/audio are cache-only refreshes with **no live event** — a `/api` SSE
  stream wanting live embed/audio badges must **poll** `container
  action=status`, not subscribe), MCP idle-reap + status poll/broadcast.
- `exposed_model_names()` / `list_models()` — the full `/v1/models`
  aggregation logic (aliases + public locals + live passthrough catalogs,
  with `context_length` and zero-pricing for local/self-hosted upstreams);
  `exposed_model_names` is now a thin re-export of
  `capabilities::exposed::exposed_entries`, which also derives each entry's
  `capabilities`/`max_output_tokens`/`notes` via the `capabilities::{for_local_row,
  for_aux, for_audio, for_catalog}` builders and the `capabilities::notes`
  writer (model-capabilities design). Worth reusing verbatim for an
  `/api/models` read endpoint rather than re-deriving it, since it needs live
  upstream HTTP calls (`catalog::upstream_models`) that `ops::models`
  deliberately skips.

## 8. `web/hf.rs` (+ `hf.rs`) — HF download registry

- Durable state lives in SQLite (`store::HfModelRow`, §4); **live byte
  progress lives only in memory** (`hf::HfManager`, `hf.rs:266-309`) — lost on
  restart, which is why `spawn_background_tasks` re-queues `queued|
  downloading` rows on boot (`server.rs:92-103`).
- `DownloadProgress { id, repo, file, received, total: Option<u64> }`
  (`hf.rs:255-262`, `Serialize`) — the shape for a live progress SSE/poll DTO;
  `ops::hf_downloads` already merges it with the DB row (`ops.rs:1742-1769`).
- Status state machine (string, not an enum in Rust — lives in
  `HfModelRow.status`): `queued → downloading → done | failed |
  update_available`. The dashboard additionally synthesizes `"interrupted"`
  client-side when a DB row says `queued`/`downloading` but has no live
  `HfManager` entry (`web/hf.rs:87-91`) — **a `/api` JSON layer should
  replicate this derivation**, it is not stored anywhere.
- Core reusable actions, target-parameterized (`chat|embed|audio`), already
  factored out of the HTML handlers for exactly this kind of reuse:
  `queue_download`/`queue_files` (`web/hf.rs:221-269`), `redownload`
  (`:273-286`, also serves retry/resume), `delete_tracked` (`:289-306`,
  refuses while a download is live), `check_updates` (`:310-326`, ETag
  compare via HEAD, sets `update_available` on drift).
- Update-check mechanism (`hf.rs:229-249`, `check_update` `:390-404`): HEAD
  the HF resolve URL, compare `x-linked-etag`/`etag` (normalized,
  `normalize_etag` `:212-217`) against the etag recorded at download time
  (`mark_hf_done`, `store.rs`). No remote etag ⇒ "can't tell", not "changed".
- Split-GGUF handling (`expand_parts`/`split_part_name`, `hf.rs:79-97`) —
  selecting any part downloads all sibling parts; `hf_repo`/`hf_table`
  collapse them into one logical entry (only part 1 is directly loadable,
  `primary_part` field, `web/hf.rs:41-43`, `109-111`).

## 9. `gguf.rs`, `llama_caps.rs`, `modelinfo.rs`

- **`gguf.rs`** — a dependency-free GGUF header parser (`gguf.rs:1-26`).
  Reads only the header (megabytes, never the multi-GB tensor data), treats
  every declared length as untrusted (bounded by real remaining file size, no
  guessed constants). Central output type `ModelSummary` (`:604+`):
  `architecture, general_name, general_type, size_label, quant,
  file_type_id, context_length, block_count, embedding_length, head_count,
  head_count_kv, key_length, value_length, sliding_window(_pattern[_is_array]),
  rope_freq_base, has_chat_template (+ full template text), has_mtp_layers,
  is_mmproj, …`. This is what answers "what is this GGUF" for
  `model_inspect`/`local_model_plan`.
- **`llama_caps.rs`** — parses `llama-server --help` text (from the *running*
  container image, never hardcoded) into `LlamaCaps` (`:140-160+`): `flags`
  (long, no dashes), `short_flags`, `enums` (documented fixed value lists),
  `soft_enums` (inferred, not exhaustive), `list_enums` (comma-subset flags),
  `value_flags`, plus deprecated/removed sets. Exposes
  `validate_args`/`validate_pair`/`is_managed`/`is_deprecated`/`supports` used
  by `ops::config_warnings` (`ops.rs:985-1071`) and `ops::llama_flags`. Since
  the vocabulary is a live property of the container image, any `/api`
  endpoint surfacing "allowed values" for a param must go through this (or
  cache its result) rather than hardcode an enum list.
- **`modelinfo.rs`** — the semantic layer over the two above plus the running
  router (§5's "not in ops.rs" table). Owns path resolution against
  `models_dir` with a hard `..`/`.` rejection (`modelinfo.rs:34-56`, models
  dir is a strict boundary even for `self_admin=full` callers), file-role
  classification (`role_of`, `:79-96`; weights vs `mmproj` vs `drafter`, by
  GGUF metadata — authoritative over the filename-based
  `ops::classify_repo_file` heuristic used for HF repo browsing), and the
  drafter-architecture → `--spec-type` mapping (`spec_type_for`, `:105-116`)
  that is "the single most valuable thing in this module" per its own doc
  comment (a published model card's suggested flag value is frequently wrong;
  this derives the real one from the GGUF header).

## 10. `audio.rs` — third managed container (context, not directly requested)

- `AudioManager` (`audio.rs:112-235`) is a structural mirror of
  `RouterManager` (`router.rs`) reusing the same `RouterStatus`/`RouterState`
  types and the same `podman inspect` → `parse_inspect_state` parser — but
  renders a JSON `server.json` (`render_server_config`, `:33`) instead of an
  INI preset, since audio.cpp's config format differs. `CatalogSnapshot`
  (`:420+`) is a separately-fetched (GitHub-hosted) catalog of installable
  model specs, cached in `AppState.audio_catalog` and the KV store — distinct
  from the HF download registry.

## 11. Related types referenced above but not deep-dived (context only)
- `ir::Params` (`ir.rs:204-221`, `Serialize+Deserialize`): `temperature,
  top_p, top_k, max_tokens, presence_penalty, frequency_penalty, seed,
  stop: Vec<String>` — this is `ModelAlias.param_overrides`'s type; a
  `with_defaults()` merge favors explicit client values.
- `error::GatewayError` (`error.rs:6-40`) — the `/v1`-path error type, with
  `http_status()` (`:43-64`) and `kind()` (machine-readable, logged in
  `request_logs.error_kind`) mappings. **`ops.rs` does not use this** — it
  returns bare `String`s, so any HTTP status mapping for `/api` errors has to
  be invented fresh (see §12 traps).
- `mcp::McpStatusView` (`mcp/mod.rs:306-313`, `Serialize`): `id, name, status:
  "connecting"|"ready"|"stopped"|"error", tool_count, detail`. This is the
  `Event::Mcp` payload shape and `ops::mcp_servers`'s live-status join source.
- `catalog::Pricing`/`upstream_models()` (`catalog.rs:29,48`) — per-token
  pricing + live upstream model listing, cached in `AppState.catalog:
  CatalogCache`; used by `server.rs::exposed_model_names` for `/v1/models`.
  `catalog::ModelInfo` has grown (model-capabilities design §4): reasoning,
  modalities, tool/structured-output support, `max_output_tokens`, `created`,
  split per protocol shape (OpenAI/Kilo, Gemini, Anthropic) — read by
  `capabilities::for_catalog`, not by this module.

---

## 12. API design implications

**Reuse `ops.rs` as the backend for nearly everything.** It is already
protocol-agnostic, already the second consumer alongside the HTML dashboard,
and already encodes every validation rule (self-loop MCP guard, reserved tool
prefix, models-dir path boundary, secret redaction). A JSON `/api` handler
should in the common case be: deserialize body → `ops::patch_from_args` (or a
typed `serde` struct) → call the matching `ops::*` fn → map `Result<Value,
String>` to an HTTP response. Do not re-implement validation in the new layer.

Per-domain mapping:

| Domain | Call | Notes |
|---|---|---|
| Status/dashboard tiles | `ops::status` | one call for everything |
| Models list (aliases/local/embed/audio) | `ops::models` | static only |
| Models list incl. passthrough catalogs, pricing, context length | `server::exposed_model_names` | needs live upstream HTTP; not in `ops.rs` |
| Upstreams CRUD | `ops::upstream_set` / `ops::upstreams` | |
| Model aliases CRUD | `ops::model_set` | `upstream` field takes name or id |
| Local (chat) models CRUD + diagnostics | `ops::local_model_set` / `_get` / `_check` | `_get`/`_check` do GGUF+build validation even though read-only; both take a `target` and cover aux rows too (`_get` reports `class`) |
| Aux (embedding / rerank) models CRUD | `ops::aux_model_set` (`lmgw__aux_model_set`) | same conventions as chat; `extra_args`/`extra_run_args` accept newline text (tool plane) or a token array (dashboard, `args` alias) via `ops::ArgList`; path boundary is the aux models dir |
| GGUF discovery / metadata / planning / load test | `modelinfo::{gguf_files,model_inspect,local_model_plan,local_model_test}` | all take a `target` (`chat` default, `aux`, and `audio` for listing); `model_inspect` reports `serve_as`/`aux_kind` from the header's `pooling_type`/classifier head; `local_model_plan` returns an aux plan (`class: aux`) for encoders wherever found; `local_model_test` probes by class/kind (generate / embed / rerank). `model_inspect`/`local_model_plan` can spawn a subprocess (`probe=true`); `local_model_test` always mutates VRAM state, gate accordingly — it, `lmgw__container start`/`restart` and warm starts are all refused while `settings.hold.active` (GPU hold); `hold_set` is the only writer of that flag |
| llama-server flag vocabulary | `ops::llama_flags` | requires chat container running |
| HF browse/download | `ops::hf_repo` / `ops::hf_add` / `ops::hf_downloads` / `ops::hf_set` | or the lower-level `web::hf::{queue_files,redownload,delete_tracked,check_updates}` if finer control is needed |
| Container lifecycle (chat/embed/audio) | `ops::container` | `apply` is the "make config live" step; distinct from `local_model_test` (the "prove it loads" step) |
| MCP servers CRUD + test | `ops::mcp_server_set` | list-valued fields are delimited strings, not JSON arrays — a Leptos form can send real arrays and join them before calling, or `ops.rs` could grow a JSON-array variant later |
| Settings | `ops::settings` / `ops::settings_set` | **narrow** — self_admin, bind_addr, tokens, and container definitions are NOT covered; a full Settings page needs new `store::save_settings` calls or an expanded patch |
| Logs (unified request feed) | `ops::logs` (snapshot) + `store::query_logs` (direct, more filter options: `upstream_name`) | `ops::logs` doesn't expose `upstream_name` filter — either extend it or call `store` directly |
| Live event stream | `state.telemetry.subscribe()` → `Event` enum | mirror `web/mod.rs:581-637`; note embed/audio container status changes are **not** broadcast (5s poll loop only updates their cache) — SSE consumers need a polling fallback for those two badges |
| Chat threads (existing Chat tab) | `store::{list_chat_threads,get_chat_thread,append_chat_message,...}` | no `ops.rs` wrapper exists yet — direct `store` calls, or add one |
| Stored `/v1/responses` conversations | `store::{list_response_chains,list_chain_responses,delete_response_chain,gc_responses}` | no `ops.rs` wrapper yet either |

Traps to design around:

1. **No auth on the existing web layer** (`server.rs:66`) — decide explicitly
   whether `/api` sits inside `auth_mw` (like `/v1`/`/mcp`) or outside (like
   today's dashboard). Mounting it wrong either exposes config mutation with
   no auth, or breaks the loopback-dashboard assumption other code relies on.
2. **`ops.rs` returns `String` errors, not `GatewayError`** — there is no
   existing HTTP-status mapping for admin-plane failures (missing id, bad
   enum, self-loop, path-outside-models-dir, etc. are all just `Err(String)`).
   The `/api` layer must invent its own error → status code convention (400
   for validation, 404 for missing id, 409 for conflicts, etc.) since none is
   inherited "for free" the way `GatewayError::http_status()` gives `/v1`.
3. **`reload_snapshot()` after every write** — forgetting it means the DB is
   updated but `snapshot()` (and thus every subsequent read, and the live
   proxy) still serves stale config until the next unrelated write triggers a
   reload elsewhere. Every `ops::*_set` already does this; a bespoke `/api`
   write path must too.
4. **Blocking/slow calls**: `RouterManager::help()` (needs container up),
   `container action=apply/start/stop/restart` (shells to `podman`, no
   timeout wrapper visible), `modelinfo::model_inspect`/`local_model_plan`
   with `probe=true` (up to ~10s), and especially `local_model_test` (up to
   300s — a real cold model load). None of these should run inline in a
   request handler expected to answer quickly; consider async job semantics
   (kick off + poll, or SSE progress) for `local_model_test` and `apply`
   at minimum, matching how HF downloads are already handled (fire-and-poll
   via `hf_downloads`, not a blocking POST).
5. **Path-prefix conventions**: GGUF-ish fields (`gguf_path`, `mmproj_path`,
   `draft_gguf_path`, `chat_template_file`) are stored **relative to the
   models dir**; the container/render layer adds the `/models/` prefix. Any
   `/api` DTO exchanging these paths with a browser should stay relative and
   let the existing `modelinfo::resolve`/`ops` boundary checks (`..`
   rejection) do the validation — don't have the frontend construct
   in-container absolute paths.
6. **Secrets discipline**: reuse `ops::REDACTED` (`"<set>"`) sentinel and the
   "only overwrite on a non-empty supplied value" pattern (`opt()` helper,
   `ops.rs:93-98`) for any new secret-bearing field exposed through `/api` —
   do not invent a second convention.
7. **`self_admin` also gates Admin Chat and `/mcp/admin`**, not just the
   tool-call plane — it's a single crate-wide capability switch
   (`config.rs:674-726`). If `/api` mutation should be independently gated
   from the MCP tool plane (e.g. the Leptos UI always allowed to mutate,
   regardless of whether MCP self-admin is `read_only`), that needs a new,
   separate gate — reusing `self_admin` for both would tie the UI's write
   access to an unrelated agent-facing setting.
