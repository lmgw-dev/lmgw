# GPU hold — design (2026-09-04)

Successor note to the per-model-containers design (2026-08-30), §4 and §8.
Revised after adversarial review (same day); the review's findings are folded
in and marked *(rev)* where they changed a decision.

## 1. Summary

A manually toggled **hold** that takes lmgw off the GPU without taking the
gateway down. While hold is active:

- Every request that would need a local container is either **re-routed to a
  fallback alias** (a cloud model) or **refused with a 503** whose body names
  the hold. Cloud routes are untouched.
- Every resident local container is **stopped**: idle ones the moment hold
  engages, busy ones as soon as they drain. Nothing new is started — not by a
  request, not by a warm start, not by an operator action.
- The state is **visible everywhere**: tray icon + checkable menu item,
  dashboard titlebar badge, `GET /api/vram`, the `vram` SSE frame,
  `lmgw__status`, `lmgw__settings`.

The reason it exists: the owner games on the same GPU. Today the only way to
keep lmgw off the card is to quit it, and every pipeline pointed at it then
fails on connection refused instead of on a status code it could handle.

Hold is **orthogonal to admission control**. `vram.enabled = false` (or no
NVML) leaves admission inactive and hold still works — it is a switch on the
resolve and start paths, not a capacity figure.

## 2. Semantics, precisely

| Situation while hold is active | Outcome |
|---|---|
| Request resolves to a cloud alias | Forwarded unchanged. |
| Request resolves to a local model with a usable fallback | The **resolve step itself** returns the fallback's route (§4); everything downstream — protocol checks, passthrough fields, egress — sees a cloud route exactly as if the client had asked for the fallback alias. Response carries `x-lmgw-fallback: <alias>`; body `model` stays the requested alias (already how `serialize_completion` works); request log: `requested_alias` = original, `upstream_*` = fallback's. |
| Request resolves to a local model, no usable fallback | `503`, error kind `gpu_hold`, OpenAI type `api_error`, Anthropic type `api_error` *(rev: not `overloaded_error` — that is the Anthropic SDKs' retry-with-backoff signal, and a hold can last an hour)*. Returned at resolve time, before any stream byte, on every ingress. |
| Local model is still resident and busy (draining) | New requests to it are **still** refused/fallback — hold means no new GPU work, residency is not a loophole. |
| A join waiting on a start in flight when the hold comes on (a candidate alias's join, a realtime session's warm load of a lazy row, 2026-10-03) | **Refused once the start is up.** The join asks the hold when it begins and again after it got its claim, by where the claimed container computes; the claim is let go, so no warm-up sends a lazy model the first request that would load its weights. A request already admitted and parked on the same start is admitted work, and the sweep drains it. |
| Unattended batch jobs: quickdoc ingest extraction, golden-query generation | **Refused, never fallback** *(rev)*. Nobody is waiting on them, they can be re-run after the hold, and an ingest's embedding step cannot fall back anyway (§2 lookup order). The job row carries the `gpu_hold` error. |
| quickdoc's in-process embedder / reranker (`InProcessEmbedder`, `InProcessReranker`) | **Refused** *(rev 2)*, even though they ride the request-path helpers (`embed_in_process`, `rerank_in_process`) that *do* fall back. A corpus pins `(upstream, model, dims)`, and vectors from a fallback of the same width pass every check and land in the index unsearchable; a rerank through another model reorders answers the trace attributes elsewhere. Refused **before** the call (so no cloud tokens are spent on vectors that are then discarded) and again on the returned `Option<String>` fallback, which is the backstop nothing can bypass. The construction-time `gate` keeps plain `resolve` so the pin cannot move. |
| Interactive in-process paths: dashboard chat, agent chat, `/v1/responses`, MCP sampling, workflow classification | Fall back like HTTP requests — someone is waiting on each. |
| Warm start at boot, `lmgw__container start` / `restart`, dashboard start | Refused **before anything is stopped** *(rev)*, with a message naming the hold (`Fit::Held`). `restart` is refused up front — it would only ever stop. |
| `lmgw__container apply` on a running model | Stops it (hold wants it stopped anyway) and says the new configuration takes effect at the first start after the hold is released. *(rev 2)* Only a stop that **succeeded** is reported that way — per-model and group alike. A failed stop under a hold is reported as the failure it is: without a hold the `start_model` after it is the recovery, but under one nothing starts and nothing retries, so "was stopped" would be false and the container is still on the card. |
| `stop` | Works as today. |
| `lmgw__local_model_test` | Refused before admission: it resolves through `chat_local_route`, which does no row check and no fallback, so a hold-time test would otherwise start a container. |
| Dead-container recovery (`LocalHold::recover`) | Refused: the restart goes through `admit_local`, which holds. The runner gets the 502 "could not be restarted: …" it already knows. |
| An agent loop / `responses` chain / direct-port client keeps a model busy for an hour | It keeps the GPU for that long. Hold never kills work. The model is listed as *draining* the whole time (§6), and "Stop all models" in the tray is still the axe. |
| lmgw restarts with hold persisted | Reconciliation adopts whatever podman still runs (`Ready`, `in_flight = 0`), the hold sweep stops it, warm starts are skipped. |
| Audio row on the CPU (`backend: cpu`, the per-row CPU switch, 2026-10-02) | **Served, not swept.** It uses no VRAM, so the hold has no claim on it: its requests are answered by its own container (its `hold_fallback` goes unused), the sweep leaves it running and names it in `kept_on_cpu`, a boot under the hold warm-starts it, and container verbs act on it. Decided per model by `Snapshot::gpu_block_for` (a benchmark's lease still covers it). A container started on the GPU before its row was switched is held until it stops: its requests go to the row's hold fallback when it has a usable one, are refused otherwise, and no wait, join or realtime warm load may put work on it (checks after a wait go by the descriptor being started, not the row as it reads now). |

"Usable fallback" = an alias that `Snapshot::resolve` accepts **and** whose
route `classify()`s as non-local. A fallback that is itself local, or does not
resolve, is a 503 whose message says exactly that — a misconfiguration is
surfaced, never silently downgraded to a refusal.

*Changed 2026-10-06* (the owner's ruling: a configured fallback is always used,
with no exception by content; only capability keeps content from a route): the
one thing downstream that treats the fallback's route other than as if the
client had named it is a request's images. A fallback whose exposed
capabilities say `vision: false` gets them as placeholders ("omitted: the
answering model cannot see images"), with a WARN naming it, instead of images
its provider would refuse (`gate::fallback_images`; the route says it is a
fallback, `Route::fallback`). `x-lmgw-fallback` names who answered and
`x-lmgw-images-omitted` how many images it did not get; the Chat's reply says
it in `images_note` and sends a PDF's pages as its text. A fallback that
sees, or whose vision is unknown, gets the images.

*(rev)* Known limit: on a deployment with a **bare** `expose_all` upstream,
`resolve_passthrough` accepts any name, so a typo'd fallback resolves at set
time and at request time and fails at the provider with the provider's own
error. Validation (§3.3) cannot detect that; the docs say so.

**Fallback lookup order** for a held local model of class C and id M:

1. The row's own `hold_fallback_mode`:
   `inherit` → step 2; `none` → **no fallback** (refuse even if a global
   exists); `alias` → the row's `hold_fallback`.
2. If inheriting and C is **Chat** → `settings.hold.fallback_alias` (when set).
3. Otherwise no fallback.

Aux (embed/rerank) and audio models never inherit the global: a different
embedding model silently corrupts a vector index, so those only fall back when
their own row says so. (Owner's decision, 2026-09-04.)

## 3. Config model

### 3.1 Settings

New top-level block, `#[serde(default)]` so existing blobs load unchanged:

```rust
pub struct HoldSettings {
    /// The switch. Persisted, so a restart mid-game does not lift it.
    pub active: bool,
    /// Global fallback for chat-class local models. `None` = refuse.
    pub fallback_alias: Option<String>,
}
```

`settings.hold`. Not inside `VramSettings` — `enabled: false, hold: true`
would read as a contradiction, and hold is a runtime mode, not a capacity
policy.

**`active` is not patchable through the settings patches.** It is toggled only
through `ops::hold_set` (§6), because engaging it has a side effect (the
sweep) that a generic settings save must not grow. A `HoldSettingsPatch {
fallback_alias: Option<String> }` (`""` = clear, otherwise validated) joins
`SettingsFullPatch`; `ops::SettingsPatch` gains `hold_fallback_alias:
Option<String>` with the same convention so `lmgw__settings_set` can set it.
`GET /api/settings-full` returns `"hold": s.hold`; `ops::settings` (the
hand-rolled JSON behind `lmgw__settings`) gains a `"hold": {active,
fallback_alias}` block *(rev)*; `lmgw-api-types::SettingsFull` mirrors it as
`HoldSettingsDto`.

*(rev)* **Settings writes get a mutex.** `ops::settings_set`,
`settings_set_full` and `legacy_sweep` all read-modify-write the whole
`Settings` blob with no lock, so a dashboard save racing `hold_set` could
revert `hold.active` after the sweep already ran. `AppState` gains
`settings_write: tokio::sync::Mutex<()>`; every `store::save_settings` caller
holds it across snapshot → mutate → save → reload. Pre-existing latent bug,
fixed here because hold is the first setting a race would visibly break.

*Changed 2026-10-08:* the mutex covers snapshot → mutate → save → publish, and
the MCP reconcile runs once it is released, in every writer (`settings_set`,
`settings_set_full`, `hold_set`, the boot sweep of old container names). The
reconcile can start a stdio server, and its start can take as long as a cold
image pull: under the mutex, a tray click on the hold waited for that, and so
did the next settings save. The publish stays under it, since the next writer
copies the published settings and would otherwise revert the save.

`reload_snapshot` also calls `vram.forget_plans()` and `mcp.reconcile()` on
every call. A toggle is rare; accepted, and the footprint cache refills on the
first request after release.

### 3.2 Per-model override

*(rev)* Two columns, not a `''` sentinel: `ops::opt` and the aux/audio patch
structs already define `""` as "not supplied", so a three-state single column
would be unrepresentable through every patch path.

Migration `0024_hold_fallback.sql`, same shape as 0022:

```sql
ALTER TABLE local_models ADD COLUMN hold_fallback_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE local_models ADD COLUMN hold_fallback TEXT;
-- same two for aux_models and audio_models
```

`hold_fallback_mode ∈ {inherit, none, alias}`; `hold_fallback` is the alias
and is only meaningful when the mode is `alias`. Rust: `HoldFallbackMode`
(serde lowercase) + `hold_fallback: Option<String>` on `LocalModel`,
`AuxModel`, `AudioModel` (config + api-types mirrors), read and written by the
three `list_/insert_/update_` store families, and a `Snapshot::hold_fallback_for(class,
model_id) -> Option<String>` implementing the §2 order.

Patch structs: `ops::LocalModelPatch`, `web::api::AuxPatch`,
`web::api::AudioPatch` gain `hold_fallback_mode: Option<HoldFallbackMode>` and
`hold_fallback: Option<String>`. Setting `mode = alias` requires an alias and
validates it; `clear: "hold_fallback"` resets both to `inherit`/`NULL` — in
**both** places the existing `clear` convention lives (the `overlay_params`
no-op arm at ops.rs ~1086 and the caller-side reset ~1643). The MCP plane
reaches only the chat one (`lmgw__local_model_set`) — the aux/audio asymmetry
is pre-existing and not widened here.

### 3.3 Validation (set time)

`ops::validate_fallback_alias(&snap, alias) -> Result<(), String>`: alias must
resolve and must not classify as local. Applied by every setter above. Request
time re-checks (§2), because rows change. See the bare-`expose_all` limit in §2.

## 4. The gate *(rev: swap at resolve time, not inside `admit`)*

The review found four call sites that inspect the route **before** `admit`
(legacy completions and the audio handlers check `upstream.protocol`,
dashboard chat injects a llama.cpp-only passthrough field, the quickdoc plans
hold the route immutably). Swapping inside `admit` would invalidate all of
them. So the swap happens where the route is born:

```rust
pub struct Resolved {
    pub route: Route,
    /// `Some(alias)` when hold re-routed a local model to this fallback.
    pub fallback: Option<String>,
}

impl Snapshot {
    /// `resolve`, plus the hold rule: a local route while `hold.active` becomes
    /// its fallback's route, or `Err(GatewayError::GpuHold)` when it has none.
    pub fn resolve_for_request(&self, alias: &str) -> Result<Resolved, GatewayError>;
}
```

The **14** request-shaped sites switch from `resolve` to `resolve_for_request`
(proxy.rs: chat 615, legacy completions 1066, count_tokens 1285, embeddings
1467, rerank 1639, audio 1895/1989/2114; responses.rs 105; web/chat.rs 300;
web/agentchat.rs 361; mcp/handler.rs 163; web/workflows.rs 765; and
`count_tokens_inner`'s in-process callers inherit it). The two batch runners
(quickdoc/ingest.rs 432, quickdoc/golden.rs 307) keep `resolve` and are refused
by `admit_local` (§2). `modelinfo::local_model_test` (879) refuses explicitly
before admit.

`admit`'s signature does not change. Downstream of the swap a fallback route
classifies as non-local, so `admit` returns `Ok(None)` and the caller forwards
exactly as for a cloud alias. `admit_local` **opens with** the hold check and
returns `Err(GpuHold)` when active — the safety net for recovery, the batch
runners, and any future caller that bypasses `resolve_for_request`.

Error variant:

```rust
/// GPU hold is active and this local model has no usable fallback.
#[error("'{model}' is a local model and lmgw is holding the GPU{detail} — release the hold or configure a fallback alias")]
GpuHold { model: String, detail: String },   // 503 · gpu_hold · api_error / api_error
```

`detail` is `""` or e.g. `" (fallback 'x' is itself a local model)"`.

`check_background_start` gains a first branch: hold active ⇒ `Fit::Held(why)`.
`Fit` is `pub` and matched exhaustively in `lifecycle::boot` and
`ops::start_model`; both get the arm. `ops::start_model` reports
`"GPU hold is active — {why}"`, not the "not enough GPU memory" wording.
`ops::model_apply`, `recreate_running`, `group_start` and the `restart` verb
check `hold.active` **before** their stop step (§2).

**Response header.** `x-lmgw-fallback: <alias>` on every HTTP handler's
response when `Resolved.fallback` is set — **including its error responses**
*(rev 2)*. A fallback-served request whose provider then fails is still a
request the fallback answered, and since the body's `model` stays the
requested alias the header is the only thing that says so. The inner helpers'
error tuples therefore carry it too: `proxy::Failed = (Option<Route>,
Option<String>, GatewayError)` for embeddings/rerank/audio, and
`(Option<String>, GatewayError)` for `count_tokens_inner`. Every handler returns
`axum::response::Response` (streams included, `Body::from_stream`), so one
helper mutating `headers_mut()` at each handler's return covers all of them.
The work is carrying `fallback` **out of the inner helpers** whose return types
in-process callers share: `count_tokens_inner`, `embeddings_inner` /
`embed_in_process`, `rerank_inner` / `rerank_in_process`, and `AudioOutcome` —
six return types grow an `Option<String>`.

## 5. Freeing the GPU

`lifecycle::hold_sweep(state) -> HoldSweep { stopped: Vec<String>, draining:
Vec<String>, failed: Vec<String> }`: for every registry entry that is `Ready`
with `in_flight == 0`,
ask its `/slots` (`vram::busy_slots` becomes `pub(crate)`; same probe eviction
uses, because the dashboard publishes container ports and a direct client
mid-generation is still respected — an unanswerable `/slots`, e.g. audio.cpp,
counts as idle exactly as it does for eviction), then `Registry::stop(class,
id, force = false)`. **Stop, never remove** — the stopped container is what
the next start's `--replace` collects (predecessor §3.6). `Starting` entries
and busy ones go into `draining`. Nothing is forced.

*(rev 2, post-review)* A stop that **fails** — `podman stop` outliving
`vram.unload_timeout_seconds`, or podman itself not running — goes into
`failed` as `"{class}/{model_id}: {error}"`, logged at warn. Its own bucket
because `Registry::stop` forgets the map entry before returning `Err`
(registry.rs: after a failed stop lmgw's belief about the container is
worthless), so unlike a draining one **nothing retries it** — the container
keeps the card until somebody looks. Reported before this, it was reported as
nothing at all: `stopped: []`, `draining: []`, "0 container(s) stopped".

Called from three places:

- `ops::hold_set(true)` — immediately, and its result is the op response.
- `reap_idle` — every 15 s tick while `hold.active`, **before** the idle
  logic and regardless of `idle_seconds`. This is what drains busy models:
  their claim drops, the next tick stops them. `idle_seconds = 0` models are
  therefore reaped under hold even though they are never reaped otherwise.
  *(rev)* When the tick stopped at least one container it pushes a `vram`
  frame (`vram::broadcast`) — the frame is event-driven, nothing else would
  tell the dashboard the model is gone.
- `boot` — *(rev)* right after `legacy_sweep`, and `boot` **returns** before
  the warm-start block when hold is active. `boot` is spawned, not awaited, so
  relying on `Fit::Held` alone would let the warm-start `join_all` delay the
  free.

The engage race is closed by the registry: `StartClaim::ready` flips `Ready`
and increments `in_flight` under one lock, so a request admitted just before
the toggle owns a `Ready`/busy entry the sweep skips, and the reaper takes it
after the request ends.

## 6. Surfaces

- **`ops::hold_set(state, active: bool) -> Result<Value, String>`**: no-op if
  unchanged; under the settings mutex saves `hold.active`, publishes the new
  snapshot, when engaging runs the sweep, and only then reconciles MCP; pushes
  a `vram` frame; returns
  `{ "active", "fallback_alias", "stopped": [...], "draining": [...], "failed": [...] }`,
  with the failures also in the message sentence (a tray click sees only that).
  *(rev 2)* The three-step order is load-bearing in both directions:
  publishing the snapshot is what arms the gate, so it precedes the sweep or a
  request arriving mid-sweep starts a container the sweep already walked past;
  and `mcp.reconcile` — a `join_all` over autostart servers — follows the
  sweep, because one unreachable tool server would otherwise delay handing the
  GPU back by its whole connect timeout. Same hazard §5 restructured `boot`
  for; hence `AppState::publish_snapshot`, the half of `reload_snapshot` that
  only touches this process. *Changed 2026-10-08:* the reconcile runs after the
  settings mutex is released (§3.1's note of this date).
  Exposed as `POST /api/op/hold_set {"active": bool}` and MCP `lmgw__hold_set`
  (`writes: true`, so self-admin `read_only` refuses it like `lmgw__container`).
- **`VramView`** gains `hold_active: bool`, `hold_fallback_alias:
  Option<String>` and `draining: Vec<String>` (resident entries that are busy
  or starting while hold is active — continuously visible, not only in the
  one-shot op response *(rev)*), spelled `"{class}/{model_id}"` exactly as
  `HoldSweep` and the `hold_set` response spell it *(rev 2: one name for one
  thing — a model id alone is not unique across the three classes, and two
  shapes for the same list is how a surface ends up unable to match the op
  response against the live frame)*; `VramStatus` mirrors. `inactive_reason` is
  untouched — hold is not a reason admission is inactive.
- **Dashboard.** Titlebar (`shell.rs`) shows a `HOLD` pill whenever
  `live.vram.hold_active`, with `· N draining` when applicable; clicking it
  calls `hold_set(false)` (a release, like the tray). Settings page gets a
  "GPU hold" section: the toggle (calls `hold_set`, not the settings patch),
  the global fallback `<Select>` (widgets.rs) over `/v1/models` aliases with a
  "None — refuse" entry, and one line explaining that only chat models inherit
  it and that batch jobs never fall back. The three model editors get a "Hold
  fallback" `<Select>`: *Inherit* (chat: "inherit global"; aux/audio:
  "inherit — none") / *None* / aliases, mapping onto mode + alias. Palette:
  existing app.css tokens, no new colours.
- **Tray.** `CheckMenuItem` "Hold GPU · pause local models" above "Stop all
  models"; the handler calls `ops::hold_set(&st, !current)` and then the same
  sync the loop runs. The 5 s status loop looks the icon up with
  `app.tray_by_id("lmgw-tray")` *(rev: rather than moving a handle into the
  task)* and syncs on change only: `set_icon` (tray.png ↔ tray-hold.png),
  `set_tooltip`, the check state, and the runtime line gains ` · HOLD`. So a
  toggle from the dashboard or MCP reaches the tray within one tick.
- **Icon.** `src-tauri/icons/tray-hold-source.svg`: the robot silhouette with a
  bottom-right badge — a circle punched out of the head corner via the mask,
  refilled smaller, with two pause bars punched out of it. Rendered with
  `rsvg-convert -w 64 -h 64`. `src-tauri/icons/gen_tray.sh` renders both tray
  PNGs from their SVGs, which also makes the existing conversion reproducible
  for the first time.

## 7. Testing

`tests/it/vram_admission.rs` fixture (fake podman + FakeGpu + wiremock
containers), plus a cloud wiremock upstream with an alias row for fallback
cases (no bare `expose_all` upstream, so unknown aliases really are unknown):

1. Hold on, chat, no fallback → 503 `gpu_hold`, no `podman run`.
2. Hold on + global fallback → 200 from the cloud mock, header
   `x-lmgw-fallback`, body `model` = requested alias, request log
   `upstream_name` = fallback upstream, no run.
3. Hold on, embed with only the global fallback → 503; with a row override
   (`mode = alias`) → served by the cloud mock.
4. Row `mode = none` on a chat model with a global set → 503.
5. Fallback alias that is local → 503 naming it; unknown alias → 503 naming it.
6. Engage while one model is idle-resident and one is mid-request → idle one
   in `stopped`, busy one in `draining` and in `GET /api/vram`; release the
   claim, run `reap_idle`, busy one stopped although its `idle_seconds` is 0.
7. `vram.enabled = false` + hold → still 503 (independence).
8. Hold on: `ops::container start` and `restart` refused with the hold wording
   and **no stop issued**; `apply` on a running model stops it and says so;
   boot warm start skipped and adopted containers stopped; `local_model_test`
   refused.
9. Release → the same request starts a container.
10. `POST /api/op/hold_set` round trip; MCP: `lmgw__hold_set` present under
    `full`, refused under `read_only`; `lmgw__status.vram.hold_active`;
    `lmgw__settings` shows the `hold` block.
11. Set-time validation: global/per-model fallback refused when local or
    unknown; `clear: "hold_fallback"` resets both columns.
12. Legacy completions with an Anthropic-protocol fallback → the handler's own
    protocol error, not a silent misroute (the resolve-time swap at work).
13. Quickdoc golden generation under hold with a global fallback set → job
    fails with `gpu_hold`, cloud mock never called.

`bash ci/check.sh` (clippy, trunk build, workspace tests) is the gate.

## 8. Work packages (sequential)

1. **Model + plumbing**: `HoldSettings`, settings mutex, migration 0024,
   config/store/api-types fields, `Snapshot::hold_fallback_for`,
   `GatewayError::GpuHold`, patches + `clear` + validation, `ops::settings`
   block. Compiles, tests pass.
2. **Gate + lifecycle + ops**: `Resolved`/`resolve_for_request`, the 14 sites,
   header plumbing, `admit_local` check, `Fit::Held`, `hold_sweep`,
   reaper/boot hooks + broadcast, `hold_set` op + route + MCP tool, view
   fields, `local_model_test` refusal, apply/restart gating.
3. **Tests** (§7).
4. **UI**: settings section, titlebar pill, three editor selects.
5. **Tray + icon + gen script.**
6. **Docs**: README "GPU hold" subsection, core-notes gate table.
7. **Adversarial review** of the whole diff, then `ci/check.sh`, merge to main.

## 9. Out of scope

Automatic engagement (detecting a game via NVML per-PID attribution — still the
predecessor §4 follow-up); a `Retry-After` header (no honest number to put in
it); MCP setters for aux/audio rows; fallback for anything other than hold
(queue timeouts keep failing as they do today); catalog-aware validation of
fallback aliases on bare `expose_all` upstreams.

*(2026-09-27)* Fallback beyond the hold is now specified: a shortfall of VRAM
that lmgw cannot free (outside use) triggers it too, while contention among
lmgw's own models still queues. See
[2026-09-27-candidate-aliases-unified-kv-design.md](2026-09-27-candidate-aliases-unified-kv-design.md)
§4.7.
