# LLM API Gateway (lmgw) — Design

**Date:** 2026-06-09
**Status:** Draft for review

## 1. Summary

`lmgw` is a self-hosted LLM API gateway for personal/homelab use. It exposes a
stable, OpenAI- **and** Anthropic-compatible API to local clients, routes each
request — by a configurable model *alias* — to an upstream provider
(OpenAI-compatible, Anthropic, or Gemini), translating protocols as needed. It
manages local llama.cpp models through **llama-server router mode** (generating
its model preset and controlling its Podman container) and provides a web UI for configuration and
live observability.

It ships as a **Tauri v2 desktop app** that lives in the system tray. The tray
controls a long-running Axum HTTP server embedded in the same process; the
Tauri window simply displays the server's web UI. The same UI and API are
reachable from any browser/app on the trusted LAN.

### Goals
- One stable client API (OpenAI + Anthropic shapes) decoupled from where a model
  actually runs.
- Configure upstreams and map friendly local aliases → `{upstream, model, params}`.
- Auto-switch local llama.cpp models via llama-server router mode, configured
  and lifecycle-managed from the app.
- Web UI for configuration + live request observability.
- Tray-resident desktop app for quick local control.

### Non-goals (v1)
- Multi-user accounts / RBAC / team usage accounting.
- Cost dashboards, time-series charts, long-term analytics (logs + live metrics only).
- Load balancing / automatic upstream failover / retries across upstreams.
- Encryption of secrets at rest (plaintext + file perms, see §13).
- Headless/server deployment as a shipped artifact (architecture supports it; not built in v1).

## 2. Locked decisions

| Decision | Choice |
|---|---|
| Local model switching | Delegate to **llama-server router mode**; gateway **generates & reloads its model preset** |
| llama-server runtime | Gateway **manages its Podman container** (start/stop/restart/reload) |
| Deployment scope | **Personal homelab** — single user, trusted LAN, minimal auth |
| Backend language | **Rust** (Axum + Tokio) |
| Frontend | **HTMX + Askama** server-rendered, assets embedded; SSE for live data — *superseded 2026-08-29 by a Leptos CSR SPA, see [docs/design/ui-rebuild/plan.md](ui-rebuild/plan.md)* |
| Desktop shell | **Tauri v2** app with **system tray**; window loads the local web UI |
| Client (ingress) API | **OpenAI** (`/v1/chat/completions`, …) **+ Anthropic** (`/v1/messages`) |
| Upstream (egress) protocols | **OpenAI-compatible + Anthropic + Gemini** (protocol translation) |
| Translation architecture | **Canonical IR + adapters** (N+M, not N×M) |
| Persistence | **SQLite** (config + request logs) |
| Secret storage | **Plaintext** in SQLite, protected by `0600` file permissions |
| Observability | **Request logs + live metrics** (no charts in v1) |

## 3. Architecture overview

One Rust binary (the Tauri app). A single Tokio runtime hosts:

- **Axum server task** — the gateway API + web UI + SSE, bound to a configurable
  address (default `127.0.0.1:8787`; may bind `0.0.0.0` for LAN access).
- **Tauri main thread** — system tray icon + menu, and a window whose webview
  loads `http://127.0.0.1:<port>/` (the Axum-served dashboard — HTMX when this
  was written, the Leptos SPA since the 2026-08-29 cutover; same URL either way).

Both share one application state (`Arc<AppState>`): config snapshot, SQLite pool,
telemetry bus, and the Podman/llama-server manager. Tray menu actions call the same
core functions the HTTP handlers do — no internal HTTP hop.

```
                       ┌─────────────────────────── lmgw (Tauri app, one process) ──────────────────────────┐
                       │                                                                                      │
  LAN browser / app ──HTTP──▶  Axum server  ──┐                          Tauri tray + window                 │
  (OpenAI / Anthropic SDK)     /v1/* , /ui ,   │                          - status, Open Dashboard,           │
                               /events (SSE)   │   Arc<AppState>          - Start/Stop llama-server, Quit     │
                                               ├──▶ store (SQLite)  ◀──────┘  (calls core fns directly)        │
   Tauri webview ──loads──▶ http://127.0.0.1:<port>/ (same HTMX UI)                                            │
                                               ├──▶ telemetry bus (broadcast → SSE)                            │
                                               ├──▶ router (alias → upstream)                                  │
                                               ├──▶ adapters (IR ↔ openai/anthropic/gemini)                    │
                                               └──▶ podman mgr ──▶ llama-server router ──▶ model instance procs │
                       └──────────────────────────────────────────────────────────────────────────────────────┘
```

> Route names in the diagram are as-of-2026-06: the dashboard plane is now the
> SPA at `/` plus `/api/*` (JSON + SSE), and `/ui` — the SPA's parallel mount
> during the rebuild — permanently redirects to `/`.


### Crate layout
- **`lmgw-core`** (lib): ingress parsing, IR, router, adapters, proxy engine,
  store, telemetry, llama-server + Podman manager, web (Askama/HTMX) handlers, the
  Axum `Router` factory. No Tauri dependency.
- **`lmgw`** (`src-tauri`, bin): Tauri v2 shell — tray, window, lifecycle,
  single-instance; spawns the `lmgw-core` Axum server on startup and wires tray
  actions to core functions.

This split keeps the gateway testable headless and makes a future headless bin
(`lmgw-headless`) a thin wrapper.

## 4. Request data flow (chat completion)

```
client → POST /v1/chat/completions (OpenAI)  ─┐   or  POST /v1/messages (Anthropic)
   ingress parser (openai|anthropic) → IR     │
   router: alias "my-claude" → {upstream: anthropic, model: "claude-…", overrides}
   egress(anthropic): IR → /v1/messages request (reqwest, streaming)
   upstream SSE (message_start, content_block_delta, …) → IR deltas
   ingress serializer (matches the client's protocol): IR deltas → client SSE
   telemetry: record {alias, upstream, tokens, ttfb, total, status} → broadcast + SQLite
```

**Uniform local/remote routing:** the llama-server router is just an
`openai`-protocol upstream of kind `llama_server`. A "local model" is an alias
pointing at it, so local and remote requests share one proxy path. Local-model
entries *also* feed the router's preset generation (§8).

## 5. Canonical IR (internal representation)

Provider-neutral types, rich enough for both ingress shapes and all three
egress shapes. Built incrementally (text → tools → multimodal).

- `ChatRequest { model_alias, messages, params, tools, tool_choice, stream, stop, … }`
- `Message { role: System|User|Assistant|Tool, content: Vec<ContentPart> }`
- `ContentPart::{ Text(String), Image{ mime, data|url }, ToolUse{ id, name, args }, ToolResult{ id, content } }`
- `Params { temperature, top_p, top_k, max_tokens, presence/frequency_penalty, seed, … }` (optional fields; unknown/unsupported params dropped per-egress with a logged note)
- `StreamDelta::{ TextDelta, ToolCallDelta, Usage, Stop{ reason }, Error }`
- `Usage { prompt_tokens, completion_tokens }`
- `Completion { content, finish_reason, usage }`

Mapping notes: OpenAI `tools`/`tool_calls` ↔ Anthropic `tool_use`/`tool_result`
content blocks ↔ Gemini `functionDeclarations`/`functionCall`/`functionResponse`;
system prompt is a top-level field for Anthropic/Gemini but a message for OpenAI;
finish/stop reasons normalized.

## 6. Ingress (client-facing API)

Two protocol surfaces, both → IR:
- **OpenAI:** `POST /v1/chat/completions`, `POST /v1/completions`,
  `POST /v1/embeddings`, `GET /v1/models` (aggregates enabled aliases).
- **Anthropic:** `POST /v1/messages`, `GET /v1/models` (Anthropic-shaped list).

Each ingress has a parser (request → IR) and a serializer (IR / IR-deltas →
that protocol's response + SSE framing). Streaming and non-streaming supported.
Embeddings: OpenAI-shape in/out; routed to OpenAI/Gemini embedding endpoints
(Anthropic has none — alias to an embeddings-capable upstream).

## 7. Egress adapters

One isolated module per upstream protocol, each implementing a trait:

```
trait Egress {
    fn build_request(ir: &ChatRequest, model: &str, overrides) -> http::Request;
    fn parse_response(resp) -> Completion;            // non-stream
    fn parse_chunk(bytes) -> Vec<StreamDelta>;        // SSE → IR deltas
    fn map_error(status, body) -> GatewayError;
}
```

- **openai** — near pass-through; also serves llama-server (kind `llama_server`).
- **anthropic** — `/v1/messages`, `x-api-key` + `anthropic-version` headers,
  content-block SSE events.
- **gemini** — `:generateContent` / `:streamGenerateContent`, `contents[]`/`parts`
  shape, `role: "model"`, key via query/header.

Upstream calls use `reqwest` streaming + `reqwest-eventsource`. Per-upstream
timeout; client cancellation propagates (drop → abort upstream request).

## 8. llama-server router mode + Podman management

The app owns the router's **model preset and container lifecycle**. llama-server
launched without `-m` runs in *router mode*: a parent process that spawns one
llama-server instance per requested model, loading/unloading on demand
(`--models-max` bounds concurrently loaded models; `sleep-idle-seconds` per
model frees memory after idle).

- **Preset generation:** local-model entries render `models-preset.ini`
  (one `[model-id]` section per model: `model = /models/<gguf>` + llama-server
  flags as INI keys + `sleep-idle-seconds`) to a known config path.
- **Container lifecycle:** the Podman manager shells out to the `podman` CLI via
  `tokio::process` (predictable, no API-version coupling) to run/stop/restart the
  llama-server container (`ghcr.io/ggml-org/llama.cpp:server-cuda`, entrypoint
  `/app/llama-server`, `--models-preset /config/models-preset.ini`), mounting the
  generated preset and the models/GGUF dir, with GPU access
  (`--device nvidia.com/gpu=all` / CDI) and the API port.
- **Reload on change:** after writing the preset, restart the container
  (llama-server also offers `GET /models?reload=1` as a lighter refresh;
  restart is the always-correct fallback).
- **Health/status:** manager tracks container state (running/stopped/health) and
  surfaces it to the UI/tray.
- **Speculative decoding / MTP:** a local model can reference an external draft
  GGUF (e.g. MTP heads) via structured params; the preset renders
  `model-draft = /models/<path>` plus `spec-type` (`draft-mtp`, …),
  `spec-draft-n-max`/`-n-min` and `spec-draft-ngl`. The draft file lives in the
  same mounted models dir as the main model.
- **HF model manager:** browse a Hugging Face repo's GGUF files (tree API),
  download into `<models dir>/<owner>/<repo>/…` (streamed to `.part`, atomic
  rename; split `-NNNNN-of-MMMMM.gguf` files fetch all parts), tracked in
  `hf_models` together with the resolve-URL ETag. "Check for updates" HEADs the
  resolve URL and flags rows whose ETag changed; re-download updates in place.
  Interrupted downloads restart on app launch. `HF_ENDPOINT` overrides the hub
  base URL; an optional token (Settings) unlocks gated/private repos.

The router is registered as an `openai`-protocol upstream of kind `llama_server`
pointing at its local port, so routing to local models needs no special case —
the router itself dispatches on the request's `model` field.

## 9. Data model (SQLite)

```
upstreams(
  id, name, protocol TEXT[openai|anthropic|gemini], kind TEXT[generic|llama_server],
  base_url, api_key, extra_headers JSON, timeout_ms, enabled, created_at, updated_at)

models(  -- aliases
  id, alias UNIQUE, upstream_id FK, upstream_model_id,
  param_overrides JSON, enabled, created_at, updated_at)

local_models(  -- belong to the llama_server upstream; feed preset generation
  id, model_id UNIQUE, params JSON, args JSON, idle_seconds, gguf_path,
  enabled, created_at, updated_at)

hf_models(  -- Hugging Face downloads tracked in the models dir
  id, repo, file, dest_path, etag, size_bytes,
  status TEXT[queued|downloading|done|failed|update_available],
  error, downloaded_at, created_at, UNIQUE(repo, file))

api_keys(  -- gateway ingress keys (auth optional, see §13)
  id, name, key_hash, enabled, created_at)

request_logs(
  id, ts, client_key, ingress_proto, requested_alias, upstream_id, upstream_model,
  egress_proto, status, ttfb_ms, total_ms, prompt_tokens, completion_tokens,
  streamed, error_kind, error_msg)

settings(key PRIMARY KEY, value)
```

Access via `sqlx` (SQLite, async, compile-checked queries). DB file `0600`.
Config is cached in-memory (`Arc<Snapshot>`), atomically swapped on edit so the
hot path does no DB reads.

## 10. Observability (logs + live metrics)

Every request produces a `request_logs` row and a push onto a
`tokio::broadcast` channel. The dashboard subscribes via SSE (`GET /events`,
HTMX `sse-swap`) to render a live request feed and live counters (req/s, error
rate, tokens, active-by-model/upstream). A filterable log table backs it with
history; clicking a row shows request detail. Retention configurable (keep last
N rows / N days); a periodic task prunes. Secrets are never logged.

## 11. Web UI (HTMX + Askama)

> **Superseded (2026-08-29).** The htmx/Askama UI was replaced by a Leptos CSR
> single-page app (`crates/lmgw-ui`) over a JSON + SSE `/api` plane, and deleted
> at the P8 cutover. What is still accurate below is the *feature* inventory —
> the same surfaces exist, regrouped into a sidebar. See
> [docs/design/ui-rebuild/plan.md](ui-rebuild/plan.md) for the target
> architecture and [parity.md](ui-rebuild/parity.md) for the as-is
> inventory this replaced.

Server-rendered pages, embedded via `rust-embed`, progressively enhanced with
HTMX; SSE for live regions. No JS build step.

- **Dashboard** — live request feed + counters + recent errors; llama-server status.
- **Upstreams** — CRUD + "test connection".
- **Models / aliases** — CRUD; map alias → upstream + model; param defaults.
- **Local models** — CRUD; generate/reload the router preset; container controls + status.
- **HF models** — browse a repo's GGUFs, download with live progress (polled
  HTMX partial), update checks, one-click "add as local model".
- **Logs** — filterable table; per-request drill-down.
- **Settings** — gateway keys + auth toggle, bind address, retention, paths,
  HF access token.

## 12. Tauri shell & tray

- **Tray menu:** status line (running / port / llama-server state) · Open Dashboard
  (show/focus window) · Open in Browser · Start/Stop llama-server · Restart gateway
  · Quit.
- **Window:** single webview loading `http://127.0.0.1:<port>/`. Closing the
  window hides to tray (server keeps running); Quit exits.
- **Single instance:** `tauri-plugin-single-instance` so a second launch focuses
  the existing tray app.
- **Startup order:** spawn the Axum server in Tauri `setup()`, then create the
  window pointing at the local URL (webview retries until the server is ready).
- **Optional (flagged, not committed for v1):** autostart on login
  (`tauri-plugin-autostart`); native notifications on upstream errors.

### Wayland / NVIDIA (required for this machine)
> **Superseded 2026-09-28:** lmgw now sets `__NV_DISABLE_EXPLICIT_SYNC=1` instead
> (NVIDIA only, and only when no WebKit render variable is already set). It
> avoids the Error 71 crash *and* keeps the zero-copy hardware renderer;
> `WEBKIT_DISABLE_DMABUF_RENDERER=1` means software compositing, measured
> 2026-09-27 at 21 fps vs 60 (1600×1000) and 4 vs 60 (5000×1400) on the RTX 4090.
> An explicit `WEBKIT_DISABLE_DMABUF_RENDERER=1 lmgw` stays the escape hatch.
> See `render_workaround` in `src-tauri/src/main.rs`. The text below is the
> original design.

WebKitGTK (Tauri/`wry`) crashes on NVIDIA+Wayland with
`Gdk-Message: Error 71 (Protocol error)` unless `WEBKIT_DISABLE_DMABUF_RENDERER=1`
is set. Mitigation:
- Set it **early in `main()`** (`std::env::set_var(...)`) before the webview/GTK
  initializes, and in the dev command:
  `WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo tauri dev`.
- Also set it in the packaged launcher (`.desktop`/wrapper) as a belt-and-suspenders.
- Do **not** use `GDK_BACKEND=x11` (mis-renders on NVIDIA via XWayland).
- *Verify the in-`main` `set_var` reliably prevents the crash on this box during
  the first Tauri spike; if not, fall back to the launcher env.*

Fedora prerequisites: `webkit2gtk4.1-devel`, `libsoup3-devel`, GTK/`ayatana`
appindicator dev packages for the tray.

## 13. Auth & secrets

- **Auth off by default** (trusted LAN). When enabled, ingress requires a gateway
  API key (`Authorization: Bearer` / `x-api-key`); keys stored hashed in
  `api_keys`. Binding `0.0.0.0` should prompt enabling auth in the UI.
- **Upstream secrets:** plaintext in SQLite; DB file `0600`; never logged or
  rendered in full in the UI (masked). Optional at-rest encryption is a v2 item.

## 14. Error handling

Single `GatewayError` type → normalized error responses **in the client's
protocol** (OpenAI- or Anthropic-shaped) with sensible HTTP status:
unknown alias → 404, auth → 401, bad request → 400, upstream/transport → 502,
timeout → 504, upstream-returned errors mapped through with provider detail
preserved in the log. Mid-stream upstream failure terminates the client stream
cleanly and logs the partial (ttfb captured, completion marked errored). UI
config/validation errors render inline.

## 15. Testing strategy (TDD)

- **Adapter golden tests:** OpenAI↔IR↔Anthropic↔Gemini for non-stream and
  streamed chunk sequences, from recorded fixtures (per provider).
- **Ingress round-trips:** OpenAI-in and Anthropic-in → IR → re-serialized out.
- **Router tests:** alias resolution + param-override merge precedence.
- **llama-server preset:** snapshot tests of generated INI.
- **Podman manager:** command-construction tests (mock the process runner); a
  gated integration test that actually starts/stops a container.
- **End-to-end proxy:** against a `wiremock` upstream, asserting translated
  output (both directions) + a correct `request_logs` row + a live-feed event.
- Build order to de-risk translation: **text → tool calling → multimodal/images.**

## 16. Crates & tooling

`tauri` v2 (tray-icon, single-instance; optional autostart/notification) ·
`axum` · `tokio` · `tower-http` (trace/cors/timeout/compression) ·
`reqwest` (stream) + `reqwest-eventsource` · `serde`/`serde_json` ·
`sqlx` (SQLite) · `rust-embed` (was: `askama`, removed at the P8 UI cutover) ·
`tracing` + `tracing-subscriber` · `secrecy` (in-memory key handling) ·
`thiserror`/`anyhow`. Build/run via `cargo tauri` (no Node toolchain required;
the webview loads the Axum URL rather than a JS bundle).

## 17. Build milestones (for the implementation plan)

1. **Skeleton:** workspace (`lmgw-core` lib + `lmgw` Tauri bin); Axum server task
   started from Tauri setup; tray with Quit + Open Dashboard; window → localhost;
   Wayland env workaround verified. SQLite + migrations.
2. **Config plane:** upstreams + aliases CRUD (UI + store + in-memory snapshot);
   `GET /v1/models`.
3. **Proxy core (text):** IR + OpenAI ingress + OpenAI egress, streaming pass-through;
   request logging + live feed; Dashboard + Logs pages.
4. **Multi-egress:** Anthropic + Gemini egress adapters (text → tools → images).
5. **Anthropic ingress:** `/v1/messages` parser + serializer (stream + non-stream).
6. **llama-server router + Podman:** local-model CRUD, preset generation,
   container lifecycle/reload, status in UI + tray.
7. **Polish:** auth toggle + keys, retention/pruning, test-connection, settings,
   error normalization pass, packaging (.rpm/AppImage) with the env workaround.

## 18. Open verification items (resolve during implementation, not blocking)
- Whether `GET /models?reload=1` suffices as the preset **reload** mechanism (vs container restart).
- Reliability of `WEBKIT_DISABLE_DMABUF_RENDERER` set inside `main()` vs launcher.
- Podman **CDI / GPU** flags for the llama-server container on this Fedora+NVIDIA host.
- Gemini streaming detail (SSE vs chunked JSON) and exact usage-token reporting.
- Tauri v2 external-URL window + tray feature flags / Fedora system deps.
