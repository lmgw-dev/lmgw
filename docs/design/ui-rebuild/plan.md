# UI rebuild — Leptos replacement for the htmx dashboard

Status: **done — P0–P8 built and verified** (2026-08-29). The Leptos SPA owns
`/`; the askama/htmx UI is deleted. Companion docs: [parity.md](parity.md) — as-is
inventory of the old UI (kept as the record of what was replaced);
[core-notes.md](core-notes.md) — core internals brief. This doc is the target
architecture and the phase plan.

## Goals (2026-08-29)

- Fully new UI, **feature complete** vs the old one, but free to reorder / merge / split
  pages. Modern, efficient, **quick, live and stateful** — no infinite scrolling, real
  visual structure, desktop-app feel (it always runs as a Tauri desktop tool).
- **Hybrid design, one system**: clean / minimal / round foundation (Claude / Gemini /
  ChatGPT register) for user-facing surfaces (chat, audio), a **dense variant of the same
  components** for admin surfaces. Same tokens, radii, palette; different spacing and
  information density. One user (the owner), daily driver, UI and core ship together.
- **HF-first model management**: adding a model starts from a Hugging Face repo (search,
  pick quant, download). Local GGUF file selection stays available only as the exception
  path. Wizard-shaped flows where multi-step.
- Automation (explicitly chosen): **auto-plan + auto-create when an HF download
  finishes**, and **HF update tracking** (detect new revisions of downloaded files,
  one-click re-download). Explicitly **not** chosen: auto-apply on save and auto-test
  after apply — container apply stays a deliberate, visible step.

## Architecture decisions

- **Leptos CSR** (client-side WASM), not SSR/hydration. Streaming needs (live logs, live
  generation, download progress) are served equally well by SSE in both modes; SSR's
  first-paint advantage is irrelevant for an always-on local tool. CSR keeps
  `lmgw-core` free of Leptos/WASM build coupling.
- New workspace crate **`crates/lmgw-ui`**: Leptos + leptos_router, built with **Trunk**
  to `dist/`, served by `lmgw-core` as embedded static assets (rust-embed style: embedded
  in release, read-from-disk in debug for a fast dev loop with `trunk watch`).
- New workspace crate **`crates/lmgw-api-types`**: serde DTOs shared by the JSON API
  (server side) and the UI (client side) — type-safe end to end, no hand-kept parallel
  types.
- **JSON + SSE admin API under `/api/`** in `lmgw-core`, alongside (not replacing) the
  existing htmx routes. Strictly separate from the `/v1` LLM plane. Live views (log tail,
  download progress, container state, generation preview) are SSE endpoints designed
  first-class, not retrofitted.
- **Parallel mount during the rebuild**: new UI at **`/ui`**, old UI stays at `/` until
  parity. At the end (done in P8): the SPA moved to `/`, `/ui/*` became a permanent
  redirect to the same path at the root, and the askama templates, htmx assets and old
  web handlers were deleted. The Tauri window already pointed at `/`, so nothing to flip
  there. The SPA's MCP page moved `/mcp` → `/mcp-servers`, because `/mcp` at the root is
  the northbound MCP protocol endpoint.
- Old UI receives **no changes** during the rebuild (frozen except critical fixes).

## Information architecture (draft — refine against parity.md)

Left **sidebar** (grouped), replacing the top tab row; content area uses master-detail
layouts instead of separate edit pages where it fits.

- **Use** (minimal density): Chat · Audio Lab · Workflows
- **Serve** (dense): **Models** — one unified surface for model aliases, local chat
  models, embeddings, audio models; origin (cloud upstream / local llama.cpp) and type as
  facets, not separate tabs. Primary action: **Add from Hugging Face** (wizard: repo →
  quant → download → auto-plan → created). Downloads + HF update tracking live here.
  Explicit **Apply** bar whenever container config is dirty.
- **Connect** (dense): Upstreams · MCP servers
- **Observe** (dense): Dashboard/Status · Traffic (logs + responses merged: live tail →
  detail drill-in) · Wiring
- **Settings** (dense)

Inventory-driven constraints (details + line refs in parity.md):
- **Three managed containers** — chat router, embed router, audio.cpp. Apply is a
  separate deliberate step *per target*; the apply bar and downloads views are
  per-target, not global.
- **Custom dropdown component required** everywhere: WebKitGTK ignores CSS on native
  `<select>` popups (that's why select.js exists). Same for consistent modals.
- **Secrets convention**: "empty input keeps stored value" + explicit clear checkbox.
- **Chat/Audio Lab** are already Lit+JSON/SSE mini-apps; their JSON APIs (threads, tasks,
  voices) are the porting base. Chat streams SSE over POST bodies → hand-rolled SSE
  parse over fetch ReadableStream in WASM too.
- **Download tables poll only while something is active** — keep that; better: SSE.
- Dashboard backend links must follow the request `Host` header (LAN access).
- Local-model path fields: container path prefix stripped on write, shown on read.

## Design system (locked 2026-08-29, sample: design-sample.html)

- **Themes**: dark-first cool graphite (`#16191C` base, cool off-white text), light
  Breeze-like neutral gray. Desktop-native, not editorial: never warm paper/cream tints
  (revised 2026-08-31 — the original warm palette read as a print page); never pure black.
- **Type**: IBM Plex Sans (UI, 400/500/600) + IBM Plex Mono (all data: model names,
  paths, logs, numbers). Bundled woff2, no CDN. Display = Plex Sans 600, tight tracking.
- **Color roles**: `accent` (Breeze-family blue, `#3DAEE9` dark / `#1F6DAD` light) =
  interactive only; `amber` = liveness (downloads, streaming, pulse); `green`/`red` =
  health/error status only. Status hues are signal-strength, not pastel. Model identity
  chips get a deterministic hue from the alias name hash.
- **Shape**: round foundation — controls r6, dense cards r10, airy cards r14, pill chips.
- **Density**: one component set; `.density-dense` (admin) vs `.density-airy` (user)
  flip spacing/control-height/type-size custom properties. No second component set.
- **Layout**: undecorated window → 40px titlebar (drag region: brand, ambient traffic
  pulse, window controls) + grouped left sidebar (eyebrow labels Use / Serve / Connect /
  Observe / Settings) + content pane. Master-detail inside pages; no infinite scroll —
  real tables with virtualization/pagination.
- **Signature**: the titlebar pulse — a small live sparkline of gateway traffic, amber
  when generating/downloading, calm dot when idle. Fed by the global SSE event stream.

## Build & CI

- Toolchain additions: `wasm32-unknown-unknown` target + `trunk` in
  `ci/install-build-deps.sh` (pinned prebuilt binary + sha256, not `cargo install` —
  building trunk from source pulls libdeflate-sys, which does not compile with
  Fedora's GCC 16); `ci/build.sh` runs `trunk build --release` for `lmgw-ui`
  before `cargo build`. CI: the project's existing runner.
- Verify current Leptos / Trunk versions via docs at implementation start (Leptos moves
  fast; don't trust memory).

## Phases

Each phase ends compiling, verified, and committed. Old UI keeps working throughout.

Progress: P0 ✅ · P1 ✅ · P2 ✅ (audio model CRUD deferred to P6) · P3 ✅ · P4 ✅ ·
P5 ✅ (chat; the deferred parity gaps closed 2026-08-29: highlight.js code
blocks with copy/preview toolbar + sandboxed HTML preview, `?t=` deep link and
URL sync, IR tool-card replay on reopen) ·
P6 ✅ (audio model CRUD on the Models page, Audio lab surface, container
lifecycle controls; live-container E2E done 2026-08-29: real TTS + ASR through
a dev-named audio.cpp container) ·
P7 ✅ (Wiring ported 2026-08-29 — `GET /api/wiring` + the chain view, plus the
Overview connect snippets; Workflows/mail ported 2026-08-29 — new
`GET /api/workflows/mail{,/stream,/message}` + `POST /api/op/mail_{fetch,
classify,reclassify,apply}` in `web/api_workflows.rs`, the JSON twin of the
htmx page: IMAP, classification, the job registry and the SSE state machine
(`workflows::job_frames`) are shared, not duplicated. The last bridge anchor
in the sidebar is gone — every page is now native. Titlebar pulse shows real
live ~tok/s: in-flight stream progress counted in the proxy drain, 2s rate
window, 500ms stats ticks only while active) ·
P8 ✅ (cutover 2026-08-29 — see the P8 entry below).
Container ops from a dev instance: shut the production lmgw down first and use
a dev-unique container_name (dev names collide with production otherwise).

- **P0 — skeleton**: `lmgw-ui` + `lmgw-api-types` crates, Trunk build, embedded serving
  at `/ui`, app shell (sidebar, routing, light/dark, titlebar), design tokens + the
  two-density component base. One local HTML token/component sample page first to lock
  the visual language before Leptos components exist.
- **P1 — API foundation + Dashboard**: `/api` scaffolding (error model, SSE helpers),
  status endpoint, live dashboard page as the proving ground for the live-data patterns.
- **P2 — Models domain** (the heart): unified models list, local model config editor,
  HF wizard (search → quant → download → auto-plan → auto-create), downloads panel with
  live progress, HF update tracking, explicit apply + test panel.
- **P3 — Upstreams · MCP · Settings**.
- **P4 — Traffic**: live log tail, request/response detail, responses view.
- **P5 — Chat** (first minimal-density surface, live generation view).
- **P6 — Audio + Audio Lab**.
- **P7 — Workflows/mail · Wiring · Dashboard polish**.
- **P8 — Cutover** ✅: SPA at `/` (Trunk `public_url = "/"`, `web/ui.rs` owns `/` plus a
  root catch-all that serves the shell for client-side routes and 404s under `/api/`,
  `/v1/`, `/chat/api/`, `/audio-lab/api/`); `/ui*` → 308 to the same path at the root.
  Deleted: `crates/lmgw-core/templates/` (34 files), `crates/lmgw-core/assets/` (14
  files), `web/mcp.rs`, and every page handler in `web/{admin,audio,embed,hf,responses,
  wiring,workflows,chat,audio_lab}.rs` — those modules now hold only the internals the
  `/api` plane and `crate::ops` call. `askama` dropped from `lmgw-core`. Docs + `ci/check.sh`
  updated; `web_pages.rs` rewritten against the `/api` plane.

  **Known gaps the cutover exposed** (features the old UI had that the new plane had not
  grown yet — all five are closed now; kept as the record of what was ported when):
  1. ~~audio.cpp spec catalog~~ — closed. `GET /api/audio/catalog` serves the *cached*
     snapshot (memory → kv, never an implicit fetch) mapped to families/packages with
     install state, on-disk size and the editor prefill; `POST /api/op/audio_catalog`
     (`action: refresh|download`) is the old `/audio/catalog/{refresh,download}` pair —
     refresh live-fetches `model_specs/*.json` and persists it, download queues the
     package's files through the shared HF queue (target `audio`), fire-and-forget, with
     progress read back from `/api/hf/downloads`. The Models page's audio section gained
     **Add from catalog**: families grouped (installed/served first, auto-open), packages
     with format·precision/file count/size/state, amber live progress, and a "Create
     model" action that opens the audio editor prefilled — the row is never auto-created,
     mirroring the old per-package "serve" form. `LMGW_AUDIO_CATALOG_ENDPOINT` overrides
     the catalog host (mock catalog in `scripts/mock-audio-catalog.py`, E2E in
     `tests/it/audio_catalog.rs`).
  2. ~~Hide/unhide passthrough models~~ — closed. `POST /api/op/model_visibility`
     (`action: hide|unhide`, `upstream_id` + `model_id`) wraps
     `store::{hide,unhide}_passthrough_model`; `/api/models/full`'s `passthrough` entries
     now carry `id` + `hidden`, and the Models page lists each passthrough upstream's live
     catalog with a hide/unhide affordance — a hidden row stays listed, dimmed, rather than
     vanishing.
  3. ~~Duplicate a local model~~ — closed. `local_model_set` gained a `duplicate` action
     (`ops.rs`), mirroring the pre-P8 `unique_copy_id` semantics exactly: a fresh
     `<id>-copy`/`-copy-2`/… id, GGUF/params/args/enabled/public all carried over verbatim.
     Wired to a row action on the Models page; `local_duplicate_clones_under_fresh_id`
     restored in `web_pages.rs`.
  4. ~~Responses store switch + eviction rules~~ — closed. Was already reachable via
     `settings_set_full`; the SPA Settings page gained a "Responses storage" section
     (store on/off, evict-after-idle-hours, max-chains) next to Tokens & updates.
  5. ~~`client_key` on a request log row~~ — closed. `dto::RequestRow` and
     `telemetry::RequestSummary` both carry it now (populated at every `record_*` site in
     `proxy.rs`/`mcp/ingress.rs`), so it rides `/api/logs` and the live `request` SSE frame
     alike; the Traffic page's detail drill-in shows it when non-empty.
