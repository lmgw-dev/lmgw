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
- *(2026-10-02, the per-row CPU switch)* An audio row may set `backend:
  cpu` and `threads` (migration 0056; `None` inherits the class). The
  descriptor's `audio_settings` are then the class's engine keys with the
  row's in effect (`runtime::audio::engine_settings`), and a CPU row that
  inherits the class's `extra_run_args` gets them without the GPU
  passthrough (`label=disable` kept; an override is used verbatim, and
  `local_model_get` and the editor warn when it still passes a GPU). The
  strip covers CDI and `--device` nodes, `--gpus`, GPU-node volumes and
  mounts, the NVIDIA runtime, CDI annotations and `NVIDIA_VISIBLE_DEVICES`,
  and adds `NVIDIA_VISIBLE_DEVICES=void` against the legacy NVIDIA OCI hook
  (the CUDA images set it to `all`); a passthrough configured some other way
  on the host is not seen. The live check of 2026-10-02 (dev copy) served
  real transcriptions from such a container with `HostConfig.Devices = []`
  and no GPU process for it.
  `ModelRuntime::placement()` (`Gpu | Cpu`) is recorded on the registry
  entry, and every VRAM and hold decision reads it: a CPU container is
  charged nothing, never evicted, and not stopped by the GPU hold. A row
  that sets neither renders the `server.json` it rendered before.

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
  `lmgw.class=<class>`, `lmgw.model=<model_id>`, `lmgw.engine=<engine>`, and
  `lmgw.run_args=<digest>` of the `extra_run_args` it was started with
  (2026-10-02). Reconciliation filters on the instance label, never on name
  patterns.

### 3.4 Reconciliation, deletion, shutdown

Boot: `podman ps -a --filter label=lmgw.instance=<prefix>` →

- Running container whose inspected `Cmd` matches the currently rendered
  argv, whose `lmgw.run_args` label matches the run args rendered now (podman
  normalizes the flags themselves, so they are compared through the label; a
  container without one is from an older lmgw and is not adopted), **and**
  whose model still exists and is enabled: adopt (recover host
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
path removes the husk (or leans on `--replace`) before rerunning. One
retry, only for the port conflict (`registry::PortRetry`); since
2026-10-02 an agent's service container and a benchmark's container take
the same retry, as both publish an ephemeral port too.

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
- **A model that would work is never refused; one that fits only partially
  starts with a warning** (the owner's ruling of 2026-11-11: a model that would work is never blocked; one that fits only partially starts with a warning. A start must
  also not push other residents into OOM; where lmgw does not know a figure
  it charges conservatively; it refuses only what really cannot work).
  Today that is an image pipeline that can run with less than all of it on
  the card (`vram/plan/image.rs`): `offload_to_cpu`, a `params_backend`
  naming `cpu` or `disk` (for every module or some), or sd.cpp's
  `--auto-fit`, which is on by default and off only with `auto_fit off` or
  an explicit `params_backend`. Such a pipeline is charged its files, as
  every pipeline is, wherever they fit after the evictions any start makes;
  its largest file is no upper bound for what a generation takes (measured
  with `offload_to_cpu`: a 3.6 GiB largest file, a 7.7 GiB peak). Only where
  the whole does not fit does admission give it less (`vram/partial.rs`),
  and only where its engine sees exactly one card (`partial::cards_seen`:
  the cards its resolved run args name, else every device the last ledger
  read listed; a native Vulkan build sees every card whatever the run args
  name). `--max-vram` is a per-device budget while the ledger pools the
  devices, so on several cards sd-server could draw the budget on each:
  such a row takes the whole-pipeline path, and its refusal or wait line
  says partial fitting on several cards is not done yet. A row whose args
  set its own `max_vram` gets that budget plus its context where that much
  is free beyond the headroom, else the normal path; one whose own
  `max_vram` is 0 or less follows the driver's free memory, which bounds
  nothing, and needs the whole (its refusal and `local_model_get` say
  why). Every other row gets:
  1. its **on-card part** where that fits — for a row whose weights all
     wait off the card its context plus its learned peak, before the peak
     its largest file plus its context; for any other row all of it;
  2. else what is free beyond the headroom, when the budget that leaves
     (that less the context) is at least the row's **working budget**: the
     smallest `--max-vram` a generation of the row has completed under
     (`image_models.work_bytes`, migration 0104, learned by
     `vram/partial/work.rs` when a generation on a held entry completes;
     lowered by each smaller one that works);
  3. else — no working budget learned yet, or less than it — what is free,
     only when nothing else of lmgw's is on the card or reserved for it and
     no fallback would answer. Otherwise the start takes the normal path: it
     waits, drains or evicts, and a configured fallback answers. A measured
     context that leaves no budget at all never starts.
  The **context** is sd-server's own memory outside its budget: the row's
  learned idle residency (`image_models.idle_bytes`, migration 0103), what
  the container of a row whose weights all wait off the card holds at rest,
  read per process (`vram/partial/floor.rs`) — once per container, not
  while a claim was taken during the reading (a per-entry claims counter)
  and not when the row's files, args, engine image or run args changed
  meanwhile. Auto-fit and per-module rows have no context measured yet:
  their idle residency holds whatever weights auto-fit placed, and no
  reading that separates sd-server's context from them exists yet, so the
  stand-in and its "not measured yet" stay for them. Where it is not measured,
  `CONTEXT_STAND_IN` is set aside for it: 1.0 GiB, the most a context has
  been measured holding (CUDA, `offload_to_cpu`, the 24 GiB test card,
  image-generation §12.4), said in every sentence that uses it. The context
  is no working budget, which is why the working budget is a figure of its
  own: on a Windows test card the Vulkan context is 5 MiB, and a budget just above
  it does not render (table below).

  A partial start is **held to what it was given**: sd.cpp's auto-fit and
  its weight streaming otherwise size themselves to the driver's live free
  memory, which includes the headroom, other pipelines' kept-free peaks,
  audio's pending loads and a starting model's weights not loaded yet.
  Admission stages the grant for the registry entry the start creates
  (`registry/admission_grant.rs`); the entry renders it into every launch
  as an absolute `--max-vram` (sd-server's per-device budget in GiB for
  managed weights and runner buffers; sd.cpp parses it as a float with
  `parse_strict_float` at the pinned `master-1079-a3c49b1` and at
  `c678dfe`; 0 and negative values follow the live free figure, so lmgw
  renders the thousandth, never 0), less the context — measured, else the
  stand-in where the grant leaves room for it, else the whole grant with the
  context on top, said in the warning. The flag binds whatever placed the
  weights: sd.cpp's model manager checks every runner's request against it,
  so `offload_to_cpu` and an explicit `params_backend` are held as auto-fit
  is (the help's "during automatic graph-cut execution" undersells it). A
  `max_vram` in the row's args wins at render, which is why admission gives
  such a row that budget (above); where the row's value is not what
  admission gave (an edit after the decision) the entry's warnings say it
  is not held to its grant, and a build without the flag is not held and
  says that too. The container carries the grant as the label
  `lmgw.grant=<granted>:<max_vram>` (bytes), so boot reconciliation renders
  the expected command with that `--max-vram`, adopts the container instead
  of removing it, and the adopted entry keeps the grant and its charge,
  with a warning worded by what holds sd-server (the grant's flag, the
  row's own `max_vram`, or nothing in a build without the flag) and, where
  that is not the grant, the not-held note restored from the label and the
  row, so it teaches no working budget.

  The ledger charges a partial entry its grant, or its budget plus its
  context where that is more (the row's own `max_vram` as the budget where
  it sets one; the context measured, else read at rest for a row whose
  weights all wait off the card, else the stand-in). A whole start of a row
  whose weights all wait off the card is charged, once its peak is
  learned, the peak plus its context — measured, else what its processes
  hold at rest (a reading while a generation runs holds the peak too, and
  is not taken as the context) — and with neither its files plus its peak, what admission
  charged before it adapted (budget-only mode and a probe without per-process
  figures never learn a context, so that stays). Before the peak it is
  charged its files. In measured mode what its processes hold now comes off
  what is kept free. A container held to a grant teaches no peak: what it
  draws is what its grant allows.

  Measured 2026-11-11 on a Windows test VM (a GTX 970 test card, 4 GiB, a 560-series driver), sd-server of sd.cpp `master-1079-a3c49b1` (the pinned Vulkan
  build), Z-Image-Turbo Q4_K with `ae.safetensors` and Qwen3-4B-Instruct
  Q4_K_M as the text encoder, `--steps 8 --cfg-scale 1`, one 512×512
  generation per case, the card's `memory.used` from `nvidia-smi` every
  100 ms less the 154 MiB it showed with nothing running (WDDM lists no
  per-process figures there):

  | Case | At rest | During the generation | Time |
  |---|---|---|---|
  | `--offload-to-cpu`, no `--max-vram` | 5 MiB | 3110 MiB (all that was free) | 146 s |
  | `--offload-to-cpu --max-vram 1` | 5 MiB | 1010 MiB | 150 s |
  | `--offload-to-cpu --max-vram 2` | 5 MiB | 2038 MiB | 146 s |
  | auto-fit (no offload) `--max-vram 2` | 5 MiB | 2038 MiB | 146 s |
  | `--offload-to-cpu --max-vram 0.5` | 5 MiB | fails: "need 1453.98 MB device / 941.98 MB budget", first segment | — |

  Auto-fit with a 2 GiB budget put every module's weights in RAM (its plan:
  `--params-backend "diffusion=cpu,te=cpu,vae=cpu"`). Reproduce with
  `scripts/sdcpp-max-vram-probe.ps1` on a Windows test machine, one case per run with
  nothing else on the card (its usage line names the parameters: the
  engine's `sd-server.exe`, the models dir, a scratch dir under the
  checkout's `target\`, a loopback port in 18900–18999); the CUDA context of
  1.0 GiB is image-generation §12.4's.

  The sentence: where the whole does not fit, the start goes ahead with "It
  will work but only fits partially on the GPU, expect slowdowns: …" naming
  the whole, the headroom, what is free, the setting, what it was given
  (and against which known budget) and the `--max-vram` it is held to with
  its context. It is logged and kept on the registry entry from its claim
  on, through `ready` and every launch of that entry (the Models page's
  warning chip, `lmgw__status`, the start/restart answer's
  `ModelDone.warnings`, the image test's `ImageTestPassed.warnings`);
  `local_model_get` says it of the row against the card's total
  (`ImageModelDetail.warnings`). Refused, with numbers, is what cannot run:
  a model all on the card that is larger than the card (an image row that
  pins its weights is told which setting pins it and which would let it run
  with less), a measured context that leaves no budget, and nothing beyond
  the headroom free.

  **A failed generation teaches too** (`vram/partial/work.rs`): a 5xx from
  sd-server on an entry held to a grant whose budget was at least the
  row's working one shows that figure is not enough for every generation
  (a budget learned from a 512² picture need not hold for a 1024² one).
  The figure is raised to the budget sd.cpp names — its model manager's
  "need … MB device / … MB budget" or its graph-cut split's "segment …
  needs … MB on …" (MB of 1024²), read from the error and the container's
  last 200 log lines — so a start needs that plus its context free; where
  sd.cpp names none, the figure is cleared and the rule for a row with none
  learned applies until a generation completes again. A failure under less
  than the working budget teaches nothing new.

  **Learned figures belong to an engine** (migration 0105,
  `image_models.learned_key`): the peak, the context and the working
  budget are keyed by the resolved engine image, run args and native build
  (`vram::plan::image::engine_key`, the native launcher's variant choice
  with its failed-CUDA marks on Windows). Each container records the key it
  started with (after a fallback, the build it fell back to) and teaches
  its figures under it; a figure learned under another key clears the
  others. Admission reads a row's figures only under the key the row
  resolves to now (a running container's under its own), so a class image
  switched from Vulkan to CUDA, or Windows choosing CUDA again, counts them
  as not measured and charges conservatively until they are learned again;
  switching back finds them. Figures learned before 0105 carry no key and
  are learned again. They are still reset when the row's files or args
  change.

  Not done yet: the grant is the entry's for its life, so a pipeline
  squeezed once stays squeezed and warned after the card frees up, until
  its container stops (no re-grant on a later idle relaunch yet); partial
  fitting on several cards (per-device budgets); the working budget is the
  row's, not per size, so a figure raised by a large picture holds for the
  small ones too. Not counted as partial yet: an aux row's partial
  `--n-gpu-layers` and llama.cpp's `--fit` on an aux row, which admission
  charges in full, and an image row's `--backend` assignments (`vae=cpu`) on
  a row with auto-fit off.
- **A llama-server row is charged what lands on the card, and where
  llama.cpp's `--fit` would shrink it, lmgw fits it instead** (the same
  ruling, 2026-11-11; `vram/plan/chat/fit.rs`, `vram/partial/llama.rs`).
  - *A partial `--n-gpu-layers N`* (0 < N ≤ the layer count, field or args)
    is charged its share: llama.cpp counts the output layer as a layer and
    fills from the back (`i_gpu_start = n_layer + 1 − N`, b12244), so the
    first `n_layer + 1 − N` layers' weights stay in host memory — summed from
    the GGUF's tensor list as a lowest-priority CPU override, so an `-ot` to
    a device still wins — and so does their share of the KV cache and
    recurrent state (each layer's cache sits with the layer). The compute
    buffer is charged in full, with op offload's copy of the largest
    host-kept weight on top. `--no-kv-offload` (`-nkvo`) charges no KV cache.
    Such a row is held to what it says: it waits, evicts or falls back like
    any model, never shrunk.
  - *`--fit`* (on by default in b12244, `common/fit.cpp`) reads each
    device's free memory as the driver reports it at load and keeps
    `--fit-target` free on it (default 1024 MiB per device). It lowers an
    unset context from the trained one, no lower than `--fit-ctx` (4096) per
    stream in steps of 256, then — only with `--n-gpu-layers` on auto and no
    tensor override of the row's — moves a mixture of experts' routed
    experts and then whole layers off the card. A value the row sets is one
    it leaves alone. `--fit-target` is a margin, not a budget: the driver's
    free figure includes the headroom, the peaks the ledger keeps free for
    pipelines and the weights of models still loading, so a margin cannot
    hold llama-server to what admission gave it. lmgw therefore makes the
    same choices against its own figures, in the same order (context first,
    down to the floor; then `--n-cpu-moe` layer by layer for a mixture of
    experts; then whole layers), and renders them as explicit `--ctx-size`,
    `--n-cpu-moe` and `--n-gpu-layers` with `--fit off` (the grant's
    `LlamaPlacement`, `runtime/argv/llama_fit.rs`), so `--fit` has nothing
    left to adjust and the start skips its dry-run load. Admission's charge
    and what llama-server is told are then the same placement. A row the
    card holds as written gets nothing rendered over it, and llama.cpp's
    own `--fit` runs at its start as it always did. Where admission keeps
    less than `--fit-target` free beside such a row, llama.cpp may still
    lower its context a little (it then draws less than it is charged);
    that is not said anywhere yet.
  - *Only where the image has `--fit`* (`vram/fit_vocab.rs`). ik_llama.cpp
    and llama.cpp builds older than `--fit` load a row as written, so a row
    is fitted only where its image's `--help` lists `--fit` (`llama_caps`,
    read by every admission decision of such a row and by the model
    editor's checks, cached per image ID). Anywhere else, and until the
    flags were read, it is charged as written with no fit flag rendered,
    and its footprint note says why. The native launcher's own llama.cpp
    build (Windows) counts as having it: its `--help` is not read yet, and
    the build lmgw installs (b12244 on a Windows test VM) has it.
  - *The row's own `--fit-target`* (`-fitt`, MiB per device; the largest
    on the one card) is what it asks to keep free beside it, and `--fit
    off` would leave llama.cpp nothing to keep it with: admission keeps it
    instead where it is more than the headroom, in the target, in the
    start's needs and in the "too large" judgement, and the note says so.
  - *What it is charged.* Where what the card can give it now cannot hold
    the row as written beside its margin (a trained context of 262144
    tokens on a 4 GiB card), it is charged its **target**, the most of it
    that fits there, and that is what a start makes room for, evicting idle
    models as any start does. What the card can give it now is its total
    less what other programs hold there now, with lmgw's own models counted
    as room, since a wait or an eviction frees them: each decision reads it
    off its own ledger (what is free, plus what lmgw's other models on the
    card are charged with their kept-free peaks) and fits the row again on
    every pass of its wait, so the target and the admission check use the
    same figures and a target fitted on a card that holds only other
    programs' memory is admitted. The learned growth never shrinks the
    target: it only makes the start wait for or evict lmgw's own models,
    and with none left the start goes ahead without it, warned ("Headroom",
    below). The target is what the card can give the row at all now, so it
    goes ahead as an image pipeline's on-card part does, a fallback or not
    ("Context fitted to the GPU: …", or "It will work but only fits
    partially on the GPU, expect slowdowns: …" with layers off the card,
    naming the whole, the headroom, what is free, what it was given and the
    flags). *Fixed 2026-11-11:* the target
    had been fitted to the card less the 7-day minimum of other programs'
    use and the learned growth, then admitted against the driver's free
    memory now; whenever other programs held more than their minimum (nearly
    always) it never fitted, so a live fallback answered for good, or every
    idle model was evicted and the row started below its target, and a
    growth taught by a game lowered the context for the container's life.
    The footprint preview outside a decision (`local_model_get`) fits the
    row to the card less other programs' use at the newest reading, else at
    rest.
  - *Less than its target* lasts for the container's whole life, so it is
    given only where it is the only way the model runs now: nothing more
    can be freed, nothing else of lmgw's is on the card or reserved for it
    (busy residents finish, drain or are evicted, and the start waits for
    them as any start does), and no fallback would answer in its place (a
    configured fallback always applies). §4.7's verdict judges a fitted row
    by its least placement and its margin against the card less what other
    programs hold now: wherever that least fits, a target does, so only a
    shortfall in other programs' use now sends the request to the fallback.
    A guest's deferral is its fallback, so a background start
    never shrinks a row below its target either. Then it goes ahead with
    the most that fits in what is free beyond the headroom. It waits while
    not even its least placement (the floor context and, where layers may
    move, the output layer alone on the card, every routed expert in host
    memory) fits what is free, and is refused only where that least does
    not fit the card beside what other programs hold at rest and the margin
    (the headroom's structural part, below), with what it needs ("Even the
    least llama.cpp's --fit would run it with (…) needs …"). Both warnings stay on
    the registry entry and in the start answer. A row whose engine sees
    more than one card is charged as written: llama.cpp splits layers by
    each card's free memory while the ledger pools the cards, so fitting on
    several cards is not done yet.
  - *Measured 2026-11-11* on a Windows test VM (a GTX 970 test card, 4 GiB, a 560-series driver), llama.cpp `b12244` CUDA 12 as lmgw's engine install fetched
    it, a debug lmgw (the headless runner, a scratch data dir), fixed
    headroom 1024 MiB, the desktop at 130 MiB. Qwen3-4B-Instruct-2507 Q4_K_M
    (36 layers, trained on 262144 tokens), one row alone on the card at a
    time; "drawn" is `nvidia-smi`'s used memory once the start settled less
    the 130 MiB before it (lmgw's D3DKMT share agreed within 1 MiB):

    | Row | Before this change | Now: charged | Told | Drawn |
    |---|---|---|---|---|
    | no context, layers on auto (`--fit` row) | refused: 38.7 GiB at the trained context | 2827 MiB, the warning | `--ctx-size 4096 --n-gpu-layers 34 --fit off` | 2876 MiB |
    | `ctx 8192`, `--n-gpu-layers 20` | charged in full (3.5 GiB), refused | 2119 MiB | as written | 2172 MiB |
    | `ctx 8192`, `--n-gpu-layers 999` | refused | refused: 3.5 GiB plus 1.0 GiB headroom on a 4.0 GiB card | — | — |

    The `--fit` row's target on that card was every layer at 4096 tokens
    (3.0 GiB); with 3.8 GiB free less the headroom it got 33 of its 36
    layers and the output layer. Both started rows answered a chat request.
    The gap between charge and draw (49 and 53 MiB) is the CUDA context and
    allocator slack the headroom covers. Auto headroom on that test VM after the
    run: "using the fixed 1.0 GiB until lmgw has watched the graphics card
    for 24 hours (1 so far; other programs grew by up to 1.2 MiB so far)".
    Reproduce with `scripts/llama-fit-check.py` (its usage line: an ssh port
    forward to lmgw's bind address, the owner token, the machine's models
    dir).
  - *Measured on Linux 2026-11-11*: a 24 GiB test GPU (driver 600.20.03), image
    `localhost/lmgw-llama-server:official-master`, a scratch dev instance
    (`scripts/dev-instance.sh`, its own container prefix) with
    `vram.budget_mb` 8192 so another lmgw instance kept the rest of the card,
    fixed headroom 1024 MiB, the desktop at 2.1 GiB. Qwen3-VL-4B-Instruct
    Q4_K_M as text (36 layers, trained on 262144 tokens); "held" is
    `nvidia-smi`'s per-process figure for llama-server:

    | Row | Charged | Told | Held after start | Held after the first request |
    |---|---|---|---|---|
    | no context, layers on auto (`--fit` row) | 5032 MiB | `--ctx-size 18176 --fit off` | 5516 MiB | 5516 MiB |
    | `ctx 8192`, `--n-gpu-layers 20` | 2119 MiB | as written | 2576 MiB | 2576 MiB |

    The gap is 484 and 457 MiB, the same for a 2.1 and a 4.9 GiB charge and
    not grown by a short first request: llama-server's CUDA context and
    allocator slack, which Linux's per-process figure counts and WDDM's
    (49 and 53 MiB on the test VM) barely does. That is the remainder the
    estimate leaves to the headroom by design (`vram/plan.rs`: "the CUDA
    context, ~450 MiB per container as measured"), and the 807 MiB margin's
    source gap includes it. It is not added to the estimate yet: doing so
    moves every chat row's charge on Linux by about 0.5 GiB and would count
    it twice beside the margin, so it waits for a decision. Meanwhile
    a row fitted beside the 1 GiB fixed headroom on Linux leaves about
    0.5 GiB of it for other programs' growth. Reproduce with the script's
    second usage line (`--rows fit,ngl --file <gguf>`, no `--ssh`).
    The first run also found that a footprint fitted before any device read
    (a declared budget makes the total known first) dropped its sizer for
    good and refused the row's first start as too large; fixed, with a test.
  - *Adopted at boot.* A fitted start's container carries its placement
    and charge (`lmgw.llama-fit=<bytes>:ctx=…,n-cpu-moe=…,n-gpu-layers=…,
    fit=off`). Boot renders the expected command with it — only the values
    the row still leaves to fitting, so a row since edited to set its own
    context or layers is not adopted with the stale ones — adopts the
    container with its grant, and words its warning for the adoption. (An earlier version wrote no label, so boot found a command the row does not
    render and removed every fitted container on each start of lmgw.)
  - *The command preview* (`local_model_get`'s `command_line`) is the
    running container's own where admission fitted it, else what a start on
    the card as last read is told, and an advisory says which and that the
    placement is decided at each start.
  - Not done yet: learned figures for llama-server rows (a ready container
    is measured by the driver, so this matters only for the start); an aux
    row's partial `--n-gpu-layers` and `--fit`; ladder rungs, which are
    charged as written; reading the native build's own `--help`; the target
    is the entry's for its life, as an image grant is.
- **Headroom, auto or fixed** (approved by the owner 2026-11-11;
  `vram/headroom.rs`). The headroom did two jobs as one fixed number
  (`vram.headroom_mb`, 1024 MiB): (a) room for other programs' GPU memory
  to grow after a start, and (b) slack for lmgw's own estimates until a
  model is measured. `vram.headroom_mode` is `auto` (the default) or
  `fixed` (the old behaviour, `headroom_mb` for both).
  - *(a) Growth of outside use.* What other programs hold now is already
    out of the driver's free figure, so only its growth matters. A sampler
    reads the per-process attribution (§4.7's: used memory less what lmgw's
    containers hold) every 30 s, where it can — admission on, boot settled,
    per-process figures, no start in flight — and keeps per hour the lowest
    and highest reading and the largest rise within the hour (migration
    0106, `vram_outside_hours`). Auto's headroom is the largest rise over the
    last 7 days: the most any reading exceeded an earlier one, across hours.
    A rise shorter than 30 s is not seen. Pooled over the devices, as the
    ledger pools them; per-device growth is not learned yet. No reading is
    taken while the GPU hold or a benchmark's lease keeps lmgw's models off
    the card: whatever has the card then would read as growth of the
    desktop's (a 19 GiB job under the hold would teach 19 GiB and refuse models for a week).
  - *(b) A margin while a model starts or is not measured yet*, the class's
    largest recorded gap between estimate and process: llama.cpp (chat, aux)
    807 MiB (decision-models design, Appendix C's "estimate against the
    process" table, Qwen3.8-Flash-Next at `--n-cpu-moe 43`, ubatch 1024,
    2026-11-09); sd.cpp 1.0 GiB (`CONTEXT_STAND_IN`; none for a partial
    start, whose grant sets its context aside, nor for a row whose context
    is measured); audio.cpp as much as its estimate, at least 0.78 GiB
    (residency about twice the files, pocket-tts 0.90 GiB held against
    0.12 GiB on disk, realtime design §9.4) until its residency is learned.
    It rides on the start being admitted, on a starting entry's and a
    reservation's charge, and on an unlearned audio row's pending load. A
    llama-server keeps it after `ready` until a request that runs a prompt
    or encodes has run on it: the claim of a chat, completion, responses,
    embedding, rerank or decision request it answered is let go, or a
    ladder send ends (the CUDA pool's temporaries in the 807 MiB arrive with
    the first prompt). A token count (`/tokenize`, a native count) runs no
    prompt and leaves it (`vram::send_local_count`; *fixed 2026-11-11*, any
    answered send had cleared it). The ledger's note on it says so. A
    climb's new container keeps it until it serves one. *An adopted
    container* (boot, or a reconciliation pass, after every lmgw restart)
    has served an unknown number of requests under the earlier lmgw:
    llama.cpp rows learn no footprint of their own yet, so the one
    measurement it can have is a per-process reading of what it holds.
    Where the driver gives per-process figures, the ledger reads it (as it
    reads an audio or off-card image container) and a reading is its
    measurement: no margin. Without such figures (a driver with no
    per-process list, PIDs not read yet) nothing of it was ever measured,
    and it keeps the margin until it serves a request. *Fixed 2026-11-11:*
    every adopted chat and aux container had been charged the 807 MiB after every lmgw start until its first request, which an idle embedding or decision
    row may not see for days. The margin is dropped then, or once an audio
    row's residency is learned.
  - *Too large is judged without the growth* (2026-11-11, under the owner's rules). Whether a model can ever run is
    the card's total less what other programs hold at rest — the lowest
    reading of the window — and the margin. The learned growth never
    refuses a model as too large. In `fixed` mode the fixed value does
    both, as before.
  - *Learned growth decides waiting* (approved by the owner 2026-11-11: it decides
    only wait, evict or fall back, never whether a model can run at all,
    and it must not shrink a container for good). A start waits for, and
    evicts, lmgw's own models until its footprint, the growth and its
    margin are free. Once nothing of lmgw's is left on the card to wait for
    or evict (after the reconciliation pass has looked for containers lmgw
    lost track of), a start that fits beside what other programs hold now
    with its margin alone goes ahead with a warning that names the growth
    and when it was seen ("Learned growth not kept free: other programs'
    GPU memory grew by up to 200 MiB on 2026-01-15 14:00 (auto headroom),
    which VRAM admission keeps free beside a start while it can wait for or
    evict lmgw's own models. None of lmgw's is left on the card, and
    chat/… fits beside what other programs hold now with its 807 MiB margin
    (…), so it starts without that room: should other programs grow that
    much again, the GPU can run out of memory"). The warning stays on the
    registry entry for its life (a whole start's on the entry's
    `admission_note`, a partial one's in its grant). This holds for a
    request's start, an operator or warm start, a guest's start (whose
    guest may evict only background models, so the owner's still count as
    something to wait for: it defers) and an owner's ladder climb (logged).
    The growth never sends a request to its fallback: §4.7's verdict, and a
    climb's, ask the card for the start and its margin only, so a fallback
    answers only for a shortfall in other programs' use now, as before.
    Such a shortfall still waits where there is no fallback, and its
    timeout names what holds the card. A fitted llama-server row's target
    is fitted beside its margin, never the growth (above). *Fixed 2026-11-11:* the wait had needed the
    growth free once too-large stopped counting it, so a row that cannot
    adapt (explicit context, image, audio) whose footprint fits beside
    other programs' use now with its margin but not with the growth too
    evicted every lmgw model, then timed out after the queue timeout with
    "another process is using the GPU", or waited for ever with no queue
    timeout. A guest's ladder climb is still denied where the growth is
    not free (it defers; not changed yet).
  - *Its source is named, and it can be forgotten* (the same decision).
    The sentence says when the largest growth was seen ("other programs
    grew by up to 4.0 GiB, on 2026-01-15 14:00") and every figure behind
    it — a reading every 30 s, the last 7 days, used once 24 hours have
    readings, the margins (807 MiB for llama.cpp, 1.0 GiB for an image
    model, at least 799 MiB for a voice model) and the at-rest figure
    whether a model can run is judged by — so a game that taught it is
    plain to see. `headroom_now` carries the same as `learned_at`,
    `at_rest_bytes`, `sample_seconds`, `window_days`, `hours_needed` and
    `margins`. Settings (GPU → Admission → Learned growth) has "Forget
    learned growth…", which confirms and then deletes every reading (owner
    op `headroom_forget`); auto uses the fixed value until it has learned
    again.
  - *Until enough is learned* — fewer than 24 hours of the window with
    readings, or no per-process figures — auto uses `headroom_mb` for both
    jobs with no margin on top, and Settings says so ("Headroom: auto, using
    the fixed 1.0 GiB until lmgw has watched the graphics card for 24 hours
    (3 so far; other programs grew by up to 12 MiB so far)"). Learned:
    "Headroom: auto, 640 MiB: other programs grew by up to that much, on 2026-01-15 14:00 (a reading every 30 s, the largest rise over the last 7
    days, used once 24 hours have readings). A model that is starting or not
    measured yet gets a margin on top: … Whether a model can run at all is
    judged without the growth: against the card less what other programs
    hold at rest (130 MiB) and that margin. The growth only makes a start
    wait for or evict lmgw's own models: with none of them left, a start
    that fits beside what other programs hold now with its margin goes
    ahead, with a warning that names the growth". The settings page (GPU → Admission) has the mode, the fixed
    value and that sentence; `GET /api/settings-full` carries it as
    `headroom_now`, `lmgw__settings` as `vram.headroom`, and
    `lmgw__settings_set` takes `headroom_mode` and `headroom_mb`.

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

Host: a 24 GB GPU machine; image `localhost/llama-server-cuda:official-latest`
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
