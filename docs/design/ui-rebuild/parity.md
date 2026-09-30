# lmgw web UI — feature-parity inventory

As-is inventory of the current axum + askama + htmx dashboard, for building the
Leptos replacement without re-reading the old code. No redesign opinions here —
only what the code actually does. File:line references point at the checkout
this was written against (`crates/lmgw-core/`).

Route table lives in `web/mod.rs:44-206` (`routes()`); it is the index for every
`file:line` below unless stated otherwise.

---

## Dashboard (`/`)

**Route:** `GET /` → `mod.rs:497 dashboard()`. Also `GET /events` (SSE, see
Live behavior) and `GET /assets/{*path}` (embedded static files,
`mod.rs:645 asset()`, `rust_embed`, `Cache-Control: no-cache`).

**Data shown** (`DashboardTpl`, `mod.rs:478-495`, rendered by `dashboard.html`):
- Router badge (`router_html`) — chat llama-server container state, from
  `state.router.cached_status()` (cached, no podman call on page load).
- Stats box (`stats_html`, partial `partials/stats.html`) — `req_last_minute`,
  `err_last_minute`, `active_requests`, `total_requests`, `total_errors`,
  `prompt_tokens`, `completion_tokens` (`telemetry::StatsView`,
  `telemetry.rs:47-56`), from `state.telemetry.stats()`.
- Live requests table: last 25 log rows (`store::query_logs`, `LogFilter{limit:25}`).
- Recent errors table: last 5 error-only rows (`errors_only:true, limit:5`),
  hidden entirely if empty.
- Connect panel:
  - `bases: Vec<net::Reachable>` (`{url, label}`) — every URL that reaches the
    gateway; a wildcard bind expands to loopback + LAN addresses
    (`net::reachable_urls`). Shown as OpenAI base (`{url}/v1`) and Anthropic
    base (`{url}`) copy-chips, one per reachable address.
    `base_url` = first of `bases`, used in the curl/SDK snippet.
  - `auth_enabled` flag — toggles "use a gateway key from Settings" vs.
    "any key value works" hint text.
  - `connect_models: Vec<ConnectModel{name, source}>` — `snap.exposed_models()`
    (explicit aliases + public locals; `source` is `"alias"|"local"`) plus one
    synthetic row per enabled `expose_all` upstream
    (`<prefix>/<any {name} model>`, `source:"catalog"`, rendered as a
    non-copyable ghost chip). Empty state links to `/local` and `/upstreams`.
  - Snippet `<details>`: curl, OpenAI SDK, Claude Code env-var block, using
    `base_url`.
- Backend containers table (`backends: Vec<BackendView>`, `mod.rs:400-476
  backend_views()`) — exactly 3 fixed rows, each with title, container name,
  live badge, direct API URL (`http://{host}:{port}/v1`), a UI link and a
  "manage" link:
  1. `llama-server · chat` → Web UI at `http://{host}:{port}/` (external link,
     opens llama-server's own UI), manage → `/local`.
  2. `llama-server · embeddings` → same pattern, manage → `/embed`.
  3. `audio.cpp` → **no external Web UI** (`audiocpp_server` answers `GET /`
     with 404 JSON-only); UI link instead points internally to `/audio-lab`,
     note text "no web UI — JSON API only", manage → `/audio`.
  `host` used for all these URLs is the request's own `Host` header
  (`view_host()`, `mod.rs:421-432`), not hardcoded loopback — so the links
  follow the browser when the dashboard is opened over LAN.

**Actions:** none besides navigation links and copy-chips (chips.js). No forms
on this page.

**Live behavior:** `GET /events` (`mod.rs:579-635`) — SSE, `hx-ext="sse"
sse-connect="/events"` on the outer div. On connect, immediately emits one
`stats` frame. Thereafter, for each broadcast `telemetry::Event`:
- `Event::Request(summary)` → emits a `reqrow` frame (rendered
  `partials/reqrow.html`, prepended into `#feed` via `hx-swap="afterbegin"`)
  **and** a `stats` frame (swaps `#stats-box`).
- `Event::Router(status)` → `router` frame, swaps `#router-badge`.
- `Event::Mcp(views)` → one `mcp-{id}` frame per server (used by the MCP tab,
  not this page).
- Lagged receiver → frame dropped silently.
Uses `sse.js` (vendored htmx SSE extension) + `KeepAlive::default()`.

---

## Chat (`/chat`)

**Route:** `GET /chat?t=<id>` → `chat.rs:54 index()`, just renders the shell
(`ChatTpl{thread_id}`) with a `<div id="chat-root" data-thread-id>` and loads
`/assets/chat/chat-app.js` as an ES module (import map maps `lit`/`marked`/
`highlight.js` to vendored bundles, no build step, no CDN). All real behavior
is a Lit web component (`ChatApp` + `ChatMsg` custom elements) talking to a
small JSON API:

| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | `/chat/api/threads` | `list_threads` (`chat.rs:67`) | sidebar list, most-recent-active first |
| POST | `/chat/api/threads` | `create_thread` (`chat.rs:84`) | body `{model_alias, kind}`; `kind` = `"chat"` or `"admin"` (anything else → `"chat"`) |
| GET | `/chat/api/threads/{id}` | `get_thread` (`chat.rs:103`) | `{thread, messages}` full history |
| POST | `/chat/api/threads/{id}/settings` | `update_thread` (`chat.rs:125`) | `{model_alias, system_prompt, temperature, max_tokens}` |
| POST | `/chat/api/threads/{id}/delete` | `delete_thread` (`chat.rs:150`) | |
| POST | `/chat/api/threads/{id}/send` | `send` (`chat.rs:167`) | body `{content}` → SSE stream of the reply |

**Data shown** (client-side state in `chat-app.js`):
- Thread sidebar (`this.threads`): title, `kind` badge (⚙ for admin).
- Model picker: `GET /v1/models` (the real `/v1` catalog, not a web-only
  endpoint), grouped by `owned_by` into `<optgroup>`s, "Local (llama.cpp)"
  first then alphabetical; each option labelled `id — $in→$out /M` when
  pricing is present (`fmtPricing`, per-million-token, "free" at $0).
- Message list: role, markdown-rendered content (assistant only; `marked`,
  no sanitizer — single-user local app), a collapsible "reasoning"/"thinking"
  block, inline tool-call cards (Admin Chat only — name, args, output,
  ms, error state), per-turn token counts, and a live stats row (see below).
- Settings drawer (gear icon): system prompt (admin threads: hint text
  "appended to the built-in admin prompt, not replacing it"), temperature,
  max tokens.
- Live stats panel: ttft, total, decode t/s, prefill t/s, cached-tokens
  (KV reuse), speculative-decode accept %, prompt→completion token counts,
  a context-usage bar (`ctxUsed / ctxMax`). Values are server-measured
  (llama.cpp `timings_per_token`) when available — flagged un-"~"-prefixed —
  else client-side estimated from wall-clock deltas (prefixed `~`).

**Actions:**
- **New chat** / **New admin chat** buttons → `POST /chat/api/threads`.
- Click a thread row → `openThread()`, `history.replaceState` to `/chat?t={id}`.
- ✕ on a thread row → `POST .../delete`; if it was the open thread, opens the
  next one or clears to `/chat`.
- Model `<select>` change / settings drawer fields → `patchThread()` →
  `POST .../settings` (fire-and-forget, no visible confirmation).
- Composer: Enter sends (Shift+Enter = newline); **Send** → `send()`; while
  streaming the button becomes **Stop** → aborts the `fetch` via
  `AbortController` (server still finishes and persists the partial reply —
  see below).
- Per code-block hover toolbar in rendered markdown: **Copy** (clipboard),
  and for `html`/`svg`/`xhtml` fences a **Preview** button that opens a
  ~90vw×90vh sandboxed `<iframe srcdoc>` modal (Esc or backdrop-click closes).

**Live behavior — `send()` (`chat-app.js:472-594`, server side `chat.rs:167-405`):**
1. Client optimistically appends a user bubble + an empty streaming assistant
   bubble, `POST`s `{content}` to `/send`.
2. Server: persists the user message immediately (survives a disconnect);
   auto-titles the thread from the first ~48 chars of the first message if
   still "New chat"; builds the IR message list from full history (admin
   threads prepend the built-in admin system prompt + any user-set extra,
   `adminchat.rs:50-57`; agentic turns are replayed from stored **IR**, not
   flattened text, so tool calls aren't lost on reopen — `chat.rs:417-440`).
3. Server resolves the alias, dispatches in-process via
   `proxy::drive_upstream` (no internal HTTP hop, no gateway key), forces
   `timings_per_token: true` for llama-server upstreams only, and streams SSE
   frames back: `delta` (text), `reasoning`, `tool` (`start`/`args`/`ready`/
   `result` sub-events, Admin Chat only), `usage`, `stats` (llama.cpp
   per-token timings), `stop`, `error`, terminal `done`
   `{message_id, prompt_tokens, completion_tokens, ttfb_ms, total_ms, aborted,
   timings}`.
4. Client parses raw SSE text itself (`parseSSE`, handles `:`-comment
   keep-alives) via `fetch` + `ReadableStream` reader — **not** EventSource
   (POST body, can't use EventSource). Text/reasoning deltas are batched into
   the next `requestAnimationFrame` for smooth high-rate rendering.
5. Server always persists the assistant reply (even if the client aborted or
   disconnected) and records a Logs row (`ingress_proto:"chat"`,
   `proxy::record_in_process`).
6. Client refreshes the thread list after send (picks up auto-title +
   reordering).

**Admin Chat** (thread `kind == "admin"`, `adminchat.rs`): same UI, different
dispatch — routed through `crate::agent::run` with the built-in `lmgw__*`
self-admin tools attached (`SelfAdminExecutor`), budgeted by the shared
`responses_max_tool_calls` / `responses_timeout_seconds` Settings
(`adminchat.rs:79-91`). If `self_admin` Setting is `off`, tools list is empty
and the send immediately errors with a message pointing at Settings. Runs
in-process, no token, no `/mcp` hop — deliberately separate from the token-
gated `/mcp/admin` route external agents use. Logged with `ingress_proto:
"admin"`.

---

## Audio lab (`/audio-lab`)

**Route:** `GET /audio-lab` → `audio_lab.rs:54 index()`, shell only (`<div
id="audio-lab-root">` + `/assets/audio-lab/audio-lab.js` module, import map for
`lit` only). All logic in the `AudioLab` Lit component.

| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | `/audio-lab/api/models` | `list_models` (`audio_lab.rs:72`) | enabled audio models + container state + voices dir paths |
| GET | `/audio-lab/api/voices?model=` | `list_voices` (`audio_lab.rs:124`) | proxies `proxy::handle_audio_voices` (real `/v1/audio/voices` logic) |
| GET | `/audio-lab/api/refs` | `list_refs` (`audio_lab.rs:216`) | stored reference clips |
| POST | `/audio-lab/api/refs` | `upload_ref` (`audio_lab.rs:231`) | multipart upload, body-limit disabled |
| GET | `/audio-lab/api/refs/{name}` | `get_ref` (`audio_lab.rs:328`) | serves a clip back for `<audio>` preview |
| POST | `/audio-lab/api/refs/{name}/delete` | `delete_ref` (`audio_lab.rs:308`) | |
| POST | `/audio-lab/api/speech` | `speech` (`audio_lab.rs:356`) | in-process wrapper over `proxy::handle_audio_speech` |
| POST | `/audio-lab/api/transcriptions` | `transcriptions` (`audio_lab.rs:363`) | in-process wrapper over `proxy::handle_audio_transcription` |
| POST | `/audio-lab/api/tasks/run?stream=1` | `tasks_run` (`audio_lab.rs:370`) | `proxy::handle_task_run` or `handle_task_stream` |

Reference clips live under `<audio models dir>/voices/`, mounted read-only
into the container at `/models/voices`; `safe_clip_name()` rejects anything but
`[A-Za-z0-9._ -]` and a non-leading dot (path-traversal guard — this is a path
that ends up inside a container request). Uploads are written to a `.part`
tmp file then renamed, so the container never sees a half-written clip.

**Data shown:**
- Header: model `<select>` (from `/api/models`), task badge, family badge,
  "streaming" badge if `mode=="streaming"`, a checkbox "via /v1/tasks/run" to
  force the generic panel even for tts/asr models (comparison tool), a live
  `audiocpp {state}` badge, **Reload** button.
- **TTS panel** (task `tts`): text box; voice picker (built-in voice ids from
  `/api/voices`) + reference-clip picker (from the voice library) +
  reference-text field; seed, max tokens, response format
  (`wav`/`json`/`pcm`), stream checkbox (streaming models only, forces
  `response_format=pcm`) with PCM rate/channels/bit-depth fields when
  streaming; free-JSON "options" box passed through verbatim.
- **ASR panel** (task `asr`): source = upload (multipart file) or server-side
  path (JSON, with quick-fill buttons from the clip library); language;
  stream-transcript checkbox (upload source only).
- **Generic panel** (`/v1/tasks/run`, used for the other 11 tasks or the
  override checkbox): one widget per field name in `TASK_FIELDS[task]`
  (`audio-lab.js:91-113`, hand-mapped from audio.cpp's actual
  `build_request_from_json`) — text/textarea/int/float/bool/clip-picker per
  `FIELDS` spec (`audio-lab.js:45-89`); plus a raw JSON "request overrides"
  box merged in last (can reach fields with no widget); a stream toggle for
  streaming-mode models (`/v1/tasks/stream` instead).
- Request preview `<details>`: exact JSON/multipart-description about to be
  sent, so what's tested is never a guess.
- Result panel: audio `<track>` player(s) + download link (one per
  `named_audio_outputs` entry, or one bare track), transcript text +
  detected language, timeline tables for VAD segments / diarization speaker
  turns / forced-alignment words (times shown in seconds via an editable
  "timeline rate Hz" — inferred from the result's own `sample_rate` when
  present, else user-set, default 16000), raw response JSON fallback, and a
  stats row: status, ttfb, total, bytes, and (when the JSON body carries it)
  audio.cpp's own `{wall_ms, audio_duration_ms, rtf}`.
- Side panel: voice library — upload (multipart, multiple files), per-clip
  size + preview (▸, plays inline `<audio>`) + delete (✕); shows the
  configured voices dir path or a hint to set one under the Audio tab.

**Actions:** Reload (`GET /api/models`), model select (re-fetches voices),
generic/TTS/ASR override checkbox, **Synthesize** / **Transcribe** / **Run
task** (label depends on panel) → dispatch; **Stop** aborts via
`AbortController` while running.

**Live behavior:** streaming responses are consumed as SSE by hand (same
`parseSSE` as Chat). Two event shapes: `speech.audio.delta` (base64 PCM
chunks, concatenated then wrapped in a hand-built WAV header from the
declared rate/channels/bit-depth — a raw PCM stream carries no format) and
`transcript.text.delta`/`transcript.text.done` (appended live).
`/v1/tasks/stream` returns a **buffered** event list in the final JSON
(`{events, result}`), not per-token SSE from that route itself, but is still
reached over `text/event-stream`-negotiated content-type detection.

Note: live-microphone transcription (`/v1/audio/transcriptions/live`) is
**explicitly not offered** here — the code comment says it needs a duplex
chunked upload the gateway doesn't proxy and a browser can't drive.

---

## Upstreams (`/upstreams`)

**Routes:**
- `GET /upstreams` → `admin.rs:95 upstreams_page()` (list).
- `POST /upstreams` → `admin.rs:172 upstream_create()`.
- `GET /upstreams/models?upstream_id=` → `admin.rs:475
  upstream_models_datalist()` — htmx partial, live model list for the
  alias-form datalist.
- `GET /upstreams/{id}` → `admin.rs:199 upstream_edit_page()`.
- `POST /upstreams/{id}` → `admin.rs:224 upstream_update()`.
- `POST /upstreams/{id}/delete` → `admin.rs:246 upstream_delete()`.
- `POST /upstreams/{id}/test` → `admin.rs:257 upstream_test()`.

**Data shown** (`UpstreamView`, `admin.rs:55-85`, from `store::list_upstreams`):
name (links to edit), protocol (openai/anthropic/gemini), kind
(generic/llama_server), base URL, masked API key (`•`×len for ≤8 chars, else
`first4…last4`), passthrough chip (`{prefix}/*` or `*` when `expose_all`, else
"–"), timeout ms, enabled ✓/–, a per-row **test** button.

**Actions:**
- **Add upstream** form: name, protocol select, kind select, base URL, API
  key (password field), extra headers (`Name: Value` per line, parsed
  strictly — a line without `:` is a validation error), timeout ms (default
  120000), `expose_all` checkbox + prefix, enabled checkbox (default on).
  `expose_prefix` is trimmed of slashes. On submit → insert + snapshot
  reload + catalog invalidation for that upstream id.
- **Edit page**: same fields pre-filled (API key field shows a masked
  placeholder; leaving it empty **keeps** the stored key — same convention
  used everywhere a secret field appears in this app), plus a
  `supports_responses` checkbox (native `/v1/responses` vs. gateway-
  synthesized) with explanatory copy. **Test connection** button (see below).
  **Delete** with `confirm()` (also removes the upstream's aliases via FK).
- **Test connection**: protocol-appropriate models-list probe
  (`admin.rs:272-317`) — `GET {base}/models` (+Bearer) for openai,
  `GET {base-without-/v1}/v1/models` (+`x-api-key`,
  `anthropic-version: 2023-06-01`) for anthropic, `GET
  {base-without-/v1beta}/v1beta/models` (+`x-goog-api-key`) for gemini.
  10s timeout. Returns an inline `<span class="badge">` fragment (OK or
  failed-with-tooltip, HTML-escaped).
- **Load model list** button (on the Models-alias forms, not here) → `GET
  /upstreams/models` → `<datalist>` fragment, swapped via
  `hx-target="#um-list-wrap" hx-swap="outerHTML"`; unknown/errored upstream
  falls back to "list failed: {e}"; the built-in router upstream (no `id` in
  `snap.upstreams`) instead lists local model ids with note "pick a local
  model id".

---

## Models (aliases) (`/models`)

**Routes:** `GET/POST /models` → `admin.rs:455 models_page()` /
`admin.rs:546 model_create()`; `GET/POST /models/{id}` →
`admin.rs:576 model_edit_page()` / `admin.rs:599 model_update()`; `POST
/models/{id}/delete` → `admin.rs:616`; `POST /models/hide` →
`admin.rs:632 model_hide()`; `POST /models/unhide` → `admin.rs:645
model_unhide()`.

**Data shown** (`ModelsTpl`, `admin.rs:346-357`) — three sources merged into
one table plus a collapsed "hidden" table:
1. **Manual aliases** (`AliasView`, DB rows, badge "manual"): alias name
   (chip, copyable), upstream name, upstream model id, param-override JSON
   dump, enabled ✓/–. No context-length column (shown as "–").
2. **Auto-exposed** (`AutoExposedView`, badge "auto", no DB row —
   `admin.rs:382-453 passthrough_auto_exposed()`):
   - every **public local model** (`snap.public_locals()`), target
     "llama-server (built-in)", edit link `/local/{id}`, context length =
     `ctx_size / max(parallel,1)` (per-slot context);
   - every model in an **enabled `expose_all`** upstream's **live catalog**
     (`crate::catalog::upstream_models`, cached fetch), name = `{prefix}/{id}`
     or bare id, edit link `/upstreams/{id}`, context length from the
     catalog. If the catalog call fails or returns empty, falls back to one
     wildcard placeholder row (`{prefix}/<any {name} model>`) instead of
     per-model rows.
   - passthrough rows carry `upstream_id`+`model_id` so they can be
     hidden; local-model rows do not (hide button omitted for them).
3. **Hidden** passthrough models (`snap.hidden_passthrough: Set<(upstream_id,
   model_id)>`) — collapsed under a `<details>` count, "restore" button.

**Actions:**
- **Add alias** form: alias name, upstream `<select>`, upstream model id
  (free text + **Load model list** htmx button populating a `<datalist>`),
  param-default fieldset (temperature/top_p/top_k/max_tokens — "client
  values win" when the request sets them too), enabled checkbox.
- Alias row: no inline actions besides the name link to edit; edit page has
  the same fields plus **Delete** (`confirm()`).
- Auto row: **hide** button (posts `{upstream_id, model_id}` to
  `/models/hide`) — public locals have no hide control.
- Hidden row: **restore** button → `/models/unhide`.
- All mutating actions reload the snapshot; alias create/update also don't
  invalidate the catalog (only upstream edits do).

---

## Local models (`/local`) — llama-server "chat" router

**Routes:** `GET/POST /local` → `admin.rs:831 local_page()` /
`admin.rs:1013 local_create()`; `GET/POST /local/{id}` →
`admin.rs:1045 local_edit_page()` / `admin.rs:1068 local_update()`;
`POST /local/{id}/delete` → `admin.rs:1089`; `POST /local/{id}/duplicate`
→ `admin.rs:1117 local_duplicate()`; `POST /local/apply` →
`admin.rs:1149 local_apply()`; `POST /router/{action}` →
`admin.rs:1169 router_action()` (action ∈ start/stop/restart).

**Data shown** (`LocalTpl`, `admin.rs:785-802`):
- Page head: router badge (live container state) + "preset out of sync"
  warning badge when `state.router.preset_in_sync(&models)` is false (models
  changed since the INI was last rendered to disk/applied).
- Warning banner if `models_dir` is empty.
- List table: model id (link), exposed-as (public name chip when
  `public && enabled`; "disabled" dim text when public-but-disabled;
  "private" dim text otherwise), GGUF path relative to models dir (+ "missing"
  badge if the scanned GGUF file list doesn't contain it — only flagged when
  the dir scan returned *something*, so an unreadable/empty dir doesn't false-
  positive every row), ctx-size, idle-sleep seconds, enabled ✓/–, a
  **duplicate** button per row.
- Orphans section: on-disk GGUFs (scanned via `crate::hf::scan_gguf_files`,
  split-part 2+ files excluded — they belong to their part-1 entry) not
  referenced by any local model's `gguf_path` or `draft_gguf_path` — each with
  a one-click "add" mini-form pre-filling `model_id` (suggested from the
  filename), `gguf_path`, `enabled=on`, `public=on`.
- **Add local model** form (id, GGUF path w/ datalist, idle-sleep default
  300, public checkbox default on, enabled checkbox default on) — all deeper
  server params are edit-page-only, added after creation.
- Generated `models-preset.ini` dumped verbatim (`render_preset`).
- `ui_url` — llama-server's own web UI at `http://{view_host}:{listen_port}/`
  (external link in the row-actions bar).

**Edit page** (`local_edit.html`, `LocalEditView` — every llama.cpp knob is a
blank-is-"use llama.cpp default" string field): fieldsets **Model** (id, GGUF
path), **Memory & performance** (ctx_size, n_gpu_layers/-ngl, threads, batch
size, ubatch/-ub, parallel/-np, flash_attn select
default/auto/on/off, cache_type_k/-ctk, cache_type_v/-ctv, cache_ram/
`--cache-ram` MiB with `-1`=no-limit/`0`=off semantics, fit/`--fit`
default(on)/on/off, fit_ctx/`--fit-ctx`), **Multimodal** (mmproj_path,
no_mmproj/`--no-mmproj` checkbox), **Chat template & reasoning**
(chat_template_file, reasoning select default(auto)/on/off/auto,
reasoning_format select default(auto)/auto/none/deepseek/deepseek-legacy,
reasoning_budget, reasoning_preserve tri-state select
(""/on/off — "template default" / "keep thinking in whole history" / "last
assistant message only"), reasoning_effort free-text+datalist
(none/minimal/low/medium/high/xhigh/max, llama-server passes it through
unchecked), chat_template_kwargs raw JSON-object text), **Sampling** (temp,
top_p, top_k, min_p, repeat_penalty, presence_penalty, seed), **Speculative
decoding / MTP** (draft_gguf_path, spec_type free-text+datalist of 10 known
values, spec_draft_ngl, spec_draft_n_max, spec_draft_n_min), **Runtime &
exposure** (idle_seconds, `args` raw-lines textarea for anything not modeled
above, jinja checkbox, public checkbox, enabled checkbox). Path fields
(`gguf_path`, `chat_template_file`, `mmproj_path`, `draft_gguf_path`) strip a
leading `/models/` or `/` on submit (`admin.rs:947-953`) — the renderer owns
that prefix; users may paste the in-container path shown elsewhere in the UI.

**Actions:**
- **Apply preset** (`/local/apply`) — re-renders `models-preset.ini` from
  every stored local model and, if the container is currently running,
  reloads it live (`state.router.apply_config`); refreshes + broadcasts
  router status. Button gets an `attention` CSS class when out of sync.
- **Start / Stop / Restart** (`/router/{action}`) — shared with the Tauri
  tray (`run_router_action`, `admin.rs:1179-1203`); `start` first
  (re-)writes the preset file even if unchanged, so a preset always exists
  before the container mounts it.
- **Duplicate** (`/local/{id}/duplicate`) — clones a model row (same GGUF,
  params, args, idle/enabled/public) under a fresh unique id
  (`<base>-copy`, `-copy-2`, …), redirects straight to the copy's edit page;
  explicitly does **not** apply the preset — the point is "one GGUF, several
  presets" without silently reloading the running container.
  [Add form has no direct duplicate entry point besides this button.]
- **Delete** (edit page, `confirm()`).
- All create/update/delete reload the snapshot; none of them auto-apply the
  preset (flash message explicitly says "apply preset to activate").

---

## Embeddings (`/embed`) — second llama-server ("embed router")

A second, independent llama-server container, kept separate so it stays
resident instead of being evicted by the chat router's model-swap logic.
Structurally mirrors Local + HF fused onto one page, scoped to embedding
models. The container is registered as a **managed upstream**
(`kind=llama_server`, name `llama-embed`, `expose_all=true`, `base_url`
tracking the configured port) — `ensure_embed_upstream()` (`embed.rs:35-70`)
creates/updates that row on every apply/start/settings-save, which is how
`expose_all` gives automatic passthrough with no per-model alias wiring; the
Models tab is used only for renames.

**Routes:**
- `GET/POST /embed` → `embed.rs:144 embed_page()` / `embed.rs:263
  embed_create()`.
- `GET/POST /embed/{id}` → `embed.rs:296` / `embed.rs:320`.
- `POST /embed/{id}/delete` → `embed.rs:338`.
- `POST /embed/apply` → `embed.rs:351 embed_apply()`.
- `POST /embed/router/{action}` → `embed.rs:376` (start/stop/restart, shared
  fn `run_embed_router_action`, `embed.rs:386-420`).
- `POST /embed/settings` → `embed.rs:438 embed_settings()`.
- HF sub-routes scoped to `target="embed"`: `GET /embed/hf/table`, `POST
  /embed/hf/download`, `POST /embed/hf/{id}/redownload`, `POST
  /embed/hf/{id}/delete`, `POST /embed/hf/check` — thin wrappers over the
  shared `web::hf` core (see HF section); browsing a repo is `GET /embed?repo=`
  (same page, not a separate route).

**Data shown:** router badge + "preset out of sync" badge
(`state.embed_router.preset_in_sync_with(&ini)`); warning if models dir
unset; model table (id, exposed-as chip w/ prefix or "disabled", GGUF path +
missing-file badge, pooling, ctx-size, idle-sleep, enabled); orphan-GGUF
add-mini-forms (same pattern as Local, no `public` field — embed models are
always exposed via the managed upstream once enabled); generated
`models-preset.ini` dump; embedded HF downloads table + repo browser (see HF
section, target `embed`); inline **Settings** section (image, container
name, host port, models dir, models max, exposure prefix, extra
`podman run` args, auto-start checkbox); `ui_url` (embed llama-server's own
web UI).

**Edit page fields**: model id, GGUF path, pooling select
(default/none/mean/cls/last/rank), ctx-size, idle-sleep, raw `args`
textarea, enabled. Much thinner than Local's edit page — no sampling/
reasoning/speculative-decoding fields (not meaningful for embedding models).

**Actions:** same shape as Local — Add form, per-model edit/delete, **Apply
preset & reload**, Start/Stop/Restart, orphan one-click add. Settings save
also re-syncs the managed upstream (`ensure_embed_upstream`) so port/prefix
changes take effect without a restart of the gateway itself.

---

## Audio (`/audio`) — audio.cpp container + model catalog

Third managed container, `audiocpp_server` (audio.cpp), configured via a
generated `server.json` instead of an INI. Also registered as a managed
upstream (`kind=audio_cpp`, name `audiocpp`, `expose_all`,
`supports_responses:false`) via `ensure_audio_upstream()` (`audio.rs:39-74`).
Model management additionally mirrors audio.cpp's own Gradio WebUI: an
installable **catalog** of model "families" fetched from upstream
`model_specs/*.json`, cached in-memory + in the `settings` KV store
(`audio:catalog` key) — **no implicit network fetch on page load**, only on
explicit **Refresh catalog**.

**Routes:**
- `GET/POST /audio` → `audio.rs:307 audio_page()` / `audio.rs:543
  audio_create()`. Query param `?repo=` triggers a repo browse inline.
- `GET/POST /audio/{id}` → `audio.rs:589` / `audio.rs:628`.
- `POST /audio/{id}/delete` → `audio.rs:649`.
- `POST /audio/apply` → `audio.rs:662 audio_apply()`.
- `POST /audio/router/{action}` → `audio.rs:684` (shared `run_audio_action`,
  `audio.rs:694-713`).
- `POST /audio/settings` → `audio.rs:734 audio_settings()`.
- `POST /audio/catalog/refresh` → `audio.rs:786 audio_catalog_refresh()`.
- `POST /audio/catalog/download` → `audio.rs:806 audio_catalog_download()`
  (`{family, package}` → resolves the package's repo + file list → queues
  via the shared HF machinery, target `audio`).
- HF sub-routes (target `audio`): `GET /audio/hf/table`, `POST
  /audio/hf/download`, `POST /audio/hf/{id}/redownload`, `POST
  /audio/hf/{id}/delete`, `POST /audio/hf/check`.

**Data shown:**
- Router badge + "config out of sync" badge
  (`state.audio.config_in_sync_with(&server_json)`).
- **Served models** table: id (link), exposed-as chip w/ prefix, family,
  path (+ missing-dir badge — checked against the audio models dir on disk),
  task, mode (offline/streaming), enabled. Empty state points at the catalog.
- **Add audio model manually** form — for models on disk but not in the
  catalog: id, family, path, task `<select>` (13 fixed values: tts, asr,
  gen, clon, vc, svc, s2s, sep, vad, diar, align, vdes, spk), mode
  (offline/streaming), enabled.
- **Model catalog** — one `<details>` per family (auto-open if any package
  installed), showing display name, category badge, tasks, "installed"
  badge, description + languages; nested package table (display name +
  "recommended" badge, format·precision, file count, installed/partial
  badge, and either a **serve** mini-form — pre-filled model id/path/task/
  mode, only shown once *installed* — or a **Download** button, or "no
  download source" text when the spec names no repo).
- HF downloads table + repo browser (target `audio`, same shared partial as
  Local/Embed's HF section).
- Inline **Settings**: image, container name, host port, audio models dir,
  backend `<select>` (cuda/cpu/vulkan/metal/hip — "must match the image"),
  device index, threads, exposure prefix, extra `podman run` args,
  lazy-load checkbox, auto-start checkbox.
- Generated `server.json` dump.

**Edit page fields**: model id, family, path, task select, mode select, load
options (raw JSON object textarea), session options (raw JSON object
textarea), voice presets (raw JSON object: `{name: {voice_id|voice_ref, …}}`
— validated server-side, `check_voice_preset()` requires each preset to name
either `voice_id` or `voice_ref`, else 400 with a pointed error, `audio.rs:
450-458`), default voice preset (bare preset name **or** an inline JSON
object — validated against the defined preset names when non-empty,
`form_default_preset()`, `audio.rs:424-445`), a read-only list of reference
clips on hand (from the Audio Lab voice library) with their server paths for
copy-paste into a preset, enabled checkbox.

**Actions:** same CRUD/apply/start-stop-restart shape as Local/Embed, plus:
- **Refresh catalog** — live-fetches `model_specs/*.json`, persists snapshot.
- Catalog **Download** (per package) — queues every file in the package via
  the shared HF downloader.
- Catalog **serve** (per installed package) — inserts a new audio model row
  pre-filled from the package's suggested id/path/task/mode.
- Save on create/update runs **`check_voice_refs()`** (`audio.rs:504-541`)
  server-side: every `voice_ref` path under the container's `/models` mount
  must exist as a real file *on the host* right now, because audiocpp_server
  opens reference clips at container **startup** — an unreadable path kills
  the whole container rather than just failing one request, so the form
  refuses to save it first. Paths outside `/models` are left unchecked.

---

## HF models (`/hf`) — chat router's Hugging Face downloader

Shared core (`web/hf.rs`) parameterized by a `target` string (`"chat"` |
`"embed"` | `"audio"`) selecting which `models_dir` a download lands in;
`/hf/*` is the `target="chat"` front end, reused verbatim by Embed and Audio's
HF sections (`table_tpl`, `browse_repo`, `queue_download`/`queue_files`,
`redownload`, `delete_tracked`, `check_updates` are all `pub(crate)` and
shared — no duplicated logic).

**Routes:** `GET /hf` → `hf.rs:192 hf_page()` (query `?repo=` triggers a
browse). `GET /hf/table` → `hf.rs:123` (self-polling partial, see below).
`POST /hf/download` → `hf.rs:338`. `POST /hf/{id}/redownload` → `hf.rs:350`.
`POST /hf/{id}/delete` → `hf.rs:357`. `POST /hf/check` → `hf.rs:364`.

**Data shown:**
- Warning if models dir unset.
- **Tracked downloads table** (`partials/hf_table.html`) — repo, file, size,
  status (progress bar + live `{received}/{total}` while `downloading`;
  "done" badge with downloaded-at tooltip; "update available" warn badge;
  "failed" err badge with error tooltip; else a plain dim badge — includes
  a synthetic "interrupted" status for rows stuck `queued`/`downloading` in
  the DB with **no** live in-memory task, i.e. the app restarted mid-download
  — offered a retry instead of a stuck spinner). Redownload button label
  varies: "update" / "re-download" / "retry". "add as model" mini-form
  (only for `status=="done"` **and** `primary_part` — split-GGUF parts 2+
  aren't independently loadable) posts straight to `/local` (or `/embed`)
  with `model_id` suggested from the filename, `gguf_path`, `enabled=on`,
  `public=on`. Delete with `confirm()` — removes the DB row **and** the
  downloaded file (+ any stray `.part`).
- **Download from a repo**: repo input + "List GGUF files" → same-page
  `GET` with `?repo=`, server fetches the repo file list and shows only
  `.gguf` paths with size + a "tracked"/"Download" state per file. Warns if
  no HF token is set and the caller may need one for gated repos.

**Actions / core semantics worth preserving:**
- **Queue download** (`hf.rs:221-268`): looks up the repo's file list live,
  expands a chosen file to every part of a split GGUF present in the repo
  (`hf::expand_parts`), tracks each part as its own `hf_models` row
  (`store::upsert_hf_model`), and spawns a background download task per
  part (`hf::spawn_download`). Errors if the file isn't actually in the repo
  or the target's models dir isn't configured.
- **Redownload** = retry/resume/update, same code path, re-checks the models
  dir is configured first.
- **Delete**: refuses while the download is actively running ("wait for it
  to finish"); otherwise removes DB row + on-disk file + `.part`.
- **Check for updates** (`hf.rs:310-326`): ETag-compares every `done` or
  already-`update_available` row against the hub; flips status when
  changed; aggregates errors into one flash message.
- **`GET /hf/table`** self-polls every 2s (`hx-trigger="every 2s"`) **only**
  while at least one row is `downloading` or DB-`queued` (`polling` flag),
  stopping automatically once nothing is in flight.

---

## Wiring (`/wiring`)

**Route:** `GET /wiring` → `wiring.rs:57 wiring_page()`. Read-only
visualization page — no forms of its own beyond the same one-click "wire up"
mini-forms reused from Local's orphan list and one "Apply preset" button; it
composes state already computed elsewhere (`super::admin::orphan_ggufs`,
`super::admin::OrphanView`).

**Purpose:** shows the *whole signal path* per model — HF download → GGUF on
disk → preset entry → exposure (public name / aliases) → container — as a
horizontal chain of `.node`/`.edge` elements, colored ok / warn / err / off,
each broken/pending link rendered as the actual action link that fixes it
(not just a status dot).

**Data shown, per local model** (`LocalChain`, `wiring.rs:16-30`):
1. **Source** node: "disk" (no tracked HF repo) / hf repo name (status
   `done`) / "update available" (links to `/hf`) / raw hf status string
   (queued/downloading/failed, links to `/hf`) — edge to the next node is
   `missing` unless status is `done` or there's no HF row at all.
2. **GGUF** node: path if the scanned file exists, else an err node linking
   to the model's edit page ("missing — fix path").
3. **Preset** node: "disabled — enable" (off, if `!enabled`) / model id
   (ok, if preset in sync) / "apply pending" (warn, if out of sync).
4. **Exposure** node: public name + alias chips (if public & enabled) /
   alias chips only (if any explicit aliases route to it via a llama-server
   upstream) / "private — make public" (warn, links to edit page).
5. **llama-server** node: "running" (ok) / "not running — start" (warn,
   links to `/local`) — gated on live router status, not preset sync.

Then **unwired GGUFs on disk** (orphans, same list/shape as Local's), each
rendered as a two-node chain (`gguf` → one-click "wire up" mini-form).

Then **remote upstreams** (`UpstreamChain`, `wiring.rs:32-40`, non-
llama_server-kind only): upstream node (enabled/disabled) → passthrough-
prefix chip **or** explicit-alias chips **or** a warn node "nothing exposed
— enable passthrough" linking to the upstream's edit page.

Also shows the router badge, "preset out of sync" banner + an "Apply preset
& reload" button when out of sync, a warning if the models dir is unset, and
the gateway's `/v1` base URL as a copy chip.

---

## MCP servers (`/mcp-servers`)

**Routes:**
- `GET/POST /mcp-servers` → `mcp.rs:120 servers_page()` / `mcp.rs:253
  server_create()`.
- `GET/POST /mcp-servers/{id}` → `mcp.rs:313` / `mcp.rs:346`.
- `POST /mcp-servers/{id}/delete` → `mcp.rs:363`.
- `POST /mcp-servers/{id}/test` → `mcp.rs:378 server_test()`.

Config-plane only — this is the admin UI counterpart to the northbound
`/mcp` protocol endpoint (mounted separately, not part of the web router).

**Data shown** (`McpServerView`, `mcp.rs:39-93`): name (link), **live status
badge** (`ready`/`connecting`/`error`/`stopped`, from
`state.mcp.status_views(&snap)`, tool count shown when ready, detail as a
title tooltip), transport (stdio/http/sse), endpoint summary (`podman:
{image}` when isolated, else the bare command, or the URL for http/sse),
tool prefix chip (`{prefix}__`), timeout ms, autostart ✓/"lazy", allow-
sampling ✓/–, enabled ✓/–, per-row **Test** button.

**Actions:**
- **Add MCP server** form: name, transport select (stdio/http/sse);
  fieldset for stdio (container image → Podman-isolated if set else bare
  subprocess; command; args textarea one-per-line; env `KEY=VALUE` lines,
  stored plaintext; working dir; extra `podman run` args); fieldset for
  http/sse (URL; headers `Name: Value` lines, plaintext); common fieldset
  (tool prefix — rejected if it equals the reserved `lmgw__`-adjacent self-
  admin prefix or contains chars outside `[A-Za-z0-9_-]`; timeout ms
  default 60000; idle seconds — reap after N s idle, 0=never; sampling
  alias free-text+datalist of exposed model names, with a hint to use a
  dedicated small/cloud alias rather than the same single-slot local model
  the caller uses; autostart checkbox default on; allow-sampling checkbox
  default on); enabled checkbox default on.
- **Edit page**: same fields pre-filled, plus **Delete** (`confirm()`).
- **Test** (`mcp.rs:378-393`): connect + `list_tools`, returns an inline
  badge fragment ("OK · N tools" or "failed" with an escaped error tooltip)
  — mirrors the Upstreams test button's shape.
- **Connect panel** (static, no action): shows the gateway's aggregated
  `/mcp` URL as a copy chip, explains the separate token-gated `/mcp/admin`
  route for self-admin tools (closed until a token is set in Settings; Admin
  Chat reaches the same tools without one), a note on lazy servers' bounded
  connect budget + `tools/list_changed` push behavior and the 64-char tool-
  name truncation-vs-drop rule, and a collapsible Claude Code/Cursor JSON
  config snippet (includes an `Authorization: Bearer <key>` header stanza
  only when `auth_enabled`).

**Live behavior:** page wraps its table in `hx-ext="sse" sse-connect=
"/events"`; each row's status badge has `sse-swap="mcp-{id}"`, fed by the
shared `/events` stream's `Event::Mcp(views)` frames (one `mcp-{id}` SSE
event per server per broadcast — see Dashboard's Live behavior).

---

## Workflows (`/workflows`) — replaced by the agent catalog

The IMAP-backed "Mail cleanup" surface this section used to inventory is
gone. `web/workflows.rs` fused an IMAP client, a hand-rolled model call, a
process-global job map and page-specific DTOs into one workflow that could
not be copied, exported or joined by a second one, and every generic piece
a second one would have needed — the MCP tool loop, the jobs subsystem, the
in-process turn runner — was built after it, so it never caught up. WP4 of
the agent catalog deletes the lot: `web/workflows.rs`,
`web/api_workflows.rs` and their routes, the `mail_*` ops, the `Mail*` DTOs
in `lmgw-api-types`, `pages/workflows.rs`, the `scripts/mock-imap*` harness
and the `async-imap` / `mail-parser` dependencies.

The authority on what replaced it is
`docs/design/2026-09-18-agent-catalog-design.md`; this is a
pointer at it, not a second copy. An agent is a JSON manifest (a model, the
prompts, the MCP tools it may reach, a config schema, a run shape) stored as
data, so adding one never rebuilds lmgw. The catalog is `/agents` and each
agent's detail page is `/agents/:id` (Run / Runs / Definition tabs);
`/workflows` survives only as a client-side redirect to `/agents`
(`pages::WorkflowsMoved`, routed in `crates/lmgw-ui/src/app.rs`) so old deep
links land. The mail workflow itself survives as the built-in `mail-labeler`
agent, with the shape this section described intact — list unread, one
structured model call per message, a review table, one apply step that
writes — but driven by the Google Workspace MCP server's `gws__gmail_*`
tools instead of IMAP, and with every run a `jobs` row of kind `agent_run`
instead of an entry in a process-global map.

---

## Responses (`/responses`)

Session manager for stored `/v1/responses` conversations (stateful API via
`previous_response_id`). Purely a management/observability surface — no
"create a response" action here (that only happens via the `/v1/responses`
API itself); this page shows and evicts what accumulates.

**Routes:**
- `GET /responses` → `responses.rs:87 index()`.
- `POST /responses/gc` → `responses.rs:232 gc_now()`.
- `POST /responses/settings` → `responses.rs:273 settings()`.
- `GET /responses/{chain_id}` → `responses.rs:136 chain_page()`.
- `POST /responses/{chain_id}/delete` → `responses.rs:207 chain_delete()`.

**Data shown:**
- Retention summary: total chains/responses currently held, a one-sentence
  rule description (`describe_rules`, `responses.rs:294-306`, e.g. "evicted
  after 3 idle day(s), keeping at most 500").
- Chains table (`ChainView`, capped to 200 most-recently-active rows —
  `PAGE_CHAINS`, a stated visible bound, with a footer note when the real
  total exceeds it): chain id (link), head response id (copy chip — what a
  client passes as `previous_response_id` to continue), model, response
  count, status badge (completed/failed/other), token counts (`in→out`),
  byte size (human-formatted), first/last activity timestamps, "awaiting
  approval" warn badge when the chain is blocked on an
  `mcp_approval_response`, per-row **delete** (`confirm()`).
- Chain detail page: every response in the chain, oldest first, each a card
  with id, status badge, created-at, tokens, output outline (e.g. "2
  mcp_call, message" — counts per output-item type), a pending-approval
  banner naming the tool calls awaiting approval (`{name} ({approval_id})`)
  when applicable, rendered assistant text, and the full response JSON
  pretty-printed behind a `<details>`.

**Actions:**
- **Retention settings** form: `responses_store` master-switch checkbox
  (off → new responses report `"store":false` and `previous_response_id` is
  refused; existing rows stay readable and still evicted), evict-after-
  idle-hours (0 = never), max-chains-kept (0 = unlimited).
- **Run now** (`scope=rules`) — applies the two eviction rules immediately
  instead of waiting for the hourly background sweep, so the effect is
  demonstrable.
- **Clear all** (`scope=all`, danger styling, double `confirm()` via both
  `onclick` and `onsubmit`) — deletes every stored chain regardless of the
  rules.
- **Delete** a single conversation (chain detail or list).
- Eviction is explicitly **chain-aware**: the unit evicted is the whole
  conversation (timed from its most recent response), never an individual
  response — ageing out responses one at a time would always delete a
  chain's root first (always the oldest row) and strand a conversation a
  client is still extending.

---

## Logs (`/logs`, `/logs/{id}`)

**Routes:** `GET /logs` → `admin.rs:1231 logs_page()`. `GET /logs/{id}` →
`admin.rs:1268 log_detail_page()`.

**Data shown:** filter form (alias text, upstream text, "errors only"
checkbox) → `store::query_logs` with `LogFilter{alias, upstream_name,
errors_only, limit:50, before_id}`. Table (`RowView`, `mod.rs:262-339`,
shared with the Dashboard feed and the SSE `reqrow` partial): time, alias
(link to detail), upstream + model (dim), ingress→egress protocol pair,
status (+ streamed ⇉ marker), TTFB, total ms, tokens (`in→out`), error kind.
"Older →" pagination link built from the last row's id (`before_id`),
carrying the current filters forward; only shown when exactly `limit` (50)
rows came back (heuristic for "there might be more").

**Detail page**: same row fields as a definition list, plus client key
(if any) and, for errored rows, error kind + full error message in a
`<pre>` block. "← back to logs" link. No actions besides navigation — pure
read view, no delete/replay.

---

## Settings (`/settings`)

**Routes:** `GET /settings` → `admin.rs:1345 settings_page()`. `POST
/settings/gateway` → `admin.rs:1372`. `POST /settings/router` →
`admin.rs:1438`. `POST /settings/hf` → `admin.rs:1480`. `POST
/settings/update` → `admin.rs:1513`. `POST /settings/update/check` →
`admin.rs:1540`. `POST /keys` → `admin.rs:1565 key_create()`. `POST
/keys/{id}/delete` → `admin.rs:1581 key_delete()`.

One page, five independent `<form>` sections, each posting to its own
endpoint (no single "save all").

**Gateway section** (`/settings/gateway`): bind address (validated as a real
`SocketAddr`, else the whole save is rejected; change needs an app restart
to take effect — stated in the flash message), `auth_enabled` checkbox (gate
on `/v1/*`), max request body size in MiB (0 = unlimited; explanatory
paragraph lists which routes it bounds — the JSON `/v1` routes — and which
it explicitly does **not** — `/v1/audio/*`, `/v1/tasks/*`; effective
immediately, no restart; over-limit responses are `413` with `code:
"body_limit"` naming this field), log retention days, log retention max
rows, self-admin tools mode select (off / read_only / full — the
*capability* gate for `lmgw__*` tools everywhere they're reached, including
Admin Chat; can only be changed here, deliberately excluded from the
`ops::settings_set` tool so the tools can't widen their own permission),
self-admin MCP token (the *reachability* gate for the separate `/mcp/admin`
route — empty closes that route entirely with a 404; also excluded from the
tool-settable surface), responses max tool calls, responses time limit
seconds (min 1 — a 0 is explicitly rejected as "would expire before the
first turn"). These last two double as the Admin Chat turn budget.

**Gateway API keys**: table (name, enabled, per-row **revoke** with
`confirm()`), **Create key** form (name only) — on success, re-renders the
whole Settings page directly (not a redirect) with the plaintext key shown
exactly once in a flash box (`new_key: Option<(name, plaintext)>` —
`admin.rs:1565-1579`); the key itself is `lmgw-{32 hex chars}`
(`rand_hex32`, `mod.rs:255-258`) and only its hash is stored.

**llama-server (router mode) section** (`/settings/router`): container
image, container name, host port, models dir, max loaded models (0 =
unlimited), public model prefix (empty = bare ids, e.g. `local` →
`local/<id>`), extra `podman run` args (textarea, one per line), auto-start
on app launch checkbox.

**Hugging Face section** (`/settings/hf`): access token (password field,
empty-keeps-stored convention; placeholder differs based on `has_hf_token`),
optional "clear the stored token" checkbox (only rendered when a token is
already set).

**Updates section**: read-only current version + manifest endpoint display;
form (`/settings/update`): "check for new versions in the background"
checkbox, registry token override (optional — a built-in read-only token is
the default; same empty-keeps/clear-checkbox convention as HF); **Check
now** button (`/settings/update/check`) runs a one-off check synchronously
and flashes the result ("update available: X (you have Y) — install it from
the tray menu" / "up to date (X)" / "update check failed: …"). The actual
install is explicitly **not** done from the web UI — it's driven by the
Tauri tray's native prompt.

**Paths section** (read-only): data dir, llama-server preset file path.

---

## Shared machinery

### Layout (`layout.html`)
Single fixed dark theme (`:root` CSS custom properties in `style.css:1-11`
— `--bg/--bg2/--bg3/--fg/--dim/--accent/--warn/--err/--border`; no light
theme, no `prefers-color-scheme` handling, no user-facing theme toggle
anywhere in the app). `<nav data-tauri-drag-region="deep">` is the tab bar
**and** doubles as the Tauri window's draggable title bar; window control
buttons (`#win-controls`, minimize/maximize/close-to-tray) are hidden by
default and only shown by `titlebar.js` when running inside the Tauri
webview (`window.__TAURI_INTERNALS__` present) — a no-op in an ordinary
browser. Tabs, in nav order: Chat, Audio lab, Dashboard, Upstreams, Models,
Local models, Embeddings, Audio, HF models, Wiring, MCP, Workflows (the slot
the agent catalog took over, see § Workflows), Responses, Logs, Settings.
`active` (a `&'static str` template field on every page) drives the
`.active` nav-link class — each handler hardcodes its own tab's key. A flash
banner region reads `flash.msg` (ok) / `flash.err` (err) from query-string
`?msg=`/`?err=` (see below). A single shared `<dialog id="wf-modal">` sits
at the body's end for htmx-swapped detail popups — generic (any
`hx-target="#wf-modal"` swap opens it via `modal.js`), though the Mail
workflow's message viewer was its only user. The agent Run tab keeps that
idea in the `Modal` widget, showing a row's exact model input.

### Flash / redirect convention (`mod.rs:223-246`)
`back(to, msg, err)` builds a redirect URL appending `?msg=` or `?err=`
(never both), percent-encoded via a bespoke `urlencode` (space → `+`,
alnum/`-_.~` passed through, everything else `%XX`). Nearly every mutating
POST handler ends in a `back(...)` redirect-with-flash rather than
re-rendering in place — the flash is then read back out by the next GET via
the shared `Flash{msg, err}` deserialize-from-query struct embedded in most
page query structs. A few handlers deliberately render directly instead of
redirecting — `key_create`, so the one-time secret it issues is shown rather
than lost in the redirect.

### `chips.js` — click-to-copy
Any element with `[data-copy]` copies its `data-copy` value to the
clipboard on click and flashes a `.copied` class for 900ms. Uses
`navigator.clipboard.writeText`, falling back to a hidden `<textarea>` +
`execCommand("copy")` for older WebKitGTK. Used pervasively for URLs,
model names, aliases, tool prefixes, response ids, etc. — anywhere a
`<code class="chip">` appears.

### `modal.js` — generic htmx-driven `<dialog>`
Presentational only. Opens `#wf-modal` after any htmx swap that targets it
and has content; closes on `[data-close]` click, backdrop click, or native
Esc; clears its children on close so the next open starts empty. No app
logic lives here — purely a wrapper behavior any future htmx-swapped detail
view could reuse.

### `select.js` — themed `<select>` replacement
WebKitGTK renders native `<select>` popups with GTK widgets that ignore
page CSS, so every `<select>` on the page is progressively enhanced into a
button + custom `<ul role="listbox">` (arrow keys, Home/End, Enter/Space to
commit, Esc/blur/Tab to close) while the original `<select>` stays in the
DOM (hidden) and keeps carrying the real form value — so plain server-side
form posts are unaffected, and it degrades to the native control without
JS. Runs on page load and re-scans on every htmx swap.

### `sse.js` — vendored htmx SSE extension
Unmodified htmx `sse` extension (minified). Provides `sse-connect`
(open an `EventSource`) and `sse-swap="<event>"` (swap this element's
content on that named event) attributes, plus `hx-trigger="sse:<event>"`
wiring and auto-reconnect with backoff on error. Every SSE-driven region in
the app (Dashboard's stats/router/feed, MCP's per-row status badges) is
wired through this, **except** Chat and Audio Lab, which hand-roll their own
`fetch` + `ReadableStream` SSE parsing because their streams are POST bodies
(EventSource only supports GET). The Mail workflow's progress block was the
one region that opened a stream of its own; agent runs dropped that and
follow the `jobs` frame on the shared stream instead (§ Workflows).

### `titlebar.js` — Tauri window chrome
No-op outside the Tauri webview. Adds the `.tauri` body class, wires
minimize/maximize/close-to-tray buttons, keeps the maximize glyph in sync,
and implements edge-resize dragging for the undecorated Linux window (6px
hit zone, cursor-direction detection, `start_resize_dragging` Tauri
invoke) — since undecorated windows have no native resize border. Not part
of "the web UI" in a browser sense, but must be preserved if the Leptos UI
still ships inside the same Tauri shell.

### `style.css` conventions worth knowing
`.chip` / `.chip.ghost` (copyable vs. non-copyable pill), `.badge` +
`.badge.{ok,warn,err,dim}` (status pills — the vocabulary every status
badge in this inventory uses), `.flash.{ok,warn,err}`, `form.grid` (label-
per-row form layout) vs `form.cfg` (the denser two-column edit-page layout
used by Local's edit page), `.row-actions` (inline button/link rows),
`.dim` (secondary text color), `.attention` (draws the eye to an out-of-
sync "Apply" button), `.danger` (destructive-button red). The `.wf-*`
two-pane layout classes went with the Workflows surface; nothing in the tree
uses them any more.

### Query-param conventions
- `?msg=` / `?err=` — flash message, consumed once per page load (see
  above).
- `?repo=` — HF/Embed/Audio pages' repo-browse state (also doubles as the
  redirect target after a download, so the browsed list stays visible).
- `?t=<id>` — Chat's deep-link thread id (`history.replaceState` keeps it
  in sync as the user switches threads client-side).
- `?before=<id>` — Logs pagination cursor.
- `?stream=1|true|yes|on` — Audio Lab's task-run stream flag (bespoke bool
  deserializer, `de_flag`, since serde's default bool parser rejects `1`).

### Hidden/non-obvious server-side behavior
- **Path-prefix stripping**: local-model path fields (`gguf_path`,
  `chat_template_file`, `mmproj_path`, `draft_gguf_path`) strip a leading
  `/models/` or `/` on submit, because the *read* side (edit page, tool
  plane) shows paths in their in-container `/models/…` spelling — pasting
  one back must not double the prefix.
- **Empty-keeps-stored** is a convention used for every secret-ish field:
  upstream API key, HF token, update token, and an agent config field
  declared `format: "secret"` (read back as `{has_value: true}`, never the
  value). Always paired with an explicit "clear" gesture where clearing is a
  real action — a checkbox for the HF/update tokens, a `clear` list on
  `agent_config_set` — but *not* for the upstream API key edit form (no
  clear option there — you must overwrite with a new value).
- **Snapshot reload + catalog invalidation** is fire-and-forget
  (`let _ = state.reload_snapshot().await`) after nearly every mutating
  handler — a reload failure is silently swallowed rather than surfacing
  as a save error.
- **"Apply" is always a separate, explicit step** from saving a model row
  — Local/Embed/Audio all say so in their flash messages ("… — apply
  preset to activate"). No handler auto-applies on save.

---

## Flows — multi-step processes spanning pages

### A. Serve a new local chat model (HF → disk → preset → exposure → test)
1. **HF models** (`/hf`): paste a repo, **List GGUF files**, **Download** a
   file (auto-expands split-GGUF parts). Table self-polls every 2s while
   downloading.
2. Once `status=="done"` and the row is the primary part, **"add as model"**
   posts straight to `/local` with `model_id` (suggested from the filename),
   `gguf_path`, `enabled=on`, `public=on` — creating the local-model row but
   **not** touching the running container.
3. **Local models** (`/local/{id}` edit page): tune server params (context
   size, sampling, reasoning, speculative decoding, …) as needed.
4. **Local models** (`/local`): **Apply preset & reload** re-renders
   `models-preset.ini` and hot-reloads the container if it's running (or
   **Start** if it isn't — `start` always rewrites the preset first).
5. The model is now live; it's auto-exposed (no alias needed) because it
   was created `public=on` — visible immediately on **Models** (`/models`,
   "auto" badge) and as a fully-`ok` chain on **Wiring** (`/wiring`).
6. Test it from **Chat** (`/chat`) by picking it in the model selector, or
   via `curl` using the Dashboard's Connect-panel snippet.
Cross-referenced pages: HF §, Local models §, Models §, Wiring §, Chat §,
Dashboard §.

### B. Serve a new embedding model
Same shape as flow A but entirely within the Embed page's own HF section
(`/embed?repo=`) and `/embed`'s own apply/start — the embed router is a
wholly separate container/preset/managed-upstream from the chat router, so
this flow never touches `/local`, `/hf`, or `/wiring` (Wiring only shows the
chat router's local models and non-llama_server upstreams). The "add as
model" mini-form posts to `/embed` and does **not** set `public` (embed
models have no `public` field — an enabled row is exposed once the preset
is applied, via the managed `llama-embed` upstream's `expose_all`).

### C. Install and serve an audio.cpp model (catalog path)
1. **Audio** (`/audio`): **Refresh catalog** (explicit — no auto-fetch).
2. Expand a family `<details>`, **Download** a package — queues every file
   in the package through the same shared HF downloader as flows A/B
   (tracked in the same `hf_models` table, target `audio`), progress
   visible in the page's own HF downloads table further down.
3. Once every file in the package is `done`, the package's row switches
   from "Download" to a **serve** mini-form — click it to insert a new
   audio-model row pre-filled from the package's suggested id/path/task/
   mode.
4. (Optional) open the new model's edit page to add voice presets — if any
   preset needs a `voice_ref` clip, it must already exist on disk under the
   audio models dir (see Flow E) or **Apply** will 400 with a pointed
   error (`check_voice_refs`).
5. **Apply config & reload** (or **Start**) to write `server.json` and
   (re)load the container; this also re-syncs the managed `audiocpp`
   upstream.
6. Test it in **Audio lab** (`/audio-lab`) — model appears in the picker
   once enabled+applied.
Cross-referenced pages: Audio §, Audio lab §.

### D. Voice cloning setup (clip upload → preset → apply → test)
1. **Audio lab** (`/audio-lab`), side panel: upload one or more reference
   clips (multipart, unbounded body size) — written under `<audio models
   dir>/voices/`.
2. **Audio** (`/audio/{id}` edit page): the model's edit page shows the
   same clip list (as their in-container `/models/voices/<name>` paths) so
   they can be pasted straight into a `voice_presets` JSON object
   (`{"name": {"voice_ref": "...", "reference_text": "..."}}`) and/or the
   `default_voice_preset` field. Save is rejected server-side if any
   `voice_ref` path doesn't exist on disk right now (audio.cpp opens
   reference clips at container **startup**, so a missing one would kill
   the whole container, not just one request).
3. **Apply config & reload** on `/audio`.
4. Back in **Audio lab**, the new preset appears in the TTS panel's
   "Built-in voice / preset" picker (from `GET /v1/audio/voices`); the
   voice-library clip can also be picked directly as an ad-hoc
   `voice_ref` without ever having gone into a saved preset.
Cross-referenced pages: Audio lab §, Audio §.

### E. Mail labeling: dry run → review/re-run → apply
The `mail-labeler` agent's run, and the shape every `batch` agent shares
(§ Workflows for the pointer, the agent-catalog spec §2.4 for the rules):
the **config form** on `/agents/mail-labeler`, rendered from the manifest's
config schema → **Dry run · classify** starts an `agent_run` job whose stage
and counts ride the `jobs` frame on the shared event stream, while the rows
themselves stay off that feed and come from `GET /api/agents/runs/{job_id}`,
which the Run tab re-reads whenever the done count changes → the **review
table** splits into attention rows (the model fell back, or the call failed)
and the rest, each row's category editable and checkable → optional
**Re-run attention rows** against the widened config, leaving every settled
row verbatim → **Apply** starts a second job, the only step with write tools
attached, whose result lists the tool calls it made. A row's subject opens a
modal with the exact model input, mid-run included. Still the most stateful
flow in the app, but the state is durable now: a run is a `jobs` row, so a
reload reopens it instead of expiring it.

### F. Passthrough upstream exposure, hide/unhide
1. **Upstreams** (`/upstreams`): add an upstream with `expose_all` checked
   and (optionally) a prefix.
2. Its whole live catalog appears automatically on **Models** (`/models`,
   "auto" section) and as a passthrough chain on **Wiring**
   (`/wiring`) — no alias rows needed.
3. Per-model **hide** (Models page) tucks one model into the collapsed
   "hidden" section (`snap.hidden_passthrough`) without touching the
   upstream config; **restore** undoes it. If the live catalog fetch fails
   or is empty, both Models and the Dashboard's Connect panel fall back to
   one wildcard placeholder row instead of per-model rows/hides.
4. A rename/param-default alias can still be layered on top via **Add
   alias** on the Models page, independent of the passthrough exposure.
Cross-referenced pages: Upstreams §, Models §, Wiring §, Dashboard §
(Connect panel).

### G. Self-admin capability vs. reachability (Admin Chat vs. `/mcp/admin`)
Two independent gates control the built-in `lmgw__*` tools, both set on
**Settings** (`/settings/gateway`):
- **Capability** (`self_admin`: off/read_only/full) — governs what the
  tools may do, enforced identically wherever they're invoked from. Cannot
  be changed by the tools themselves (deliberately excluded from the
  tool-callable settings surface).
- **Reachability** (`self_admin_token`) — governs whether the *external*
  `/mcp/admin` MCP route exists at all (empty token = the route 404s).
**Admin Chat** (a Chat-tab thread with `kind="admin"`) reaches the same
tools **in-process**, gated only by capability — no token, because it's the
owner driving the dashboard, not an external agent. This is the one flow
in the app where the same backend logic (`self_admin_tools`,
`SelfAdminExecutor`) is reachable from two different UI surfaces (Chat § /
Settings §) with two different exposure rules, and getting the distinction
wrong is an easy way to accidentally widen what an external MCP client can
do to the gateway's own configuration.

---

## Complete route table (web/dashboard plane)

GET/POST paths only; the `/v1/*` LLM-API routes, the northbound `/mcp` (and
`/mcp/admin`) protocol endpoints are **not** part of this router (mounted
separately in `server::build_router`) and are excluded except where a web
handler visibly reuses their machinery (noted in the last column).

| Method | Path | Handler | Kind | Reuses `/v1` machinery |
|---|---|---|---|---|
| GET | `/` | `mod::dashboard` | page | — |
| GET | `/events` | `mod::events` | SSE | — |
| GET | `/assets/{*path}` | `mod::asset` | static (rust_embed) | — |
| GET | `/chat` | `chat::index` | page (Lit island shell) | — |
| GET | `/chat/api/threads` | `chat::list_threads` | JSON API | — |
| POST | `/chat/api/threads` | `chat::create_thread` | JSON API | — |
| GET | `/chat/api/threads/{id}` | `chat::get_thread` | JSON API | — |
| POST | `/chat/api/threads/{id}/settings` | `chat::update_thread` | JSON API | — |
| POST | `/chat/api/threads/{id}/delete` | `chat::delete_thread` | JSON API | — |
| POST | `/chat/api/threads/{id}/send` | `chat::send` | SSE (POST body) | yes — `proxy::drive_upstream` / `egress::for_protocol` in-process, same path `/v1/chat/completions` uses; admin threads route through `agent::run` instead |
| GET | `/audio-lab` | `audio_lab::index` | page (Lit island shell) | — |
| GET | `/audio-lab/api/models` | `audio_lab::list_models` | JSON API | — |
| GET | `/audio-lab/api/voices` | `audio_lab::list_voices` | JSON API | yes — `proxy::handle_audio_voices` |
| GET | `/audio-lab/api/refs` | `audio_lab::list_refs` | JSON API | — |
| POST | `/audio-lab/api/refs` | `audio_lab::upload_ref` | JSON API (multipart, unbounded body) | — |
| GET | `/audio-lab/api/refs/{name}` | `audio_lab::get_ref` | binary | — |
| POST | `/audio-lab/api/refs/{name}/delete` | `audio_lab::delete_ref` | JSON API | — |
| POST | `/audio-lab/api/speech` | `audio_lab::speech` | passthrough (binary/JSON/SSE, unbounded body) | yes — `proxy::handle_audio_speech`, same fn `/v1/audio/speech` calls |
| POST | `/audio-lab/api/transcriptions` | `audio_lab::transcriptions` | passthrough (unbounded body) | yes — `proxy::handle_audio_transcription` |
| POST | `/audio-lab/api/tasks/run` | `audio_lab::tasks_run` | passthrough (unbounded body; `?stream=1` variant) | yes — `proxy::handle_task_run` / `handle_task_stream` |
| GET | `/upstreams` | `admin::upstreams_page` | page | — |
| POST | `/upstreams` | `admin::upstream_create` | form-action | — |
| GET | `/upstreams/models` | `admin::upstream_models_datalist` | partial (htmx) | reads `catalog::upstream_models` (the same live-catalog cache `/v1/models` uses) |
| GET | `/upstreams/{id}` | `admin::upstream_edit_page` | page | — |
| POST | `/upstreams/{id}` | `admin::upstream_update` | form-action | — |
| POST | `/upstreams/{id}/delete` | `admin::upstream_delete` | form-action | — |
| POST | `/upstreams/{id}/test` | `admin::upstream_test` | partial (htmx badge) | hand-rolled protocol-specific models-list probe (not `/v1`) |
| GET | `/wiring` | `wiring::wiring_page` | page | — |
| GET | `/api/agents` | `api_agents::list` | JSON API | — |
| POST | `/api/agents/import` | `api_agents::import` | JSON API (body limit disabled, a manifest is as big as its prompts) | — |
| GET | `/api/agents/runs/{job_id}` | `api_agents::run_detail` | JSON API (one run's rows; `runs` is a reserved agent id so it cannot be shadowed) | — |
| GET | `/api/agents/{id}` | `api_agents::detail` | JSON API | — |
| GET | `/api/agents/{id}/export` | `api_agents::export` | JSON API (download; secrets omitted) | — |
| GET | `/api/agents/{id}/runs` | `api_agents::runs` | JSON API (the agent's `agent_run` jobs) | — |
| POST | `/api/op/{agent_*,agents_restore}` | `api_agents::op`, dispatched out of `api::op` | JSON API (ops plane) | yes — a run's model turns go through `proxy::stream_once`, the same in-process path Admin Chat uses, logged as `ingress_proto = "agent"` (tool calls as `"agent-tool"`) |
| GET | `/models` | `admin::models_page` | page | reads `catalog::upstream_models` for auto-exposed passthrough rows |
| POST | `/models` | `admin::model_create` | form-action | — |
| GET | `/models/{id}` | `admin::model_edit_page` | page | — |
| POST | `/models/{id}` | `admin::model_update` | form-action | — |
| POST | `/models/{id}/delete` | `admin::model_delete` | form-action | — |
| POST | `/models/hide` | `admin::model_hide` | form-action | — |
| POST | `/models/unhide` | `admin::model_unhide` | form-action | — |
| GET | `/local` | `admin::local_page` | page | — |
| POST | `/local` | `admin::local_create` | form-action (also the orphan-GGUF "add" / HF "add as model" target) | — |
| GET | `/local/{id}` | `admin::local_edit_page` | page | — |
| POST | `/local/{id}` | `admin::local_update` | form-action | — |
| POST | `/local/{id}/delete` | `admin::local_delete` | form-action | — |
| POST | `/local/{id}/duplicate` | `admin::local_duplicate` | form-action | — |
| POST | `/local/apply` | `admin::local_apply` | form-action | — |
| POST | `/router/{action}` | `admin::router_action` | form-action (start/stop/restart) | shared `run_router_action`, also called by the Tauri tray |
| GET | `/mcp-servers` | `mcp::servers_page` | page | reads `state.mcp.status_views` (live connection state) |
| POST | `/mcp-servers` | `mcp::server_create` | form-action | — |
| GET | `/mcp-servers/{id}` | `mcp::server_edit_page` | page | — |
| POST | `/mcp-servers/{id}` | `mcp::server_update` | form-action | — |
| POST | `/mcp-servers/{id}/delete` | `mcp::server_delete` | form-action | — |
| POST | `/mcp-servers/{id}/test` | `mcp::server_test` | partial (htmx badge) | `state.mcp.test_connection` (connect + `list_tools`) |
| GET | `/embed` | `embed::embed_page` | page | — |
| POST | `/embed` | `embed::embed_create` | form-action (also HF "add as model" target) | — |
| POST | `/embed/apply` | `embed::embed_apply` | form-action | — |
| POST | `/embed/settings` | `embed::embed_settings` | form-action | — |
| POST | `/embed/router/{action}` | `embed::embed_router_action` | form-action | — |
| GET | `/embed/hf/table` | `embed::embed_hf_table` | partial (self-polling) | — |
| POST | `/embed/hf/download` | `embed::embed_hf_download` | form-action | — |
| POST | `/embed/hf/check` | `embed::embed_hf_check` | form-action | — |
| POST | `/embed/hf/{id}/redownload` | `embed::embed_hf_redownload` | form-action | — |
| POST | `/embed/hf/{id}/delete` | `embed::embed_hf_delete` | form-action | — |
| GET | `/embed/{id}` | `embed::embed_edit_page` | page | — |
| POST | `/embed/{id}` | `embed::embed_update` | form-action | — |
| POST | `/embed/{id}/delete` | `embed::embed_delete` | form-action | — |
| GET | `/audio` | `audio::audio_page` | page | — |
| POST | `/audio` | `audio::audio_create` | form-action (also catalog "serve" target) | — |
| POST | `/audio/apply` | `audio::audio_apply` | form-action | — |
| POST | `/audio/settings` | `audio::audio_settings` | form-action | — |
| POST | `/audio/router/{action}` | `audio::audio_router_action` | form-action | — |
| POST | `/audio/catalog/refresh` | `audio::audio_catalog_refresh` | form-action | — |
| POST | `/audio/catalog/download` | `audio::audio_catalog_download` | form-action | — |
| GET | `/audio/hf/table` | `audio::audio_hf_table` | partial (self-polling) | — |
| POST | `/audio/hf/download` | `audio::audio_hf_download` | form-action | — |
| POST | `/audio/hf/check` | `audio::audio_hf_check` | form-action | — |
| POST | `/audio/hf/{id}/redownload` | `audio::audio_hf_redownload` | form-action | — |
| POST | `/audio/hf/{id}/delete` | `audio::audio_hf_delete` | form-action | — |
| GET | `/audio/{id}` | `audio::audio_edit_page` | page | — |
| POST | `/audio/{id}` | `audio::audio_update` | form-action | — |
| POST | `/audio/{id}/delete` | `audio::audio_delete` | form-action | — |
| GET | `/hf` | `hf::hf_page` | page | — |
| GET | `/hf/table` | `hf::hf_table` | partial (self-polling every 2s while downloads are in flight) | — |
| POST | `/hf/download` | `hf::hf_download` | form-action | — |
| POST | `/hf/check` | `hf::hf_check` | form-action | — |
| POST | `/hf/{id}/redownload` | `hf::hf_redownload` | form-action | — |
| POST | `/hf/{id}/delete` | `hf::hf_delete` | form-action | — |
| GET | `/responses` | `responses::index` | page | — |
| POST | `/responses/gc` | `responses::gc_now` | form-action | — |
| POST | `/responses/settings` | `responses::settings` | form-action | — |
| GET | `/responses/{chain_id}` | `responses::chain_page` | page | reads the same store rows `/v1/responses` writes |
| POST | `/responses/{chain_id}/delete` | `responses::chain_delete` | form-action | — |
| GET | `/logs` | `admin::logs_page` | page | reads `store::query_logs`, the same table every `/v1/*` request writes to |
| GET | `/logs/{id}` | `admin::log_detail_page` | page | — |
| GET | `/settings` | `admin::settings_page` | page | — |
| POST | `/settings/gateway` | `admin::settings_gateway` | form-action | — |
| POST | `/settings/router` | `admin::settings_router` | form-action | — |
| POST | `/settings/hf` | `admin::settings_hf` | form-action | — |
| POST | `/settings/update` | `admin::settings_update` | form-action | — |
| POST | `/settings/update/check` | `admin::settings_update_check` | form-action | `crate::update::check` |
| POST | `/keys` | `admin::key_create` | form-action (renders page directly, not a redirect) | issues a real gateway API key, same kind `/v1/*` auth checks |
| POST | `/keys/{id}/delete` | `admin::key_delete` | form-action | — |

Rough shape: 2 page-shell routes for Lit islands (Chat, Audio lab) plus their
~15 JSON/SSE API routes; ~10 full server-rendered index/list pages
(Dashboard, Upstreams, Models, Local, Embed, Audio, HF, Wiring, MCP,
Responses, Logs, Settings — several of these also embed a sub-editor inline
rather than needing a separate page); 6 dedicated `{id}`-keyed edit pages
(upstream, alias, local model, embed model, audio model, MCP server) plus 2
read-only `{id}`-keyed detail pages (log detail, response-chain detail);
~50 POST form-action mutation routes; the agent catalog's 6 JSON routes and
its `agent_*` ops, which have no page here because the catalog is a route in
the SPA; 2 SSE streams (`/events`, `/chat/api/threads/{id}/send`); and a
handful of self-polling/htmx partial routes (`/upstreams/models`, the three
`*/hf/table` variants, the two `test`-connection badges).

