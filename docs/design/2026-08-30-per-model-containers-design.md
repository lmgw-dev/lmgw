# Per-Model Containers (lmgw) — Design

**Date:** 2026-08-30
**Status:** Draft v3 — v1 rewritten after adversarial review against the code
(29 findings folded in; the review's blockers reshaped §3.2, §5 and §6); v3
folds in owner review — sole-user deployment, zero alias rows — which
reversed v2's "keep the managed rows" call: the rows go, their names survive
as synthetic upstreams (§5)

> Companion to [2026-06-09-llm-api-gateway-design.md](2026-06-09-llm-api-gateway-design.md)
> ("§8" = its container section) and
> [2026-08-29-quickdoc-design.md](2026-08-29-quickdoc-design.md) ("quickdoc §9b"
> = VRAM admission control). This spec **supersedes router mode**: it replaces
> the shared llama-server router containers with one Podman container per
> loaded local model, managed directly by lmgw.

## 1. Summary

Quickdoc §9b already moved the brain into lmgw: lmgw does alias→model routing,
computes VRAM fit, picks eviction victims, tracks in-flight requests, and runs
the routers with `--models-max 0` so their own LRU never acts. What remains of
router mode is process supervision, a one-port-per-class proxy, and a
lifecycle API we wrote a shim full of workarounds against
(`vram/router_api.rs`: async unload with no in-flight guard, `autoload=false`
on every read, failed loads consuming evictions, an `Unknown` state).

The queueing motivation was **measured, and the router is not the culprit**
(§10.6): the live router showed no head-of-line blocking in any provoked
scenario — a cold load overlapped a running generation, two generations
interleaved fairly (GPU contention, not serialization), two cold loads ran
in parallel. The observed backend queueing therefore points at lmgw's own
side: the global one-decision-at-a-time admission gate held across load
waits, and eviction churn. §4 fixes that directly (the gate serializes only
the fit decision; loads proceed concurrently once claimed) — the per-model
pivot enables the fix but the honest motivations are control, uniformity,
and deleting the router shim, not a router bottleneck.

Decision: **lmgw spawns one Podman container per local model on load and
stops it on unload.** llama-server runs in direct mode (`-m` + `--alias`), no
router. Container running = loaded; `podman stop` = unloaded, with VRAM
release guaranteed by process death rather than promised by an async API. All
three planes (chat / aux / audio) share one lifecycle mechanism, each model
serves on its own port with no shared choke point, and the engine/container
becomes an attribute of the model object.

Corrected across drafts: the **three class-settings sections stay**
(chat/aux/audio `models_dir` + `public_prefix` are load-bearing for HF
downloads and the MCP tool surface — §6), and the identity question landed
on **synthetic upstreams**: the persisted managed aux/audio rows are
deleted, but their *names* survive as never-persisted synthetic upstreams so
quickdoc corpus pins and the embed/rerank cross-guard keep matching (§5).
The deciding input was owner review: this is a sole-user deployment with
zero alias rows, so the alias-FK cascade that kept the rows in v2 protects
nothing real. What goes is the router protocol and the shared containers,
not the class structure around them.

## 2. What the router did → what replaces it

| Router-mode function | Replacement |
|---|---|
| Spawn/supervise child per model | `podman run -d --replace` per model; podman is the supervisor |
| One port per class multiplexing N children | Per-model host port + in-memory runtime registry (§3.2) |
| `POST /models/load` + poll `loaded` | `podman run` + poll llama-server `GET /health` until 200 |
| `POST /models/unload` + poll `unloaded` | `podman stop` + `podman wait`; NVML confirms release |
| `GET /slots` busy check before evict | lmgw in-flight ledger, plus a direct-port busy probe (§4) |
| `sleeping` warm tier | Dropped. §9b spike: wake is a full reload anyway; page cache covers the disk read |
| `models-preset.ini` / audio `server.json` (multi-model) | Per-model argv rendered at start (§3.6); audio: per-model config dir |
| `GET /v1/models?reload=1` apply | Apply = recreate that one model's container (only if running) |
| Router count-LRU (`--models-max`) | Already disabled; concept deleted |
| Aux/audio catalog via HTTP `GET {base}/models` | Table-driven enumeration; `context_length` from GGUF metadata (§5) |

## 3. Runtime model

### 3.1 Class, engine, descriptor

The three model tables stay (`LocalModel`, `AuxModel`, `AudioModel` — no
merge). **Class** = `chat | aux | audio` (the table); **engine** = `llama |
audio` (derived: chat+aux → llama, audio → audio.cpp). A `ModelRuntime`
descriptor is derived at snapshot build:

- `class`, `engine`, `model_id`.
- Argv/config inputs: `LlamaParams` + `args` for llama; the audio fields for
  audio.cpp. Rendering rules in §3.6.
- `image`: per-model override (new optional column on all three tables),
  falling back to the class settings. Pins a llama.cpp build per model.
- `extra_run_args`: per-model override (new optional column), falling back
  to class settings. The podman-level hook: per-model `CUDA_VISIBLE_DEVICES`
  via `-e`, memory limits, device placement.
- `idle_seconds`, `warm_start` (new column: start at boot; still evictable).

### 3.2 Registry and the acquire primitive

In-memory registry: `(class, model_id) → { container_name, host_port, state:
starting|ready|stopping, started_at }`, plus the existing `in_flight` /
`last_used` maps re-keyed from `(Container, model_id)` to `(class,
model_id)`. Rebuilt from podman at boot (§3.4); never persisted.

The core primitive every consumer uses is **`acquire(class, model_id) →
Endpoint guard`**: under one registry lock, look up the runtime; if `ready`,
increment in-flight and return the endpoint; if absent/stopped, start (§3.6)
and then do the same. The guard decrements in-flight on drop. Crucially:

- **`acquire` is independent of VRAM admission.** `admit()` returns
  `Ok(None)` on four legitimate paths today (vram disabled, unclassifiable
  route, no footprint, no capacity signal) and the no-telemetry degraded mode
  is documented as "the whole gateway runs unchanged". In router mode the
  router autoloaded on those paths; per-model, `acquire` is what starts the
  container. Admission (§4) is an arbitration layer that runs *before*
  `acquire` when active; when inactive, `acquire` alone keeps every path
  working.
- The in-flight increment and the residency check are atomic under the
  registry lock, closing the race where the idle reaper or an eviction stops
  a container between "is it up" and "forward". Eviction and the reaper take
  the same lock and skip anything with `in_flight > 0`.
- A connect-refused on forward (the window can still exist across a crash)
  retries once through `acquire`.

### 3.3 Naming and labels

- Name: `<container_prefix>-<class>-<slug(model_id)>-<hash6>`.
  `container_prefix` is a new global setting (default `lmgw`); a dev instance
  sets its own prefix, removing the dev/prod name-collision class entirely.
  The class component prevents a chat and an aux model with the same
  `model_id` (unique per table only) from colliding; `slug()` = lowercase,
  `[a-z0-9-]`, runs collapsed, trimmed, capped; `hash6` = short hash of the
  raw `model_id`, making the name injective even where the slug is lossy.
- Labels on every managed container: `lmgw.instance=<prefix>`,
  `lmgw.class=<class>`, `lmgw.model=<model_id>`, `lmgw.engine=<engine>`.
  Reconciliation filters on the instance label, never on name patterns.

### 3.4 Reconciliation, deletion, shutdown

Boot: `podman ps -a --filter label=lmgw.instance=<prefix>` →

- Running container whose inspected `Cmd` matches the currently rendered
  argv **and** whose model still exists and is enabled: adopt (recover host
  port from the inspected port mapping; mark `ready` after a `/health`
  probe). This is the crash-recovery path — models stay warm across an lmgw
  crash.
- Anything else (stale argv, deleted model, exited): `podman rm -f`.
- Nothing auto-starts except `warm_start` models.
- One-time legacy sweep: stop/remove the router-mode containers under the
  names the **pre-migration** settings carried (`container_name` is read from
  the old settings JSON before the shape migration rewrites it — ordering
  matters, §6), so an upgraded instance never runs both worlds.

Runtime: deleting or disabling a model stops and removes its container
immediately (not at next boot — `--replace` only collects same-name
containers). Graceful shutdown stops all managed containers (a tray app that
quit should not leave the GPU full); a crash leaves them for boot adoption.

### 3.5 Ports

Allocated inside `acquire`'s start path, under the registry lock: bind a
listener on port 0, take the port, release it, pass `-p <port>:8080`
(publish behavior unchanged from today — all interfaces, since the Overview
page links the container's own port using the browser's hostname and live-ASR
clients connect to it directly). No persisted port fields, no configured port
range (no invented static limits). After an lmgw restart the port comes back
from `podman inspect`. The residual race (an outside process grabbing the
port between release and `podman run`) is handled by treating a failed run as
retry-with-fresh-port — measured (§10.4): `podman run -d` fails
synchronously on a port conflict (exit 126, pasta names the port in
stderr), but leaves the container object in `created` state, so the retry
path removes the husk (or leans on `--replace`) before rerunning.

### 3.6 Lifecycle verbs

- **start(model)**: render argv → `podman run -d --replace --name … --label …`
  → poll `GET /health` until 200 or `load_timeout_seconds` (existing
  setting). On failure the error carries a `podman logs --tail` excerpt. No
  `--rm`: a stopped container keeps its logs for post-mortem; `--replace`
  collects it on the next start. Measured health semantics (§10.3): this
  image has **no 503-loading phase** — the port is unreachable until the
  model is up, then answers `200` directly (the image fronts llama-server
  with nginx, so early probes can see a recv-error rather than
  connection-refused; the poll treats any non-200 the same). Measured cold
  start (§10.2): `podman run` overhead ≈0.3–0.5s; run→ready 2.1s for a 2B,
  9.3s for a 27B — container overhead is noise against weight upload, as
  assumed.
- **Argv rendering is not a mechanical INI transform.** The renderer keeps
  the canonical-key dedup table (`SHORT_ALIASES`) and the `/models` +
  `/config` mount path rewrites, and adds the CLI semantics the preset hid:
  boolean flags render as bare switches (`--jinja`, never `--jinja true`),
  `false` on a default-on option renders the `--no-*` twin (the
  `reasoning_preserve` pattern), value-taking options keep their value
  (`--flash-attn on`). Direct mode **must pass `--alias <model_id>`** —
  clients send `"model": <model_id>` and llama-server would otherwise name
  the model after the file. Aux quirks move from the preset renderer into
  argv: `--reranking` without `--pooling` for rerankers, `--embeddings` for
  embedders.
- **Audio config**: audio.cpp still wants a `server.json`; each audio model
  gets its own config dir (`data_dir/audiocpp/<slug>/server.json`) mounted
  `:ro`, rendered single-model. `AudioSettings`' engine fields (`backend`,
  `device`, `threads`, `lazy_load`) keep flowing into it.
- **stop(model)**: `podman stop -t <grace>` then `podman wait`. Measured
  (§10.1): direct-mode llama-server exits cleanly in ~0.4s on SIGTERM even
  mid-generation (exit 0) — the SIGTERM-deafness that forces SIGKILL after
  the full grace is **router-mode-specific**. Default grace 10s stays as a
  ceiling that is never normally reached. The
  scheduler's eviction path and the idle reaper refuse while
  `in_flight > 0`; the explicit ops path (`lmgw__container` stop) gains the
  same refusal with an `override` escape — today `ops::container` stops
  unconditionally, so this is **new work, not an existing property**.
- **apply(model)**: running → stop + start (fresh argv); not running → no-op
  (argv is rendered fresh at every start, so there is no drift concept). The
  static pre-flight that chat apply runs today (missing gguf/mmproj/draft
  paths, `model_warnings`) moves to `start()`/per-model apply and stays in
  the group-apply response shape (§8).
- **logs(model)**: new first-class action (op + UI, §8) — `podman logs
  --tail N` for that model's container. With N containers this is the only
  place a failed start is visible; it cannot stay CLI-only.
- **probe / help**: `modelinfo`'s architecture probe and `llama-server
  --help` vocabulary read move from "exec into the shared running container"
  to a throwaway `podman run --rm <image> …`; the help cache is **keyed by
  image** (per-model overrides mean there is no single flag vocabulary; a
  model's args validate against *its* image's help, and `lmgw__llama_flags`
  gains an optional model/image argument, defaulting to the chat class
  image).

### 3.7 Idle reaper

`idle_seconds` changes meaning: from llama-server's per-child sleep to **lmgw
stopping the container** after `idle_seconds` with `in_flight == 0` (0 =
never). One periodic task over the registry, under the registry lock (§3.2).
Uniform across engines — audio.cpp gains idle unload for the first time,
which router mode structurally could not do.

## 4. Scheduler (§9b successor)

The admission design — ledger, one-decision-at-a-time gate, LRU eviction,
visible queueing, `AdmissionGuard` RAII — survives, but is now explicitly a
**arbitration layer wrapped around `acquire`** (§3.2): when active it decides
*whether and when* `acquire` may start a container; when inactive (vram
disabled, no telemetry and no budget) `acquire` runs unarbitrated, exactly
like today's degraded mode. Primitive swap:

| §9b primitive (router_api.rs) | Per-model-container primitive |
|---|---|
| `models` / `state_of` / `is_up` | Registry state (+ `/health` probe on adoption) |
| `load` + `await_state(Loaded)` | `start(model)` via `acquire` (§3.6) |
| `unload` + `await_state(released_vram)` | `stop(model)` + `podman wait` |
| `busy_slots` | `in_flight > 0`, **plus** a `/slots` probe against the container when the build serves it (§10.7) |

Consequences and corrections:

- `vram/router_api.rs` is deleted whole; `Sleeping`/`Unknown` go with it.
- Ledger keys simplify to `(class, model_id)`. The `Container {Chat, Aux,
  Audio}` enum, port-based `classify()`, `evict_audio()`-by-restart, and the
  `audio_resident` shadow set are deleted — audio is a registry entry like
  everything else.
- **The in-flight ledger alone is not sufficient for the busy check.** lmgw
  is *not* the sole ingress: the dashboard publishes container ports for
  direct use (Overview backend links, llama-server web UI, live-ASR clients
  are told to hit the container port). Direct-port traffic is exactly what
  `/slots` catches today. Per-model containers keep a busy probe against the
  victim's own `/slots` before stopping it, combined with `in_flight` —
  measured (§10.7): direct-mode llama-server serves `/slots` by default in
  this build (no flag needed), with `is_processing` flipping correctly
  under load.
- **Measurement upgrade — spike passed, now design** (§10.5): NVML
  per-process attribution works through rootless podman (the compute-apps
  PID is exactly the container's llama-server host PID, mapped via `podman
  top pid hpid`), so the resident side of the ledger becomes measured
  per-PID instead of GGUF-estimated (`GpuProbe` gains a per-process
  method). The spike also showed why this matters: a 1.3 GiB GGUF occupied
  5.1 GiB of VRAM at its configured context — file-size estimates are not
  even the right order of magnitude once KV cache dominates. Independently,
  the eviction filter `estimated_bytes > 0` must not survive: a resident
  model whose footprint reads zero would be unevictable forever; residency
  in the registry, not a nonzero estimate, is what makes a model a
  candidate. *Implementation status (2026-08-31): the branch shipped with
  device-level NVML measurement retained (admission decides on measured
  free bytes exactly as §9b did) and GGUF estimates for planning; the
  per-PID attribution this bullet describes is spike-verified and remains
  a follow-up, not yet wired into the ledger.*
- **The gate holds only the decision, not the load.** §10.6 measured that
  the backend queueing does not come from the router — which leaves the
  §9b gate itself, held across "load + await loaded", as the remaining
  serialization point. Per-model containers make the fix natural: the gate
  covers measure → fit → claim (ledger reservation) and is released before
  the container start; loads of different models then proceed
  concurrently against their reservations (the router itself handled
  parallel child loads fine, and independent containers trivially do).
  This is in scope, not follow-up — it is the actual fix for the observed
  queueing. Per-GPU gates stay follow-up.

## 5. Routing, identity, and the forwarding handoff

**The managed upstream rows are deleted; their names become synthetic.** v2
kept the persisted aux/audio rows for three reasons. Owner review collapsed
the first — sole-user deployment, zero alias rows, so the
`models.upstream_id` cascade protects nothing — and the other two turn out
to hang on the rows' *names*, not their persistence: every consumer
(`Snapshot::aux_model_for`, `vram::classify`, `quickdoc/embed.rs`'s
identity match) compares `route.upstream.name`. So the design generalizes
the pattern chat already uses (`Snapshot::router_upstream()`,
`ROUTER_UPSTREAM_ID = -1`, synthesized on the fly, never stored):

- **Three synthetic upstreams, one per class**: `llama-server` (chat, id
  −1), `llama-aux` (aux, id −2), `audiocpp` (audio, id −3). Names kept
  verbatim so existing quickdoc corpus pins (`embed_upstream =
  "llama-aux"`) keep matching with **zero data migration**; sentinel ids;
  never persisted. `AUDIO_UPSTREAM_NAME` moves from `web/audio.rs` into
  `config.rs` beside the other two constants.
- The persisted rows and `ensure_aux_upstream`/`ensure_audio_upstream` are
  deleted. The migration removes the two rows and **refuses loudly** if any
  alias or hidden-model row still references them, naming the offenders —
  surfaced, never a silent cascade. (Owner-verified: zero such rows in the
  live deployment.)
- Consequence, stated not hidden: **local models are not aliasable in any
  class.** Chat locals never were (an alias row's FK needs a real upstream
  row); aux/audio lose a capability with zero users. If aliasing locals is
  ever wanted it is a new class+model-keyed mechanism (§11).
- The zero-vector cross-guard (a reranker answering `/v1/embeddings`) keeps
  working through the synthetic name match; moving the predicate onto a
  class field carried by `Route` is a cleaner implementation option, not a
  spec requirement.

**Forward URLs.** A synthetic upstream's `base_url` is invalid by design
until `acquire` runs — there is no `listen_port` to derive one from any
more. After `admit()` (and its inner `acquire`), the route's
`upstream.base_url` is **overwritten with the acquired endpoint**
(`http://127.0.0.1:<port>/v1`) before the egress adapter builds the URL —
the adapters (`egress/openai.rs`, `responses.rs`, the proxy
audio/rerank/tokenize paths) all derive from `base()`, so this one swap
covers them. Loop-scoped guards (responses turn-loop, agent chat, quickdoc
ingest/golden) carry the endpoint with the guard they already hold. Any code
that reads a local route's `base()` without holding an acquire guard is a
bug by definition.

**Every local-backend touch goes through `acquire`.** The review found the
paths that today free-ride on router autoload; each is named here with its
treatment: `web/workflows.rs::llm_classify` → acquire; audio
`handle_audio_voices` → acquire; `modelinfo::local_model_test` → full
`admit()` (it should exercise the real path); `catalog::fetch_models`
against aux/audio rows → **removed** (next point).

**Catalog/exposure become table-driven.** `GET /v1/models` currently
enumerates aux/audio by HTTP-fetching the routers' catalogs through the
expose-all rows — with per-model containers there is no always-on port to
fetch, and no expose-all rows either. `exposed_model_names`/
`exposed_models` enumerate enabled aux/audio models from their tables under
the class `public_prefix`, with `context_length` from GGUF metadata (already
parsed for VRAM planning) instead of a live `n_ctx`. Exposure keys on
`enabled` — aux/audio tables deliberately have no `public` column and this
spec does not add one. **Resolution precedence changes, stated plainly:**
aux/audio move from the third tier (expose-all passthrough) into the second
(table-driven local resolution, beside public chat locals); aliases stay
first. A cloud `expose_all` upstream sharing a prefix now loses to a local
model of the same name — acceptable in a sole-user deployment that names
its prefixes deliberately.

Cross-model head-of-line blocking is gone by construction: two ready models
are two independent HTTP servers; the only remaining serialization is the
admission gate itself, which is genuine VRAM arbitration.

## 6. Config model and migration

- **Three class-settings sections survive** (v1's two-engine collapse lost
  load-bearing fields): chat and aux keep `models_dir` (three distinct dirs
  drive HF download placement and the `target=chat|aux|audio` enums on
  `lmgw__gguf_files`/`lmgw__hf_add`/`lmgw__hf_set`) and `public_prefix`;
  audio keeps its engine fields (`backend`, `device`, `threads`,
  `lazy_load`). Per class: `image`, `models_dir`, `extra_run_args`,
  `public_prefix` (+ audio's extras).
- Deleted per-class fields: `container_name` (derived, §3.3), `listen_port`
  (dynamic), `models_max` (concept gone), `auto_start` (replaced by
  per-model `warm_start`).
- New global: `container_prefix`. New per-model columns: `image?`,
  `extra_run_args?`, `warm_start` (all three tables), NULL/false defaults
  via the normal SQL migration path.
- Settings are one JSON blob with no version field; the migration follows
  the existing precedents (serde alias / `json_set` rewrite). **Ordering:**
  the legacy container sweep (§3.4) reads the old `container_name` values
  *before* the shape migration drops them — sweep from the pre-migration
  JSON, then rewrite.
- The settings DTO surface (`lmgw__settings`, `/api/settings-full`,
  `RouterSettingsDto`/`AudioSettingsDto`, `deny_unknown_fields`) changes
  shape with the fields; that is a contract change to third-party callers
  and gets a release note, not a compat shim.
- SELinux posture is unchanged by design: mounts stay unlabeled and the
  class defaults keep `--security-opt label=disable`; per-model containers
  multiply mounts but not the labeling approach. (Never `:Z` on a broad
  path.)

## 7. Deleted outright

`vram/router_api.rs`; `PresetReloader`/`HttpReloader` + reload-vs-restart
fallback; `render_preset`/`render_aux_preset`/`push_*_as_ini`/`ini_value`
(the canonical-key dedup table survives in the argv renderer, §3.6);
`preset_in_sync*` / `config_in_sync_with`; multi-model `server.json`
rendering; `models_max`; `ModelState` + sleeping tier; `evict_audio` +
`note_container_reset`; router-port-based `classify()`; the aux/audio
catalog HTTP fetch; the persisted managed aux/audio upstream rows +
`ensure_aux_upstream` / `ensure_audio_upstream` (their names survive as
synthetic upstreams — §5).

## 8. Surfaces

- **`ops::container` / `lmgw__container` / `POST /api/op/container`**: the
  grammar splits — `target` keeps meaning a **group** (`all`, `chat`, `aux`,
  `audio`; `embed` stays an alias for `aux`), and a new optional **`model`**
  parameter addresses one model within the group. No overloading of `target`
  with model ids, so a model named `all` stays reachable. Group `apply` =
  recreate running members and report the per-model pre-flight
  (`models_enabled` / `models_with_problems` / `problems` — shape kept);
  per-model `apply`/`start`/`stop`/`restart`/`logs` act on one runtime.
  `stop` refuses on in-flight without `override` (§3.6).
- **Status**: `/api/status` + `lmgw__status` replace `containers:
  [chat,aux,audio]` with the per-model runtime list (class, state, port,
  measured VRAM, uptime, in-flight). `/api/vram`, the `vram` SSE frame and
  `VramResident` remain the *scheduler's* view and keep their per-model rows
  (the `container` field carries the class string) — one runtime list, one
  scheduler view, no third shape. The 5s inspect-poll loop generalizes over
  the registry.
- **UI**: the Overview "Backends" panel becomes the model-runtime table
  (per-row state/port/VRAM/uptime, start/stop/restart/logs; direct-URL and
  web-UI links move from the three fixed rows to per-model rows built from
  the registry port). The three-slot `ApplyState`/`ApplyBars` machinery and
  the app-global one-podman-command `busy` lock are replaced by per-model op
  state — one model's cold start must not freeze the whole dashboard.
  Settings forms lose `container_name`/`listen_port`/`models_max`/
  `auto_start`, gain `image` per class + global `container_prefix`.
  `local_edit`'s "Rendered preset" panel becomes "Rendered command line"
  (same honesty, new renderer); the wiring page's `preset_in_sync` indicator
  disappears with the concept. A per-model container-log drawer backs
  §3.6's logs verb.
- **`lmgw__local_model_test`**: goes through `admit()` like any request —
  the test now exercises the real load path including admission.

## 9. Testing

- The `CommandRunner` fake pattern survives and becomes the main harness:
  per-model `podman run` argv assertions replace INI-render assertions
  (switch-vs-value semantics, `--alias`, mount rewrites — §3.6). New fake
  surface: `podman ps --filter` / `inspect` output for reconciliation tests
  (adopt-vs-replace, port recovery, legacy sweep) — no precedent exists in
  the current fakes, it is new machinery.
- `vram_admission.rs`'s `World` reshapes: the fake runner flips per-model
  container states and a wiremock `/health` per model answers readiness; the
  admission-math tests port, the router-protocol tests die.
- Honest blast radius (v1 undersold it): `tests/it/aux_router.rs` (apply/restart
  flow), `tests/router_preset.rs` (INI + podman-args subsets),
  `tests/it/audio_backend.rs` (config_in_sync + args), parts of
  `tests/it/vram_admission.rs`, and `tests/it/router.rs::public_local_honors_prefix`
  (reads the moved `public_prefix`) all change. Untouched: the rest of
  `tests/it/router.rs` (pure resolve precedence) and `tests/it/e2e_proxy.rs`.
- New e2e-ish test: acquire-vs-reaper race (registry lock), and the
  connect-refused retry path.

## 10. Spike results (measured 2026-08-31)

Host: the RTX 4090 machine; image `localhost/llama-server-cuda:official-latest`
(entrypoint is a bash script running **nginx in front of llama-server** —
container port 8080 is nginx, llama-server sits on an internal port; this
shapes the health-probe behavior below). All seven pre-implementation
questions are answered; none blocks the design.

1. **SIGTERM** — direct-mode llama-server exits in **0.365s** on SIGTERM
   mid-generation (exit 0); SIGINT identical. Router mode is the odd one
   out: stopping the live routers needed SIGKILL after the full 10s grace.
   Default `podman stop -t 10` stays as a never-normally-reached ceiling.
2. **Cold start** — `podman run -d` returns in 0.3–0.5s; run→`/health` 200:
   **2.1s** for Qwen3.5-2B, **9.3s** for Qwen3.6-27B (IQ4_XS, 14.6 GiB).
   Router-mode child load of the same 2B measured 1.4–2.2s: container
   overhead is noise against weight upload, as assumed. Page-cache warmth
   made no measurable difference on the small model (upload-bound).
3. **`/health` semantics** — binary: unreachable until the model is up,
   then `200 {"status":"ok"}`; **no 503-loading phase exists**. Early
   probes can see a curl recv-error (56) rather than connection-refused
   (the nginx front-end accepts before its upstream is ready) — the poll
   treats every non-200 outcome identically.
4. **Port conflict** — `podman run -d` on an occupied port fails
   **synchronously**, exit 126, pasta stderr naming the port. It leaves the
   container object in `created` state (never started), so the retry
   removes the husk or relies on `--replace`. Do not key detection on the
   container's exit code — it reads 0 for a never-started container.
5. **NVML per-process** — works through rootless podman + CDI:
   `nvidia-smi --query-compute-apps` lists llama-server with per-process
   VRAM, and its PID is exactly the container-pid↔host-pid mapping `podman
   top` reports. Measured footprint: **5.1 GiB for a 1.3 GiB GGUF** at its
   configured context (KV cache + CUDA context dominate) — file-size
   estimates are the wrong order of magnitude, which is why the ledger
   measures (§4).
6. **Cross-model queueing** — the router shows **no head-of-line
   blocking**: a cold load completed in 1.75s while another model
   generated (vs 2.2s solo baseline); two concurrent generations
   interleaved fairly (finished 4.86s/4.88s vs 2.03s/2.97s solo — GPU
   contention, combined throughput ≈ sum); two simultaneous cold loads ran
   in parallel (1.40s/1.62s). The observed backend queueing is therefore
   lmgw-side — the gate held across load waits (§4) — not the router port.
7. **`/slots` in direct mode** — served **by default**, no flag needed;
   `--slots` changes nothing observable in this build; `is_processing`
   flips correctly under load. The §4 busy probe is viable as specced.
   *Addendum 2026-09-17 (`official-latest`):* an idle slot that has finished
   a task still reports that task's `id_task` (the server keeps `task_prev`
   and `to_json` falls back to it); only a never-used slot omits the key.
   The probe reads `is_processing` as the verdict and `id_task` only as a
   fallback when the flag is absent — reading a present id as busy had made
   every once-used model unevictable.

## 11. Out of scope

Merging the three model tables; per-GPU admission gates; engines beyond
llama.cpp/audio.cpp; remote podman hosts; any change to cloud-upstream
routing; a `public` column for aux/audio models; an alias mechanism for
local models (the upstream-FK route died with the managed rows — it would
be class+model keyed if ever wanted, §5).
