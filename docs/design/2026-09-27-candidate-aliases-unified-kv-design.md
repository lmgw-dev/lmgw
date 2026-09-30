# Candidate aliases, background traffic and unified KV — design (2026-09-27)

**Status:** Implemented (2026-09-27/28): §3 and the gate core (phase 1), §4.7 (phase 2), §4.1–4.6 candidate aliases (phase 4). Choices made during implementation are in §12, for review.

Sibling of [2026-09-27-ladder-models-design.md](2026-09-27-ladder-models-design.md).
Both specs share one request gate: the max-output clamp and the fit check.
Builds on [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
(admission, eviction), [2026-09-04-gpu-hold-design.md](2026-09-04-gpu-hold-design.md)
(fallback modes, the 503 existing clients already handle) and
[2026-09-17-model-capabilities-design.md](2026-09-17-model-capabilities-design.md)
(the capability schema).

## 1. Summary

The goal is to let scheduled jobs use lmgw's local models **without ever
interrupting the owner**. Today a job's request can evict the model the owner is
working with, and the hold is no answer because it stops the owner's own use too. That
is why no such jobs exist yet.

Two features, built in this order:

**A. A unified-KV toggle on local models.** `kv_unified` plus an optional
per-conversation cap, `kv_unified_per_slot`.
- A model can run several slots and still give one conversation the whole
  context.
- llama-server aborts *every* running request when a shared pool overflows
  (fact 3). So lmgw keeps a **token ledger** per unified model:
  - each request reserves prompt + max output;
  - a request that does not fit the free pool waits its turn, strictly first
    in, first out.
- Max output becomes mandatory on a guarded model, for the same reason as on
  ladders.

**B. Candidate aliases.** An alias whose target is a **primary** local chat
model plus an ordered list of **alternates**. It has an optional `background`
flag, **one fallback for the whole alias**, and an explicit set of
capabilities.

- **The alias only ever loads the primary.** An alternate is used only when it
  is already loaded.
- **Without the flag:**
  - use the primary if it is loaded;
  - otherwise use an alternate if one is loaded, instead of evicting to make
    room;
  - otherwise load the primary, evicting as usual;
  - under the hold, or when VRAM that lmgw cannot free is short (C), use the
    fallback.
- **With the flag,** the traffic is a guest on the GPU:
  - use the primary if it is loaded, or load it if that fits without
    disturbing the owner;
  - otherwise use an alternate if one is loaded;
  - otherwise use the fallback.
- **The fallback belongs to the alias as a whole.** It has the same three modes
  every model has: inherit the global fallback, none, or a named alias. The
  candidates' own row fallbacks are never used through the alias. Privacy is
  decided here: `none` never leaves the machine and answers with the hold 503.
- **Capabilities are explicit.**
  - `/v1/models` publishes exactly the capabilities enabled on the alias.
  - Only capabilities every candidate supports can be enabled, and those are
    on by default.
  - A candidate can only be added if it supports everything that is enabled.

**C. A fallback when VRAM lmgw cannot free is short.** This applies to every
local model and every non-background alias (§4.7).
- **The hold sets lmgw's VRAM allowance to zero.** Without the hold, lmgw may
  use whatever is free. A fallback answers whenever a model does not fit into
  that allowance; the hold is just the zero case.
- **lmgw separates VRAM it can free itself from VRAM it cannot.**
  - Its own models it can free: it evicts idle ones and waits for busy ones.
    That contention keeps queueing, as today.
  - Memory used by games and other apps it cannot free. Only that shortfall
    triggers the fallback, and it does so immediately, since waiting would not
    help.
- **A model that cannot fit even on an empty card stays an error.**
- **The trigger needs per-process measurement of lmgw's own containers.**
  Without that, or with the new settings switch off, only the hold triggers the
  fallback.

**Decisions (2026-09-27):**
- **Fair queueing, never preemption.** A busy background model finishes its
  work. An owner request that needs its VRAM waits, and meanwhile that model
  takes no new background work (§4.5). The scheduled tasks are short.
- **Sharing the owner's model is fair.** Owner and background requests queue
  alike. The owner's prompt cache survives through prefix-matched slots or the
  RAM cache (facts 4–5).
- **One fallback for the whole alias,** in both modes. Privacy is decided per
  alias, through its fallback mode.
- **The resolution order in both modes** is as stated above (§4.2–4.3), in
  the owner's words:
  - **background:** "load primary if possible, if any candidate is loaded use
    that, otherwise use fallback";
  - **without background:** "load primary and evict if not on hold, if
    candidate is loaded use that instead, otherwise use fallback".
- **Capabilities on an alias are explicit** and enforced (§4.6).
- **The fallback also fires when VRAM lmgw cannot free is short (C, §4.7).**
  - The owner reads the hold as "lmgw uses no VRAM", and no hold as "lmgw may use
    what is free". A fallback therefore belongs to "the model doesn't fit into
    what lmgw may use", not to the hold switch alone.
  - The distinction between VRAM lmgw can free and VRAM it cannot is what keeps
    brief contention and misconfigured models off the fallback.
  - Where lmgw can't measure it, the behaviour is hold-only.
  - A settings switch turns the trigger off, for shared-memory systems where
    "free VRAM" moves with host RAM.
- **Rows that run llama-server's auto slots are left alone.**

## 2. Facts this rests on (checked 2026-09-27)

### 2.1 llama.cpp

Upstream master `171e884`.

1. **With `-np` unset, the server picks 4 slots and unified KV.**
   - The flags are `-kvu/--kv-unified` and `-no-kvu/--no-kv-unified`.
   - "n_parallel is set to auto, using n_parallel = 4 and kv_unified = true"
     (`tools/server/server.cpp:156-161`).
   - With `parallel` set, the KV cache is split: each slot gets `-c / np`.
2. **The per-request limit.**
   - It is `n_ctx_slot() = llama_n_ctx_seq(ctx)`: the whole pool when unified,
     `-c / np` when split.
   - It is capped by `--kv-unified-per-slot` and by the trained context
     (`server-context.cpp:4027`).
   - `--kv-unified-per-slot N` without `-c` sizes the pool to `np × N`
     (`server.cpp:164-172`).
3. **A full shared pool aborts everyone.** When decode finds no free KV cells,
   the server:
   - first clears idle slots (`try_clear_idle_slots`);
   - then halves the batch;
   - at batch size 1, sends "Context size has been exceeded." to **every
     processing slot** and clears them (`server-context.cpp:3688-3730`).
     Upstream's TODO reads "terminate only the largest active slot".

   This is the interruption that must not happen.
4. **How a slot is picked.**
   - Preferred: an idle slot whose cached prompt shares the longest prefix with
     the request, above `--slot-prompt-similarity` (default 0.1).
   - Otherwise the least recently used idle slot.
   - With no idle slot, the task waits in the server's own FIFO
     (`server-context.cpp:1547-1630, 2398-2412`).
5. **Idle slots and the RAM cache.** `--cache-idle-slots` is on by default and
   needs `--cache-ram` (default 8192 MiB).
   - When any task starts, every idle slot's prompt is saved to the RAM cache.
   - **With unified KV, the idle slots are also cleared from VRAM**
     (`server-context.cpp:2436-2452`).
   - So an owner conversation that was idle when a background request started
     comes back from RAM on its next turn: a copy, not a recompute.
   - With split KV, the idle slot keeps its KV in VRAM, and prefix matching
     steers the owner back to it.
6. **`/slots` reports what a pool check needs.**
   - Per slot: `n_ctx`, `is_processing`, `n_prompt_tokens` and
     `next_token[0].n_remain` (`server-context.cpp:686-713`).
   - Unless called in its metrics-only form, it also returns the detokenized
     prompt, which is expensive on long contexts. WP0 finds the cheap form.
7. **`-n` is not a cap** (ladder spec, fact 1). Bounded reservations need
   lmgw's clamp.

### 2.2 lmgw

8. **`check_background_start`** (`vram/mod.rs:1140`) never evicts, never queues
   and never waits; it answers `Go`, `Full` or `Held`. Today it serves warm and
   operator starts. It is the start rule for background traffic.
9. **Eviction and the admission queue.**
   - Eviction is LRU among ready, idle entries; busy ones are skipped after a
     `/slots` probe (`vram/mod.rs:1424-1483`).
   - A request that cannot be admitted waits up to
     `vram.queue_timeout_seconds`, then gets `503 vram_queue_timeout`
     (`error.rs:76-79`).
10. **Today an alias cannot point at a local model.**
    - The alias table `models` requires a stored upstream
      (`upstream_id NOT NULL REFERENCES upstreams`,
      `migrations/0001_init.sql:17-26`).
    - Local models live on synthetic, unstored upstream ids (`config.rs`,
      `ROUTER_UPSTREAM_ID`), so local models are called by their own names.
    - There are zero aliases configured.
11. **The hold machinery.**
    - `GatewayError::GpuHold { model, detail }` renders as 503 with code
      `gpu_hold`.
    - Local rows carry a fallback mode, `hold_fallback_mode`
      (inherit / none / alias), plus `hold_fallback` (`config.rs:167-256`).
    - The fallback is validated at save time (`ops::validate_fallback_alias`,
      `ops.rs:6053`) and again when used (`config.rs:2577`). A local fallback is
      refused.
12. **Published local context is `ctx_size / parallel.unwrap_or(1)`**
    (`capabilities/exposed.rs:302-307`). That is correct for split KV and wrong
    once someone sets `parallel` and turns unified on.
13. **The capability schema has one rule: absent means unknown**
    (`capabilities/mod.rs:13-16`).
    - Its facets are `vision`, `input_modalities` (includes `"audio"` when
      present), `reasoning`, `tool_calls` and `structured_output`
      (`capabilities/mod.rs:44-60`).
    - A row's `capabilities_override` can set any of them.
14. **GPU telemetry is device-wide today.**
    - NVML gives used/free/total per device (`vram/nvml.rs`). Nothing
      attributes VRAM to processes; the gpu-hold spec §9 lists "NVML per-PID
      attribution" as a follow-up.
    - Residents are charged by *estimate*: weights + KV from the GGUF, which is
      a documented lower bound. `vram.headroom_mb` covers the rest
      (`config.rs:1600-1630`).
    - On an AMD APU, capacity includes GTT, i.e. host RAM the GPU may map, and
      "free" is clamped by the host's `MemAvailable` (`vram/amdgpu.rs:1-30`).
      So on a shared-memory system, free "VRAM" moves with everything else
      using RAM.
15. **The existing fallback triggers only on the hold.** The gpu-hold spec §9
    put "fallback for anything other than hold" out of scope; §4.7 is that
    follow-up.
    - On the main deployment no row has a fallback configured as of
      2026-09-27: all 38 rows inherit, and the global fallback is unset.
    - That is one deployment, not a guarantee. Any install with fallbacks
      configured changes behaviour on upgrade, because its fallbacks start
      answering outside-VRAM shortfalls. The release notes must say so, and
      name the switch that restores hold-only (§8, WP 9).

## 3. Unified-KV toggle

### 3.1 Config

- **`LlamaParams.kv_unified: Option<bool>`**, rendered as `--kv-unified` or
  `--no-kv-unified`. `None` means the llama-server default: unified exactly
  when `parallel` is unset.
- **`LlamaParams.kv_unified_per_slot: Option<i64>`**, rendered as
  `--kv-unified-per-slot`.
- **Effective unified** = `kv_unified.unwrap_or(parallel.is_none())`.
  Effective slots = `parallel`, or 4 when it is unset.
- **Editor:** in "Context & batch", next to `parallel`, a select
  (default / on / off) plus the cap field. The hint explains what a shared pool
  does.
- **MCP:** `lmgw__local_model_set` gains both fields and `clear` for them.

### 3.2 Published context

- **Unified:** per-request context = min(`ctx_size`, `kv_unified_per_slot`,
  trained context).
- **Split:** `ctx_size / parallel`, as today.

### 3.3 The pool ledger

**When it is active.** Effective unified, effective slots > 1, and
`n_predict > 0`.
- Saving a row with `kv_unified` explicitly on and `parallel > 1` but no
  `n_predict` is refused.
- Rows that only reach a shared pool through llama-server's auto default keep
  loading as they do now, without the guard. The editor shows a one-line note
  ("slots share one pool unguarded").

**Per request to a guarded model:**
1. **Clamp** `max_tokens` to `n_predict` (ladder spec §3.2, the same code and
   the same `x-lmgw-max-tokens-clamped` header).
2. **Count** the prompt on the running server: `/apply-template` →
   `/tokenize`, plus a per-media bound (ladder spec §3.3, the same code).
3. **Reserve** `r = prompt + max_tokens`.
4. If `r` is above the per-request limit (§3.2), return
   `400 context_length_exceeded`.
5. If `r ≤ pool − Σ reservations in flight on this model`, forward.
   Otherwise, wait in the model's queue. The queue is **strictly first in,
   first out**: a small request never overtakes a waiting large one, so large
   requests cannot be starved.
6. The wait is bounded by `vram.queue_timeout_seconds`, after which the request
   gets `503 vram_queue_timeout`. The reservation is released when the response
   ends, success or not.

**What the ledger does not track:**
- **Idle slots.** They are not reservations; llama-server moves them to RAM
  itself (facts 3 and 5).
- **Requests sent straight to the container's published port.** They bypass
  the ledger. That is a documented limit. WP0 checks whether a `/slots`
  cross-check can cover them cheaply.

**Ladders in v1.** Ladder rows keep refusing unified KV (ladder spec §4.3,
rule 2 reads this toggle). With the ledger in place, "does not fit the free
pool" means wait, and "does not fit the empty pool" means climb, so a later
version can allow both together (§10).

## 4. Candidate aliases

### 4.1 Config

- **A new table `candidate_aliases`:**
  - `id`, `alias TEXT UNIQUE`;
  - `candidates TEXT`: a JSON array of local chat model ids. The first entry is
    the **primary**, the rest are **alternates** in preference order;
  - `background INTEGER`;
  - `fallback_mode TEXT` (`inherit` | `none` | `alias`) and `fallback TEXT NULL`;
  - `capabilities_disabled TEXT`: a JSON array (§4.6);
  - `enabled`, `notes`, timestamps.
- **The names are unique** across `models.alias`, local public names and
  candidate aliases.
- **Candidates are local chat models only.** A cloud model is reached through
  the fallback, which is exactly where the privacy decision sits.
- **One fallback per alias, following the row rules** (fact 11): validated
  with `validate_fallback_alias` at save time and again when used, never local.
  `inherit` reads the global hold fallback.
- **The candidates' own row fallbacks are never used through the alias.** A
  held candidate is simply not loaded, and the alias's fallback decides.
- **Refused at save time:**
  - an empty list or a duplicate entry;
  - an id that is not an enabled local chat model;
  - a fallback that does not validate;
  - a candidate that lacks an enabled capability (§4.6).

**The alias fallback**, used by both modes:
- **a fallback alias:** it serves the request, with `x-lmgw-fallback`;
- **none:** `503`, code `gpu_hold`, which is what the clients already handle.

**"Loaded"** means ready in the registry, or already starting (joining a start
in flight), and able to take this request: it fits the model's current
per-request context (§4.6 covers capabilities). A loaded model that cannot
take the request counts as not loaded for that step.

Answered responses carry `x-lmgw-candidate: <model id>`.

### 4.2 Resolving a request without the `background` flag

**Hold active:** the alias fallback, straight away. Under the hold nothing
local takes new work, loaded or not (gpu-hold spec §2).

Otherwise, the first step that applies wins:

1. **The primary is loaded:** use it.
2. **An alternate is loaded,** first in list order: use it instead of evicting
   anything to make room for the primary.
3. **Load the primary** with normal admission, which may evict idle models and
   may queue.
   - If the verdict is that VRAM lmgw cannot free is short (§4.7), the alias
     fallback answers instead.
   - `vram_too_large`, and `vram_queue_timeout` after contention among lmgw's
     own models, are returned as they are for a direct request.

Once a model is chosen, queueing, ladder climbs and the pool ledger work as for
a direct request to that model.

### 4.3 Resolving a request with the `background` flag

**Hold active:** the alias fallback, logged as `gpu_hold`.

Otherwise, the first step that applies wins:

1. **The primary is loaded** and not **draining for the owner** (§4.5): use it.
2. **The primary can be loaded without disturbing the owner.**
   - `check_background_start` says `Go`.
   - The only thing it may evict is an *idle `Background`-owned* model (§4.4).
   - Start the primary, then use it. It becomes `Background`-owned.
3. **An alternate is loaded** and not draining for the owner, first in list
   order: use it.
4. **The alias fallback.** With none, the `503 gpu_hold` message reads
   "deferred: GPU in use by <model>". It is logged with its own error kind,
   `gpu_deferred`, so the request log and dashboard can tell a deferral from a
   real hold.

**Waiting on a chosen model is fair:** llama-server's own FIFO for split KV,
the pool ledger for unified KV.

**Ladders.** Background **never climbs an `Owner` model**; a request that does
not fit its current rung treats it as not loaded. Background may climb the
primary when it is `Background`-owned, but only if the next rung passes
`may_climb`: the card size first (`vram_too_large` is a configuration error
regardless of who asks; for a guest it is logged as one and the walk goes on,
entry 92), then measured free VRAM plus the running rung's own footprint —
the climb stops that rung and evicts nothing else (entry 66).
Not `check_background_start`: that rule governs starting a primary from cold
(step 2 above), and a running climb never restarts one.

**Cost cap.** A background job's cloud spend through the fallback is capped by
the existing per-key and global budgets (`key_budget`, usage-analytics spec
§4.2). Giving each job its own key makes that cap per job.

### 4.4 Ownership

- **Each registry entry records `owner: Owner | Background`.**
  - It is `Background` when a background request started it. That can only be
    a background alias's primary, since alternates are never loaded by an
    alias.
  - The first non-background request that acquires it makes it `Owner`, and
    it stays that way until the container stops.
  - Warm starts, operator starts and every start that is not background are
    `Owner`.
- **Background traffic may evict only idle `Background` entries.**
- **Owner traffic may evict any idle entry**, as today.

### 4.5 Fair queueing, never preemption

- **An owner request needs VRAM that a busy `Background` model holds:**
  - the owner request waits in the existing admission queue;
  - the model is marked **draining for the owner**: new background requests
    skip it (a loaded alternate, or the fallback), and the ones in flight
    finish;
  - once it is idle it is evictable, and the owner's model starts;
  - if the queue timeout runs out first, the owner gets `vram_queue_timeout`,
    as today. Short background tasks make that unlikely.
- **Owner and background requests on the same model queue alike**, in the
  slot FIFO (split) or the pool ledger (unified). Neither has priority.
  - The owner's idle conversation keeps its cache: a prefix-matched slot with
    split KV, the RAM cache with unified KV (facts 4–5).
  - A candidate with `--cache-ram 0` gets an advisory in the alias editor,
    because it loses that.
- **A request that has started always finishes.** Nothing is preempted.

### 4.6 Explicit capabilities

**The toggles** are the capability facets from fact 13:
- vision (image input);
- audio input;
- tool calls;
- reasoning;
- structured output.

**A candidate supports a facet only if its published capabilities say so
positively.** Absent means unknown, and unknown counts as unsupported. The fix
is the row's `capabilities_override`, and the editor links to it.

**Stored as `capabilities_disabled`.** The enabled set is
`(facets every candidate supports) − capabilities_disabled`. That gives the
three rules directly:
1. **Only facets every candidate supports can be enabled.**
2. **Those facets are on by default,** including ones that become common
   later, for example after removing the one candidate that lacked them.
   A facet that was switched off stays off.
3. **Adding a candidate that lacks an enabled facet is refused,** naming the
   facet. Switch the facet off first, then add the candidate.

**The fallback is held to the same rule.** It answers the alias's requests
too, so it has to support every enabled facet. With `inherit`, that means the
global fallback. A cloud entry with unknown capabilities gets them through its
own `capabilities_override`.

**When a row changes later.** If a candidate's row (or the fallback) is edited
so it no longer supports an enabled facet:
- that candidate is **skipped** when routing;
- the fallback is treated as none;
- the alias shows a problem in the editor, `lmgw__models` and `lmgw__status`.

The contract never shrinks silently.

**Requests.** A request that uses a facet the alias does not enable (images
without vision, tools without tool calls) is refused with `400` naming the
facet.

**`/v1/models` publishes:**
- exactly the enabled facets;
- detail fields (reasoning levels, tool-call format) only when they are
  identical across candidates, otherwise absent;
- `context_length` and `max_output_tokens`: the minimum across candidates. A
  ladder candidate counts with its top rung.

### 4.7 Fallback when VRAM lmgw cannot free is short (all local models)

**Applies to:**
- **plain local models** of every class, through their row fallback
  (inherit / none / alias);
- **non-background candidate aliases**, through the alias fallback.

Background aliases already fall back earlier, whenever the primary cannot load
without disturbing the owner (§4.3).

**The verdict**, taken at admission for a model that is not resident:
- **`needed`** = footprint + `vram.headroom_mb`, as today.
- **`lmgw_share`** = the *measured* VRAM of every other lmgw container on the
  device, idle or busy. That is everything lmgw could free by evicting and
  waiting.

| Condition | Outcome |
|---|---|
| `needed` > capacity (`budget_mb` or the device total) | `vram_too_large`, as today. Never a fallback: this is a configuration error. |
| `free + lmgw_share ≥ needed` | lmgw can make room itself. Today's path: evict idle models, queue for busy ones, `vram_queue_timeout` on timeout. No fallback. |
| otherwise, a fallback is configured | The shortfall is VRAM lmgw cannot free. The fallback answers **immediately**. |
| otherwise, no fallback | Today's path (queue, then error), since the outside use may end within the timeout. |

**Measurement.**
- **`lmgw_share` needs per-process attribution:**
  - NVIDIA: NVML's running-process list, matched to each lmgw container's host
    PID (`podman inspect` → `State.Pid`);
  - amdgpu: the DRM `fdinfo` memory keys of those PIDs.
- **Estimates are deliberately not used for this verdict.** They are lower
  bounds (fact 14). An underestimated lmgw share reads as outside pressure,
  and would send requests to the cloud that lmgw could have served.
- **WP0 verifies** that attribution sees rootless podman containers.

**Hold-only**, meaning today's behaviour, applies in any of these cases:
- no probe answers;
- admission is disabled (`vram.enabled = false`);
- attribution is unavailable for any lmgw container on the device;
- the switch below is off.

**Setting:** `vram.fallback_on_external: bool`, default **on**.
- Settings label: "Fall back when VRAM outside lmgw's control is short".
- Hint: turn it off on shared-memory systems (APUs), where the GPU's memory is
  host RAM that grows and shrinks with everything else running (fact 14).

**Visible:**
- Every fallback response carries `x-lmgw-fallback-reason: hold |
  external_vram | background` next to `x-lmgw-fallback`, and the request log
  records the reason.
- `lmgw__status` and `GET /api/vram` show:
  - lmgw's measured share;
  - the share outside lmgw;
  - whether the external trigger is active, and if not, why.

## 5. Where it plugs in

**One async gate** runs before `admit` at every local chat send site. The
ladder spec's gate is the same function. In order:
1. the hold swap (existing, `resolve_for_request`) for direct model names;
2. the candidate pick, including the alias fallback (§4.2–4.3);
3. the ladder fit and climb (ladder spec);
4. the pool reservation (§3.3);
5. admit / acquire. Admission now returns the external-VRAM verdict (§4.7)
   before it queues, so the fallback can answer without waiting.

The clamp and the counting are shared code.

## 6. Surfaces

- **Model editor:** the `kv_unified` select and the per-slot cap; the
  unguarded-pool note (§3.3).
- **Aliases page, candidate-alias editor:**
  - the primary, and the alternates below it with drag reordering. The label
    says "used only when already loaded";
  - the background toggle;
  - one fallback select for the alias (inherit / none / alias);
  - the capability toggles. Facets that not every candidate supports are
    greyed out, with "not supported by: <ids>";
  - "add candidate" lists only models that support every enabled facet;
  - problems (§4.6) are shown inline.
- **`/v1/models`:** as §4.6.
- **Dashboard runtime and `lmgw__status`:** each entry shows its owner and a
  `draining for owner` state. Each alias shows its deferrals over the last
  24 h, counted from the request log (`gpu_deferred`).
- **MCP:**
  - `lmgw__candidate_alias_set` (create/update/delete/enable/disable; with
    `candidates`, `background`, `fallback_mode`, `fallback`, `capabilities_disabled`);
  - `lmgw__models kind=alias` lists candidate aliases with their lists,
    enabled facets and problems.
- **Settings, VRAM section:** the `fallback_on_external` switch (§4.7).
- **`x-lmgw-fallback-reason`** on every fallback response. The `lmgw__status`
  and `/api/vram` figures are listed in §4.7.
- **README:** "Candidate aliases & background jobs", "Unified KV" and "When
  fallbacks answer" subsections.

## 7. Testing

Uses the `tests/it/vram_admission.rs` fixture. The llama-server mock gains
`/apply-template`, `/tokenize`, `/slots` and a pool that fails like fact 3
when overfilled. A cloud wiremock upstream stands in for fallback cases.

1. **Unified toggle.** `kv_unified` on/off/None renders the right flag.
   Published context for unified vs split. `clear` round trip.
2. **The ledger.** Two requests whose reservations together exceed the pool:
   the second waits and starts when the first ends. FIFO: a large waiting
   request is not overtaken by a later small one. Timeout gives
   `vram_queue_timeout`. Above the per-request limit gives `400`.
3. **Validation.** Explicit unified with `parallel > 1` and no `n_predict` is
   refused.
4. **Non-background alias** (primary P, alternate A):
   - P loaded → P.
   - A loaded, P not → A: no eviction, no podman call.
   - Nothing loaded → P starts with normal admission, evicting an idle
     unrelated model.
   - A is never started by the alias.
5. **Background, primary loaded or startable.**
   - P loaded (an owner model) → served there, header `x-lmgw-candidate: P`.
   - P not loaded but fits free VRAM → P starts, `Background`-owned, even
     though A is loaded.
6. **Background, primary blocked.** P doesn't fit without evicting an owner
   model, and A is loaded → A serves.
7. **Background, nothing usable.** P can't start and no alternate is loaded:
   - fallback alias → served by the cloud mock, with `x-lmgw-fallback`;
   - fallback `none` → `503 gpu_hold`, logged `gpu_deferred`, no eviction.
8. **Background evicts background.** An idle `Background` model is evicted to
   start P; an idle `Owner` model never is.
9. **Owner waits for a busy background model.** The model is marked draining;
   a new background request goes to a loaded alternate or the fallback; the
   in-flight one completes; the owner model starts.
10. **Ownership flip.** An owner request on a `Background` entry makes it
    `Owner`; background can no longer evict it or climb it.
11. **Hold.** Both alias kinds go straight to the alias fallback. A
    candidate's own row fallback is never consulted. `none` gives `gpu_hold`
    (logged `gpu_hold`).
12. **Capabilities.**
    - A new alias enables every common facet.
    - Enabling an unsupported facet is refused.
    - Adding a candidate that lacks an enabled facet is refused, naming it.
    - Removing the odd one out re-enables a facet only if it was not switched
      off.
    - After editing a row to drop vision: that candidate is skipped and a
      problem is reported.
    - An image request to an alias without vision gets `400`.
    - `/v1/models` shows exactly the enabled set and the minimum context.
13. **External-VRAM fallback (§4.7).** FakeGpu gains per-process usage.
    - Outside use leaves too little room and the model has a fallback alias:
      the cloud mock answers immediately, with
      `x-lmgw-fallback-reason: external_vram`.
    - Busy lmgw models in the way: the request queues; no fallback.
    - Model larger than capacity: `vram_too_large`; no fallback.
    - No fallback configured: queue, then `vram_queue_timeout`, as today.
    - Attribution missing for one container, or the switch off: hold-only.
    - A non-background alias uses its alias fallback on the same verdict.
14. **Rows and plain aliases without these features.** Unchanged.

`bash ci/check.sh` is the gate.

## 8. Work packages, and the order across both specs

0. **Measure (GPU).**
   - Reproduce fact 3 on a 2-slot unified row, to confirm the guard is needed
     and learn exactly what the client sees.
   - Find the cheap `/slots` form.
   - Time restoring from the RAM cache after an idle slot was cleared.
   - The counting cost (shared with ladder WP0).
   - Per-process attribution: NVML's process list and DRM `fdinfo` both see
     the llama-server in a rootless podman container, under the PID
     `podman inspect` reports (§4.7).
1. **Unified toggle** (§3.1–3.2): params, argv, published context, editor,
   MCP. It is small and useful on its own.
2. **The shared gate core:** clamp, counting, the `ContextExceeded` error, the
   pool ledger. The ladder's WP3 reuses it.
3. **External-VRAM fallback** (§4.7):
   - per-process attribution (NVML, DRM fdinfo) mapped to container PIDs;
   - the verdict in admission;
   - the settings switch;
   - the reason header and log column;
   - the status figures.

   It is independent of ladders and aliases, and useful for plain models on
   its own.
4. **Ladder models** (their own spec, WP1–WP6).
5. **Candidate aliases:**
   - migration (`candidate_aliases`) and config/store;
   - capability derivation and validation (§4.6);
   - resolution (§4.2–4.3) with the alias fallback;
   - registry ownership;
   - draining for the owner;
   - `gpu_deferred`;
   - the headers.
6. **Tests** (§7). 7. **UI.** 8. **MCP + docs.**
9. **Adversarial review** of each merged piece, then `ci/check.sh`, then merge.
   `docs/release-notes.md` gets the contract changes:
   - migrations;
   - new params and settings;
   - the new headers;
   - the widened fallback trigger, with `vram.fallback_on_external = false` as
     the way back to hold-only.

## 9. Spec choices (veto if wrong)

- **Candidates are local chat models only.** Cloud is reached through the
  alias fallback.
- **A request that uses a facet the alias does not enable is refused** (`400`),
  not passed through. The alias is a contract.
- **The fallback must support every enabled facet too,** since it answers the
  alias's requests.
- **Background may climb only the primary, and only when it owns it** (never
  one the owner uses), and only into free VRAM.

## 10. Later

- Unified KV on ladder models (the ledger makes "wait vs climb" decidable).
- A background flag on API keys, for an app that needs both modes.
- Background for aux models (embedding jobs), where eviction hurts just as
  much.
- Upstream: llama.cpp aborting only the largest slot on overflow (its own
  TODO, fact 3) would soften the unguarded case.

## 11. Out of scope

- Preemption.
- Priority tiers beyond owner / background.
- Time windows.
- A scheduler inside lmgw.
- Cloud candidates.
- Image and audio models.

## 12. Implementation decisions (for review)

Phase 1 (`feat/gate-core`, 2026-09-27): WP1, WP2 and the §5 gate skeleton.
Taken during implementation (not yet reviewed), under "a desktop app for one user". Each is local and
reversible.

1. **The guard needs `kv_unified` explicitly on.** §3.3's activation rule
   ("effective unified") and §1's decision ("rows that run llama-server's
   auto slots are left alone") disagree for an auto row that sets
   `n_predict`. The decision wins: `LlamaParams::pool_guarded()` is
   `kv_unified == Some(true)` && effective slots > 1 && `n_predict > 0`. Auto
   rows keep today's behaviour exactly (no clamp, no count, no queue) and get
   the editor note. One predicate to flip if auto rows should be guarded too.
2. **Save-time refusals are wider than §3.3.** Each one refuses a setting that
   llama-server would ignore, or a pool lmgw could not guard.
   - `parallel` unset or ≤ 0 counts as auto everywhere. llama-server then
     forces 4 slots and unified KV, overriding `--no-kv-unified` (fact 1).
     So `kv_unified = false` is refused unless `parallel ≥ 1` is set.
   - Explicit unified with more than one effective slot needs `n_predict > 0`
     (as §3.3 says, now also for auto slots). It also needs `ctx_size > 0` or
     `kv_unified_per_slot`, because llama-server's `--fit` (on by default)
     shrinks an unset context, so the trained context is not the pool.
   - `kv_unified_per_slot ≤ 0` is refused.
   - A per-slot cap on a split row is refused. llama-server's `--help` says
     the cap only sizes or caps the shared pool, so it would be ignored.
   - Validation runs on the row *after* freeform args are hoisted into the
     typed fields, short spellings (`-kvu`, `-no-kvu`, …) included. So
     `--kv-unified` in freeform args cannot bypass these rules.
3. **Published context.** An auto row with `ctx_size` set publishes
   min(`ctx_size`, trained context). That only changes the number when the
   GGUF's trained context is smaller than `ctx_size`, where the old number
   was unreachable anyway. A row with `ctx_size` unset still publishes
   nothing, as before, because `--fit` decides its real context at start.
4. **The gate is one module with two halves, run 1, 2, 5, then 3, 4.**
   `gate::open` (per request: hold swap, candidate pick, admission) and
   `gate::fit_chat` / `fit_text` (per send: clamp, count, ladder hook, pool
   reservation). Counting needs the *running* server (ladder §3.3 already puts
   the fit check after acquire), and the in-process runners (agent chat,
   `/v1/responses`, MCP sampling, batch jobs, quickdoc) hold one admission
   across many turns, so each turn reserves and releases its own tokens. A
   reservation never lives as long as the admission.
5. **A request queued for the pool keeps its admission,** so the model counts
   as busy and cannot be evicted from under waiting work. That is the fair
   queueing of §1. No deadlock: the pool only waits on releases of the same
   model's reservations, and those requests are already forwarded.
6. **The pool wait reuses `vram.queue_timeout_seconds`** (no new setting). It
   gets its own variant, `KvPoolTimeout`, with the same contract as
   `vram_queue_timeout` (503, code `vram_queue_timeout`, Anthropic
   `overloaded_error`), because the old message says "waited for GPU memory"
   and names the wrong resource. The new one names the pool's numbers.
7. **Capacity and per-request limit come from the running container,** not
   the current row (second review, finding 1): the row it was *started* with is
   captured on the registry entry (`gate::facts::GateFacts`, set where the
   start renders its argv, and on adoption) and read through the hold. An
   edit to a busy model is refused by the apply as busy and the container
   keeps its old pool, so the ledger keeps admitting against that pool until
   the restart; a dead-container recovery picks up the new start's facts.
   Ladders record their running rung in the same place. The pool is
   `ctx_size`, else per-slot cap × slots; the per-request limit adds the
   GGUF's trained context, read at start. If the pool is unknown the request
   gets a 500 naming the fix, never a guess. A request larger than the whole
   pool is refused at once (400); a waiter whose pool shrank below its need
   (the model restarted smaller) is refused rather than left waiting.
8. **No speculative-draft margin in a reservation.** Upstream 171e884 bounds
   drafting by `min(n_ctx - n_tokens - 2, n_remaining - 1)`, so drafts never
   pass prompt + max output, and the gate always sets max output.
9. **Other ways to ask for output are bound too.** llama-server reads a native
   `n_predict` body field before `max_tokens`, so on a guarded row a
   passthrough `n_predict` is folded into `max_tokens` and removed (`-1` counts
   as an unbounded ask and is reported as clamped). Legacy completions fold
   `n_predict`, `max_completion_tokens` and `max_tokens` the same way. With
   `n > 1` the prompt is charged once and the output once per completion
   (child slots share the prompt's cells). A multi-prompt legacy request
   reserves the sum; the per-request limit applies to its largest prompt.
10. **Media on a guarded row.** Images count with a known per-image bound:
    the exact count of a fixed-size projector, Gemma 4's measured ceiling
    (raised by the row's `--image-max-tokens`), or the row's
    `--image-max-tokens`. Without one, an image request gets a 400 naming
    `--image-max-tokens`. Audio input has no bound in v1 and gets a 400 on a
    guarded row.
11. **Counting uses `add_special: true`,** because the completion path
    tokenizes with BOS and `/tokenize` defaults to false (a one-token
    undercount on BOS models). `parse_special` stays at its default (true);
    false inflated a real count from 52 to 93.
12. **llama-server's `exceed_context_size_error` maps to `ContextExceeded`**
    (400, `context_length_exceeded`) on every local row, not just gated ones.
    It used to be a generic upstream 400; the model name is filled in at the
    send sites. The fact-3 overflow body on an unguarded row stays what it
    was (a 502 upstream error). A count that the container answers with an
   HTTP error keeps that status. For example, a 400 from `/apply-template`
   stays a 400, not a retryable 502. When the gate refuses a request after
   clamping it, the response still carries `x-lmgw-max-tokens-clamped`, and
   the log still records the clamp.
13. **Visible:** `kv_pools` in `GET /api/vram` and `lmgw__status` (capacity,
    reserved, in flight, queue). JSON only; no dashboard panel yet.
14. **Known limits, documented in `gate/pool.rs`:** traffic sent straight to
    a container's port bypasses the ledger (no `/slots` cross-check built).
15. **Release only once llama-server has let go** (second review, finding 6). A send
    read to its normal end releases at once. Any other end (client
    disconnect, lmgw-side timeout or stall, upstream error, a dropped send)
    marks the reservation `releasing` in `kv_pools` and polls the
    container's `/slots` until no more slots are processing than the
    reservations still in flight account for (slots counted per prompt × per
    completion); the container gone or `/slots` unreachable releases at once.
    llama-server only notices a closed connection on its next
    `HTTP_POLLING_SECONDS = 1` poll plus one ubatch, which is the overlap
    this closes. Bounded by `vram.queue_timeout_seconds` (0: the route's
    request timeout), with a warning when hit. The upstream response is
    always dropped before the lease ends.
16. **In-process deadlines include the gate** (second review, finding 7). MCP
    sampling's `SAMPLING_DEADLINE` and the agent loops' wall clock
    (`responses_timeout_seconds`, via `/v1/responses`, Admin/agent chat,
    quickdoc, batch runs) all reach `sample_once`/`stream_once` as one
    deadline measured from the call's start; the count and the pool wait end
    at it (a pool wait cut short is a named `KvPoolTimeout`), and the send
    gets only what is left.
17. **Every ledger change is on the live frame** (second review, finding 11): grant,
    release, deferral and queue moves each push the `vram` frame.

**WP0 measurements (2026-09-27, qwen3.5-0.8b, llama.cpp 171e884):**
- Fact 3 reproduced on 2 slots, unified, pool 4096: two streams died
  together after 5.2 s (2048 + 2049 cells). Streaming clients saw HTTP 200,
  then `data: {"error":{"code":500,"message":"Context size has been
  exceeded.","type":"server_error"}}` and no `finish_reason`; non-streaming
  clients got a 500 with that body.
- `/props` `default_generation_settings.n_ctx` is the per-request limit (4096
  unified, 2048 split at `-np 2`). It has no trained context.
- **`/slots` has no per-request cheap form.** The detokenized prompt is
  included only when the container runs with `LLAMA_SERVER_SLOTS_DEBUG=1`,
  which lmgw never sets, so lmgw's `/slots` is already the cheap form (about
  2.5 KB against about 20 KB with the variable).
- `/apply-template` + `/tokenize` cost: 21 ms at 1.6k tokens, 456 ms at 37k,
  1.7 s at 123k (median; `/apply-template` is about three quarters).
- Not measured yet: the RAM-cache restore time. Per-process attribution was
  measured for phase 2 (below).
- **Disconnect overlap, measured after the fix (3 runs).** llama-server
  freed the abandoned slot 8–14 ms after the client died (its log shows
  `cancel task` → `release` about 2 ms apart). The waiting request's first
  byte came 279–281 ms after the kill. That delay is the deferred release's
  250 ms `/slots` poll-and-confirm cycle; there was no overflow.
- **Edit while busy, live.** Raising `ctx_size` from 4096 to 8192 on a busy
  guarded row saved with "keeps running the previous configuration". The
  ledger kept admitting against 4096 until the apply, then showed 8192.

**Live check of the guard (2026-09-27, same model, 2 slots, pool 4096,
`n_predict` 2048), through a dev instance:**
- The pair that made llama-server abort both requests now completes. Each
  request was clamped to 2048 (header set). The second request waited 3.7 s
  for the first, and "Context size has been exceeded" never appeared in the
  log.
- Of three requests of 1525 tokens each, two ran together and the third
  waited.
- Above the limit: 400 in 12 ms, in both dialects.
- Queue timeout (3 s): a 503 whose message names the pool's numbers.
- The count matched `usage.prompt_tokens` exactly (25 = 25).
- On a short prompt the guard added about 2 ms to time-to-first-byte.

Phase 2 (`feat/external-vram-fallback`, 2026-09-27): §4.7.

**WP0, per-process attribution (2026-09-27, RTX 4090, driver 615.71.09,
rootless podman, qwen3.5-0.8b):**
- `podman inspect --format '{{.State.Pid}}'` of the chat container is the
  llama-server host PID, and NVML's compute list shows exactly that PID
  (`/app/llama-server`, type C only, never G) within about 1 s of the first
  answer.
- Device used delta 1075 MiB, NVML per-process 1070 MiB, lmgw's estimate
  576 MiB. The per-process figure is live (1070 → 1074 MiB while
  generating).
- `/proc/<pid>/fdinfo` is readable as the same user; the NVIDIA driver
  publishes no `drm-*` keys there (fdinfo is the AMD/DRM path only).
- The desktop's own processes (kwin, VS Code) are listed too: outside use.
- Not checked live: amdgpu (no AMD GPU here; fixture tests only).

18. **Pooled like the ledger.** The verdict uses the ledger's pooled
    numbers: its capacity (`budget_mb` or the devices' total), free summed
    over devices, and lmgw's share as the measured bytes of every lmgw
    process on every device. There is no model-to-device mapping anywhere in
    lmgw, so a per-device verdict would be a guess about placement. `needed`
    is exactly today's (footprint + headroom). The table runs in §4.7's
    order: `needed > capacity` is today's `vram_too_large` first (decided
    before any attribution), then `free + lmgw_share ≥ needed` is today's
    path, otherwise External. "Free" is the measured free *before* the
    ledger subtracts learned image peaks and arrivals: with every lmgw model
    gone those are gone too (arrivals make the verdict unavailable anyway,
    entry 22).
19. **Container → processes, asked once per container.** `State.Pid` from
    one batched `podman inspect --format '{{.Name}} {{.State.Pid}}' n1 n2 …`,
    plus every descendant of it, walked down `/proc/<pid>/task/*/children`
    (a `/proc/*/stat` scan where the kernel lacks those files). That covers
    the image class, where `State.Pid` is catatonit and sd-server its child.
    The PID is cached per registry *generation*, a new number on every claim
    and adoption, so a re-run container is never matched to an old PID. A
    pass with every PID cached spawns nothing. A verdict retries what is not
    known yet or failed. The status view asks at most once per generation,
    never waits for the answer (that frame says "being read"; the inspect
    task pushes a fresh frame) and never retries a failure (the next verdict
    does). The inspect runs in its own task, so a caller that goes away still
    leaves the answer cached.
20. **NVIDIA.** Compute and graphics lists per device, merged per PID by max:
    a C+G process is listed in both with the same allocation. Repeats inside
    one list happen only under MIG and are added. Devices are summed, like
    the ledger. `UsedGpuMemory::Unavailable` stays unknown, never 0: an lmgw
    process the driver cannot size makes attribution unavailable. A device
    that answers NotSupported for the graphics list has an empty one.
21. **amdgpu.** fdinfo of lmgw's own PIDs only, fds with
    `drm-driver: amdgpu`, one count per `(drm-pdev, drm-client-id)` across
    all of them, since a dup'd or inherited fd is the same client.
    `drm-pdev` is matched to the probed card's `PCI_SLOT_NAME` (its uevent,
    else the name the `device` link resolves to). Both key generations are
    read, and `drm-resident-*` wins over the legacy `drm-memory-*`; units
    are bytes, KiB or MiB (GiB accepted). GTT counts exactly on the cards
    whose capacity counts it (APUs), so share and capacity use one
    definition. A client with no memory keys is unknown.
22. **Unavailable means hold-only, today's path.** That is the case when no
    probe answers or telemetry is not ok, when the probe cannot list
    processes, when `vram.enabled` is off, when the switch is off, or when
    the hold is on. It is also the case when a start is not measured yet: a
    `starting` entry, or a reservation whose model has no registry entry.
    Finally, it is the case when any registry entry (`ready` or `stopping`)
    has none of its processes in the driver's list, or one listed without a
    figure. The reason names the model, so a CPU-only row, or audio.cpp on
    CPU, disables the trigger visibly while it runs. Conservative on
    purpose: a wrong External sends to the cloud a request lmgw could have
    served; a wrong Unavailable only costs today's queueing.
23. **Tombstones for just-stopped containers.** A generation that leaves the
    registry turns the PIDs last measured under it into tombstones. They
    count as lmgw's share while the driver lists them, and are dropped the
    first time it does not. A tombstone listed without a figure makes the
    share unavailable. A container evicted before any pass cached its PID
    has no tombstone. That is accepted: a verdict caches every resident's
    PID before its admission can evict, and the armed status view caches
    them as soon as it sees them.
24. **Only computed when it can matter.** `vram::admit_or_external(state,
    route, alias, fallback: Option<ExternalFallback>)` is `admit` exactly
    without a fallback: no probe, no inspect. With one, the checks that cost
    nothing come first: the switch, `vram.enabled`, the hold, boot settled
    (entry 37), and the model being up (`ready` or `starting`, which is
    `Fits`). The first verdict is not taken under the admission gate,
    because a waiting request holds the gate for its whole wait, and
    External exists to answer without waiting; the re-verdicts of a request
    already waiting are (entry 44). It needs no claim: an admission that
    starts meanwhile shows up as a reservation, which makes the next verdict
    Unavailable, never a wrong External. `admit` keeps its signature and
    behaviour.
25. **The setting.** `vram.fallback_on_external: bool`, serde default on, so
    a stored blob without the key reads as on. Backend and api-types mirror
    only; the Settings switch and MCP exposure come later. Default on changes
    behaviour on upgrade for installs that have fallbacks configured (fact
    15), so the release notes must name the switch (WP 9).
26. **Visible.** `GET /api/vram` and `lmgw__status` (`VramView`) gain
    `lmgw_share_bytes` and `outside_share_bytes` (pooled used − share), both
    `null` when not measurable, plus `external_trigger_active` and
    `external_trigger_reason`. They are measured only while the trigger is
    armed. `admit_or_external` logs one `info` line with needed, free, lmgw
    share and outside whenever it answers External.
27. **The reason travels with the fallback.** `gate::FallbackReason`
    (`hold`, `external_vram`, `background`; the last is phase 4's) sits in
    `GateHeaders` as one `(alias, reason)`, so `x-lmgw-fallback` and
    `x-lmgw-fallback-reason` are always written together by `stamp`, on
    every path it already covered (errors and streams included). An alias
    that cannot be a header value is logged and neither header is written.
    The hold's swap in `gate::resolve` sets `hold`. `proxy::with_fallback`
    is folded into `stamp`. The `/v1/models` `lmgw.headers` block and the
    README header table document it.
28. **The swap at admission.** `gate::open::admit_or_fall_back`, for a route
    nothing has swapped yet, asks `gate::usable_fallback` (sync, cheap):
    first `vram::external_armed` (a local row, the model not up, the switch
    and admission on, the hold off; free, no probe), then
    `Snapshot::fallback_route` (the row lookup factored out of
    `resolve_for_request`, which the hold now uses too). A fallback found
    there goes to `admit_or_external` with the site's kept `RouteCheck` on
    the fallback's route deferred (`gate::fallback_serves`, run at most
    once and only on an External verdict, entry 42); no fallback is
    `vram::admit` exactly. `External` returns the fallback's route with no
    hold and the headers set to `external_vram`. A fallback that fails the
    endpoint's check counts as none (a debug line): the local model may
    still serve the request, so it queues rather than erroring as it would
    under the hold. One that does not resolve or is itself local gets a
    warning naming it and takes today's path; the warning fires only for
    requests that could have fallen back, not for every request to a loaded
    model.
29. **The non-gate sites go through the gate.** Embeddings, rerank, the
    audio routes (speech, tasks, uploads, voices) and the image routes call
    `gate::open`; `audio_voices_if_running` calls `gate::resolve` only,
    because it never admits. Their pre-admission checks are new `RouteCheck`
    variants, re-run on a swapped route: `Embeddings`, `Rerank` (media
    check plus section guard), `Audio`, `AudioVoices` (protocol, voices),
    and `Image(endpoint)` (protocol, image row and `edit`, or the cloud
    catalog guard). `run` is async because the image guard reads a catalog.
    The aux section guards still log the refused route's `upstream_*`
    columns. `Served`, `Failed`, `MediaOutcome` and `count_tokens_inner`
    carry `GateHeaders`, and `/v1/count_tokens` stamps through them. No
    request-shaped site calls `vram::admit` any more. The direct callers
    left are the quickdoc batch runners (ingest, golden), which `admit_local`
    refuses under the hold, `local_model_test` and `LocalHold::recover`.
30. **Pinned callers never swap at admission.** `Routed::admit_pinned` and
    `gate::open_pinned` wait for room as before §4.7. Their callers are
    `embed_once` and `rerank_once`, whose only callers are quickdoc's
    `InProcessEmbedder`/`InProcessReranker`, and `count_tokens_inner(pinned)`
    for ingest's window sizing, where a fallback's tokenizer would size the
    windows wrong. The hold is unchanged for them: it swaps at resolve, and
    quickdoc refuses it itself before and after the call.
31. **Policy parity (finding).** Scope and budget are checked on the
    requested alias before the gate at every site that checks them
    (`policy_or_refuse`, `/v1/responses`' `check_alias`, `check_internal` in
    `sample_once`), and none of these checks reads route locality.
    `price_call` prices the served route: a local upstream is free,
    otherwise the requested name's alias price row, then the fallback's
    upstream/model row. That is the same for the hold and for the external
    swap, so nothing needed fixing. Two pre-admission reads of the route are
    not policy and stay as they are:
    - `/v1/responses` picks native or synthesized from the resolved route's
      `supports_responses`. A local route never has it, so an external swap
      always runs the synthesized loop against the fallback's chat egress.
      A hold swap to a `supports_responses` fallback passes through
      natively instead. Kept, because the loop is what the local model's
      client asked for, and the stored chain is already loaded by then.
    - The dashboard chat's `model_vision` gates image attachments on the
      hold-effective alias only.
32. **The log column.** Migration `0042_fallback_reason.sql` adds
    `request_logs.fallback_reason TEXT`, NULL when no fallback answered.
    Every logging site fills it from its `GateHeaders`:
    - the handlers (`LogParams.fallback`);
    - the in-process rows (`InProcessLog.fallback`). `sample_once` and
      `stream_once` take it from the caller's admission: the `/v1/responses`
      runner, agent runs, MCP sampling, and agent chat, which now keeps the
      `Opened` headers it used to discard. The dashboard chat passes it
      through `record_chat_call`;
    - the free-form rows (`record_passthrough`, `record_request_failure`),
      which are `/v1/responses`' native passthrough and its refusals after
      a route was settled.

    Hold-served rows now say `hold`; older rows stay NULL. The column is
    also on the live request frame (`RequestSummary`), so a live row reads
    like a reloaded one. `RequestRow`, `/api/logs` and `lmgw__logs` carry it,
    and the tool's description names it.
33. **PIDs are read at ready (entry 23's edge closed).**
    `VramScheduler::cache_pids` runs while the trigger is armed and the probe
    lists processes. It plans the PID cache over every non-starting entry
    with the view's rule (ask only never-asked generations) and runs one
    background `podman inspect`. It also walks the processes under each
    init, so an image container's tombstone holds its sd-server. Callers:
    `admit_local` when the model came up, `ops::start_model`, and boot, once
    after adoption and once after the warm starts. A starting entry is left
    out; it never has a slot, so nothing is retired early. With the switch
    off, a start reads no PID.

Phase 2 surfaces (`feat/external-vram-fallback`, W2), for review:

34. **Migration numbers.** `0042_fallback_reason.sql` (entry 32) is this
    phase's migration; it landed while the ladder spec was still a draft, so
    that spec's own migration (`local_models.ladder`, `request_logs.rung`)
    becomes `0043` (ladder spec §12.1, corrected).
35. **MCP exposure.** `vram.fallback_on_external` was not reachable over MCP
    (entry 25 deferred it). It now sits next to the hold block rather than
    under a new top-level key: `lmgw__settings` gains a `"vram":
    {"fallback_on_external"}` block beside `"hold"`, and
    `ops::SettingsPatch`/`lmgw__settings_set` gain the matching
    `fallback_on_external: bool`. The rest of `VramSettings` stays
    unreachable here, unchanged from entry 25.
36. **UI placement.** The Settings switch lives on the existing GPU ×
    admission card (`crates/lmgw-ui/src/pages/settings.rs`), next to
    `vram.queue_timeout_seconds` and above the hold group — declarative, so
    the patch round-trips through the same flatten/nest the other `vram.*`
    fields already use. The titlebar GPU popover
    (`crates/lmgw-ui/src/shell.rs::GpuPanel`) gains the two shares and the
    trigger's armed/off state next to the device row; `VramStatus`
    (lmgw-api-types) needed `lmgw_share_bytes`, `outside_share_bytes`,
    `external_trigger_active` and `external_trigger_reason` added to mirror
    entry 26's `VramView` fields, since it had not been (`#[serde(default)]`
    keeps an older frame readable). The request log
    (`crates/lmgw-ui/src/pages/traffic.rs`) gets a small amber badge next to
    the served upstream whenever `fallback_reason` is set, tooltipped with
    the raw reason — one component, so the live-prepended row and a reloaded
    one render identically.

Phase 2 review (`feat/external-vram-fallback`), for review:

37. **No verdict until boot has settled** (review, finding 1). `lifecycle::boot`
    runs unawaited, so requests are served while reconcile adopts what a
    previous lmgw left running; until then those containers are not in the
    registry and their memory read as outside use. `VramScheduler::
    boot_settled` starts false; boot sets it through a drop guard once
    adoption and the legacy sweep are over, however they ended (no podman,
    a failed sweep, a panic), so it cannot stay unset. Until then the
    verdict and the status view are Unavailable, "boot reconciliation still
    running". `cache_pids` after adoption is not gated on it. The test state
    starts settled.
38. **One moment for free and share** (review, finding 2). The measurement
    takes the registry and every PID first (the only part that may wait),
    then on one blocking thread: the processes under each init, the process
    list, the devices, the process list again, per PID the larger figure
    (unknown in either reading stays unknown). Capacity and free come from
    that device read by the ledger's rule (one `pooled` helper for both).
    Then the registry's (generation, state) set and the reservation ids are
    read again; any change is Unavailable, "lmgw's containers changed while
    measuring". The in-flight count and idle age are not compared.
39. **Tombstones settle against what was asked** (review, finding 3).
    `GpuProbe::processes(own, retired)` takes lmgw's live PIDs and the
    tombstones apart. `settle` gets the asked set and drops only tombstones
    asked about and not listed, so one retired by a concurrent pass after
    this pass read the list survives to the next. amdgpu treats any fdinfo
    error but NotFound on a retired PID (EACCES on a reused PID) as not
    listed; on a live member it stays an error, so Unavailable.
40. **One bounded inspect per read** (review, finding 4). A pending PID slot
    carries the read in flight; a verdict joins it instead of spawning
    another, so N requests after a start share one `podman inspect`. Each
    read runs in its own task bounded by `vram::CONTROL_TIMEOUT` (5 s, the
    `/slots` control bound, now also documented for this); past it the slot
    records a failure naming the bound, the verdict is Unavailable, and the
    next verdict retries. The waiter's own wait has the same bound.
41. **An inspect answer counts only for a live generation** (review,
    finding 5; corrects entry 19's "never matched to an old PID"). The
    registry records no container ID, so the answer is kept only for
    generations still in the registry when podman answered. That is exact
    for the race: a name is only re-run after its previous generation left
    the registry, so an answer carrying the next generation's PID always
    finds the asking generation gone. The price is no tombstone for a
    container stopped in the microseconds between podman's answer and the
    record, which entry 23 already accepts.
42. **The fallback's route check runs after the verdict** (review, finding
    6; entry 28 updated). For images it is a catalog read, a network fetch
    on a cold cache, which every cold image request paid even when it fit.
    `ExternalFallback::confirmed_by(check)` defers it to the first External
    verdict; a refusal makes the fallback none for the rest of the
    admission (today's path). `usable_fallback` lost its `check` parameter
    and is sync.
43. **No images to a blind fallback** (review, finding 7). Sites that know
    their request carries image parts (`gate::media_parts`) say so with
    `Routed::carrying_images`: `/v1/chat/completions` and `/v1/messages`,
    the dashboard chat and agent chat, `/v1/responses`. For such a request a
    fallback whose exposed capabilities say `vision: false` is not used for
    the external swap, so it waits for the local model as before §4.7;
    `None` (unknown) stays usable. Checked inside the deferred check, so it
    costs nothing when the verdict is not External. The hold's swap is
    unchanged (there is no local model to wait for). MCP sampling and agent
    runs do not set the flag.
44. **The verdict is taken again while waiting** (review, finding 8). The
    spec's principle, waiting does not help when the shortfall is outside
    lmgw, holds during the wait too: a request that queued as Unavailable
    (a start in flight, an unattributed container) or Fits has the verdict
    re-taken every `POLL` while its fallback is still usable, behind the
    admission gate (one pinned lock future, so its FIFO place is kept) and
    in the wait for room (then under the gate it holds). External leaves the
    queue with nothing reserved and the fallback answers; the log line says
    "while it waited". Re-verdicts never re-ask podman for a failed read
    (`Fill::Recheck`), so the steady state spawns nothing. A request that
    evicted an unattributed container on today's path can therefore fall
    back once only outside use is short.

Phase 4 (`feat/candidate-aliases`, 2026-09-27): candidate aliases (§4.1–4.6,
WP5–8). Taken during implementation (not yet reviewed), under "a desktop app for one user".

45. **A background candidate that cannot take the request is skipped at the
    send, and the walk goes on.** Sites call the gate with a name only, and
    counting needs the running server (entry 4), so "able to take this
    request" (§4.1) is known only at the send, after the claim. When a
    background request's send finds the chosen candidate cannot take it — a
    climb it may not make (an `Owner` ladder, or a rung that does not fit free
    VRAM, §9), or a prompt above a non-ladder candidate's context
    (llama-server's own refusal before any work, the backstop of entry 12) —
    nothing has reached the client yet, and the gate re-runs the pick with
    that candidate excluded: the next loaded candidate in list order,
    fit-checked the same way; only when every loaded candidate is excluded,
    the alias fallback (`background`), or `503 gpu_hold` logged
    `gpu_deferred` with none. The walk is bounded by the candidate list, so
    it needs no cap. Reasons: it is the owner's literal order ("if any
    candidate is loaded use that, otherwise use fallback"), and a local
    answer keeps the data on the machine and costs nothing. Owner requests
    are unchanged: they may climb and load the primary, so only a request
    above the alias's published context (the minimum of its candidates, §4.6)
    can fail there, and it gets that model's `400`, as a direct request would.
46. **Background never waits for the owner** (revised by entry 91). Its
    start takes the admission gate with a try-lock: a gate held by another
    admission (an owner waiting for room among them) counts as "cannot start
    without disturbing the owner", and the walk moves on. Background traffic
    never enters the admission queue. What it does wait for is background
    traffic's own start or climb of the same model, which it then uses
    (entry 91). A loaded candidate is joined, never started, by the claim
    that uses it:
    if it stopped between the pick and the claim, the walk moves on instead
    of that claim starting it unarbitrated (both modes; the alias never
    starts an alternate).
47. **Draining covers every resident model while an owner admission waits
    for room.** §4.5 names busy `Background` models, but an `Owner` model kept
    busy by background requests would hold up the owner's wait the same way,
    possibly for ever under steady background load. So while any owner
    admission is in its make-room wait, background picks skip every resident
    model (they go to a loaded non-draining alternate, else the fallback);
    requests in flight finish. The mark is cleared when the last such waiter
    leaves, however it leaves. Owner waits are rare and short, and during one
    the GPU is contended by definition.
48. **Ownership.** `Background` only for a container a background request's
    primary start created (and that request's dead-container restart). Any
    non-background claim, joins included, makes the entry `Owner` until the
    container stops. Boot adoption, warm and operator starts are `Owner`. A
    climb keeps the entry, so it keeps its owner.

Phase 4, W1 (`feat/candidate-aliases`, 2026-09-27): the data layer — migration
0044, store, `config::CandidateAlias`/`Snapshot`, the new `candidates` module,
`ops::candidate_alias`, the MCP tool, `/v1/models` publishing. Taken during implementation
(not yet reviewed), under "a desktop app for one user".

49. **The enabled set is persisted; `capabilities_disabled` stores only the
    owner's explicit switches, never auto-filled** (corrects this entry's
    first draft, which materialized common's complement into the column on
    a bare `create` and so could never tell a real switch from a facet that
    was merely uncommon — rule 2 could then never re-enable itself).
    `capabilities_enabled` = `common.minus(capabilities_disabled)`, computed
    and stored at every save, never recomputed at read time (a candidate
    edited afterwards to drop a facet leaves `routable`, not a silently
    shrunk contract). A save is refused, naming the facet and the candidates
    lacking it, only against the row's *previous* save, and only for a facet
    neither common now nor in this save's `disabled`: it was in the previous
    enabled set (rule 3: a candidate dropped support and the owner did not
    switch it off — "switch the facet off first, then add the candidate"),
    or it was in the previous disabled set and this save no longer names it
    (rule 1: the owner is switching it on, and nothing supports it). A fresh
    `create` has no previous save to compare against, so nothing is ever
    refused there: an uncommon facet is simply left off, and turns itself on
    the moment it becomes common unless the owner names it (rule 2).
    (`candidates::validate::resolve_enabled`, `crates/lmgw-core/src/
    candidates/validate.rs`.)
50. **The positive-support rule for `reasoning` is not `reasoning.is_some()`.**
    Every local chat row publishes a `reasoning` object unconditionally, even
    with no thinking markers, so `is_some()` would make the facet trivially
    supported everywhere. The rule: `kind` `toggle`/`levels` (any default), or
    `fixed` with `enabled == Some(true)`; `fixed` + `enabled: false` is the
    no-markers shape, and a cloud row's absent `reasoning` needs no special
    case. (`candidates::supports`, `facets.rs`.)
51. **Non-enabled facets publish an explicit negative where the schema has
    one, else are simply absent.** `vision: false`, `input_modalities`
    omitting `image`/`audio`, `tool_calls.kind: "none"`, both
    `structured_output` flags `false` — a candidate alias states these
    outright, since its capabilities are a deliberate contract, unlike a
    plain row's usual "unread" absence. `reasoning` has no negative shape
    (`fixed`+`enabled:false` is a model fact, not a refusal to reason), so a
    disabled reasoning facet is left absent instead.
    (`capabilities::exposed::candidate_alias_entry`.)
52. **Deleting or disabling a candidate row, or the alias named as the
    fallback, is not refused** — exactly as instructed, and consistent with
    a row's own hold fallback already being re-validated at use rather than
    pinned at save time. The alias' next `derive` reports it as a problem
    (`'<id>' is disabled`, `fallback '<fb>' does not resolve: treated as
    none`) and the gate skips it; nothing about the alias' own row is
    touched by another row's edit.
53. **`Snapshot::resolve` refuses a candidate alias name outright, never
    resolves one.** Which candidate answers is a per-request decision the
    gate worker makes, not a static `Route` — so `resolve` (and everything
    through it: `resolve_for_request`/`usable_fallback`/
    `validate_fallback_alias`) checks `Snapshot::candidate_alias` first and
    returns a named `BadRequest` instead of falling through to
    `UnknownAlias`. Every fixed-model caller (quickdoc, corpus creation, the
    dashboard chat picker, a row's own fallback validation) gets that
    refusal for free.
54. **Name uniqueness is one new direction, not two.** A candidate alias'
    own save checks all three other name spaces (plain aliases, every local
    class's public name, other candidate aliases). The reverse — a plain
    alias or local row refusing a name already taken by a candidate alias —
    is the one new refusal on those existing save paths. Whether a plain
    alias and a local row's public name can already shadow each other is a
    pre-existing gap, out of scope.
55. **`Snapshot.candidate_aliases` is keyed by lowercased name**, unlike
    `Snapshot.aliases`'s literal-case key (a pre-existing inconsistency, not
    copied forward) — every uniqueness/lookup helper here normalises to
    lowercase at the lookup instead.
56. **`Snapshot::usable_fallback` gained a third refusal clause**, "is itself
    a candidate alias" — checked first, before `resolve` even runs (a
    candidate alias has no route `resolve` could return anyway). Both
    `ops::validate_fallback_alias` (save time) and every request-time
    fallback lookup (`fallback_route`, and this phase's `alias_fallback`) go
    through this one function, so "a fallback may never be local and never
    another candidate alias" is one rule, not two copies that could drift.
57. **The gate worker's per-request query is `candidates::derive::
    cached_pick`**, not a cheaper reimplementation of `derive` — the win is
    not repeating the same cached GGUF/catalog reads every request. A small
    in-process cache (`(Arc<Snapshot> pointer, alias name) -> CandidatePick`)
    is filled on first use after a reload and dropped whole (not
    entry-by-entry) the moment the snapshot pointer changes — reloads are
    rare, and one entry is a handful of small strings, so no size bound was
    needed.
58. **`capabilities::exposed::local_row_entry`** factors `entry_for`'s
    `"local"` arm to take a `&LocalModel` row directly, since a candidate
    need not be `public` (unlike `Snapshot::exposed_models`'s `"local"`
    tier) — same cached reads, correct for a private candidate too.
59. **One `Box::pin` indirection**, in `candidates::derive::fallback_supports`,
    breaks a call cycle the compiler sees statically (E0733) but that never
    actually recurses (a fallback can never itself be a candidate alias,
    entry 56) — the standard fix for an async cycle the type system cannot
    see is finite.
60. **`ops::candidate_alias_set`'s response is a fresh re-derivation, not an
    echo of the patch.** Every write reloads the snapshot and re-derives
    before answering, so the response matches what the very next
    `lmgw__models`/`/v1/models` call would show — no second round trip
    needed.

Phase 4, C1 (`feat/candidate-aliases`, 2026-09-27): the runtime/VRAM half —
ownership, the join, the guest's start, restart rules, draining, the guest's
climb (`runtime/registry/ownership.rs`, `vram/background.rs`).

61. **The origin is a parameter, and joining a start is a claim.**
    `Registry::acquire` stays the owner's and `acquire_as(spec, origin)` takes
    the origin; it is not an `AcquireSpec` field, so no existing caller or
    literal changed and a climb's `StartSpec` never carries one (a climb keeps
    the entry, so it keeps its owner). An owner claim flips the entry in the
    same lock hold as the claim, and already when it parks on a start or a
    climb in flight: the status shows whose model it is while it loads, and
    a guest's eviction judged in that window cannot take it. A `stopping`
    entry is never flipped. Boot adoption is `Owner`.
62. **A joined start that fails, or that a stop lands on, is an error from
    the join, and the walk never starts that primary again.** The join
    answers `None` for no entry, a `stopping` one, a guest while draining, a
    start abandoned by its client and a failed climb; but a start it joined
    that fails is that start's 502, exactly as for `acquire`'s waiters. The
    walk counts it as not loaded (entry 74) and remembers it: a primary whose
    joined start failed is never followed by "load the primary" (§4.2 step
    3) or a guest's start in the same walk, which would pay a doomed load
    twice or bring back a model somebody had just stopped (review R,
    finding 1).
63. **Draining is a registry counter, armed at the first shortfall.** It
    lives in the registry because a guest's join reads it under the map lock
    it claims in and `list()` publishes it; it is an RAII count nobody awaits,
    so it cannot deadlock with the admission gate, a climb's gate yield or a
    climb's drain. `decide` takes it on the first look that does not fit,
    before any eviction, so guests stop turning its eviction victims busy.
    An owner climb's make-room loop takes it too; a guest's climb never gets
    there. It is armed only when `Snapshot::any_background_alias()`, so the
    frame of an install without guests never shows it. Accepted (H4): a
    guest's in-process run (agent run, `/v1/responses` loop) holds one claim
    for the whole run, and an owner waiting for its model waits for the run.
64. **A guest yields to any queued admission, and a guest never measures
    a card it cannot measure.** `start_background` joins a loaded primary,
    otherwise takes its model's guest turn (entry 91) and try-locks the gate,
    and is `Blocked` while the owner drains or an owner admission is queued
    (background traffic's own queue rows, a guest climb's, never count:
    entry 91), checked again after each eviction, so a guest holding the gate
    gives it up after at most one eviction. Admission off, or no telemetry and no budget, is
    `Blocked` naming `vram.budget_mb`: a guest cannot promise not to disturb
    the owner on an unmeasured card, and loaded models still serve it. A
    primary larger than the card is `vram_too_large`, as §4.7 rules.
    **(Superseded by entry 74 for a guest: the walk goes on to the loaded
    alternates, then the fallback, instead of ending in that error.)** Its
    evictions use `stop_idle_background`, which re-checks the owner under the
    lock, because the victim's `/slots` probe can take seconds and the owner
    may have claimed and released it meanwhile.
65. **Each hold carries a restart rule, and "no" is `candidate_lost`.**
    `Restart::{Admit, Background, No}` is set at the claim: `admit` gives
    `Admit` (every existing caller unchanged), `start_background` gives
    `Background`, and `join` takes it from the gate (`No` for an alternate).
    A dead container's recovery and a sync that finds the entry gone restart
    by that rule; `No`, and a guest's restart that is `Blocked`, end in
    `GatewayError::CandidateLost {model, detail}`, which `retry_dead_container`
    and the ladder send pass through unwrapped so the gate can pick again. If
    it escapes it is a 503 with kind `candidate_lost`: the model the request
    was on is gone, and the next request picks afresh. Not a refusal kind in
    the rollup, since nothing was declined on purpose.
66. **A guest's climb: the card size first, the running rung counts as
    freed, and the rules are asked again after the drain.** The order is the
    hold, `vram_too_large`, then `may_climb`, so an impossible rung stays a
    configuration error for guests too (logged as one; the gate then picks
    again, entry 92). "Free VRAM" is the measured free plus
    the running rung's own footprint: the climb stops that rung, and it
    evicts nothing else. The drain still waits for the model's own sends,
    which is the fair FIFO on a chosen model. The post-drain admission
    try-locks the gate and re-checks ownership: an owner who parked on the
    climb made the model theirs, so the climb is denied, the mark clears and they
    are served by the running rung at once. A guest's climb takes no
    outside-VRAM verdict; the gate answers the denial. §9's "only the
    primary" is read from `Snapshot::candidate_alias(hold.alias())`.
67. **The wire code and the log kind part ways once.** `to_openai_json`
    wrote `kind()` as its `code`, and a deferral must be `gpu_hold` for
    clients and `gpu_deferred` in `request_logs.error_kind`, so
    `GatewayError::code()` exists and differs from `kind()` for
    `GpuDeferred` alone. `gpu_deferred` joins `telemetry::REFUSAL_KINDS`.
68. **An interim answer for `Climbed::Denied`.** Superseded by entry 73.

Phase 4, C2 (`feat/candidate-aliases`, 2026-09-27): the gate half —
`gate/candidate/{mod,walk,uses}.rs`, glue in `gate::open`, `gate::send`,
`gate::fit` and the send sites.

69. **`resolve` hands back a local chat route; admission picks.** Under the
    hold the alias fallback answers at `resolve` (reason `hold`), or
    `gpu_hold` naming the alias. Otherwise `resolve` returns the first
    routable candidate's route, else the primary's, and claims nothing. The
    sites that read the route between the halves (`/v1/responses`' native
    choice, the chats) then see a local model, as for a direct name. Every
    candidate is a local chat row, so the site's route check answers the same
    for each. Which candidate answers, or the background fallback, is decided
    at admission, under the same "may still swap" contract as §4.7.
70. **An owner alias with nothing it may load answers from its fallback,
    reason `unavailable`** (rewritten after review R, finding 6: decided for
    the owner's literal order, which ends "otherwise use fallback"). When the
    primary is not routable (a lost facet, disabled, deleted) and no
    alternate is loaded, the alias fallback answers with the new
    `FallbackReason::Unavailable` (`x-lmgw-fallback-reason: unavailable`, the
    same word in `request_logs.fallback_reason`), and the drift is logged as
    a warning so it is fixed, not lived with. With no fallback that can
    answer (none, unusable, or lacking an enabled facet), it is
    `GatewayError::CandidateUnavailable`: 503, kind `candidate_unavailable`,
    in `REFUSAL_KINDS`, naming the alias, the primary and why. A background
    alias in the same state goes to its fallback with reason `background`,
    since the primary cannot be loaded.
71. **The facet 400 is `Routed::using(FacetSet)`**, called between `resolve`
    and admission. It refuses at once with `unsupported` (400) naming the
    facet, under the hold too. `/v1/responses` calls it before its native
    branch, which never reaches admission. What counts as a use:
    - image and audio parts;
    - tool definitions the model is given, which the sites add: the client's
      and MCP tools on `/v1/responses`, always on agent chat, and on agent
      runs whose manifest attaches tools;
    - a `json_schema`/`json_object` `response_format`, or llama.cpp's
      top-level `json_schema` (not `grammar`);
    - reasoning only when the request asks to think (H3).

    Under the hold with no fallback, `resolve`'s 503 comes before the 400
    (H2).
72. **One place judges a candidate alias's fallback: `fallback_serves`,
    keyed by the requested name.** A fallback that does not positively
    support every enabled facet counts as none on every path: the
    outside-VRAM swap, a climb's, the hold during a climb, and the background
    walk. The hold at `resolve` checks it itself, and names the facet in the
    `gpu_hold` detail.
73. **The re-pick hands the site a whole admission.** `Sent::Fallback` became
    `Sent::Rerouted(Result<Opened, OpenFailed>)`, so every site has one arm
    for a climb's fallback and for a re-pick. `Err` carries fresh headers, so
    a deferral after a skip never names the skipped candidate. The excluded
    list rides on `AdmissionPolicy.candidate` (now `Clone`). Re-pick
    triggers:
    - `candidate_lost` (both modes);
    - for guests, `ContextExceeded` — a guarded pool's limit, handed from the
      fit to the send on the lease (`TurnLease::skip`) rather than refused,
      or a ladder's top rung — and llama-server's backstop 400 on a non-ladder
      send, held back for guests only;
    - `Climbed::Denied` (replaces entry 68's interim);
    - for guests, a climb that fails, and a ladder's second backstop 400
      (entry 92).

    The walk re-runs from the top with the exclusions, so a guest whose
    alternate refused may still start its primary. Bounded by the list and
    by an in-process caller's deadline.
74. **A join that fails counts as not loaded, and a primary that cannot be
    brought up ends the walk only when nothing else answers.** A start the
    join waited on that did not come up is logged, and the walk moves on,
    because "loaded" means able to take the request (entry 62 keeps it from
    starting that primary again).
    - Owner: a loaded alternate answers; otherwise the failed start's error
      is the answer, named for the primary, as for a direct request that had
      joined it. §4.2 step 3's own errors (`vram_too_large`,
      `vram_queue_timeout`, a start that fails) come back as before.
    - Guest (review R, finding 4; decided for the owner's literal order, "load
      primary if possible, if any candidate is loaded use that, otherwise
      use fallback"): a primary whose start fails, whose joined start fails,
      or that no card could hold (`vram_too_large`, warned about as the
      configuration problem it is) was not possible to load, so the walk
      goes on to the loaded alternates, then the alias fallback (reason
      `background`). With no fallback that can answer, the primary's own
      error is the answer rather than a deferral: it says more. A guest's
      climb that fails is answered the same way (entry 92).
75. **Loops keep their first candidate; a re-picked turn borrows another.**
    `/v1/responses`, agent chat, agent runs and MCP sampling hold one
    admission for the run. A turn that is re-picked runs on a hold of its
    own, and the next turn starts from the run's hold again. A unary
    `/v1/responses` stamps the candidate the run opened on, and each turn's
    log row names the model that served it. Legacy completions keep the
    client's body for a re-served send on a candidate, since the fit rewrites
    its max-output fields.
76. **A pinned caller is refused a candidate alias** (`unsupported`): it
    needs one model's tokenizer or vectors. `/v1/count_tokens` is not pinned
    and walks like any request, so a background alias's count may start its
    primary (H5). It is not re-picked at its send: a candidate that went away
    under the count answers `503 candidate_lost`, a metadata call the client
    repeats (review R, finding 5; the one place `candidate_lost` reaches a
    client).

Phase 4, S (`feat/candidate-aliases`, 2026-09-27): status surfaces and docs —
`deferrals_24h` on `CandidateAliasView`, `lmgw__status`'s `candidate_aliases`
section, `lmgw__models kind=alias`, the README. Taken during implementation (not yet reviewed), under
"a desktop app for one user".

77. **The deferral count joins by alias name, case-insensitively, and a live
    builder never returns "unknown".** `request_logs.requested_alias` is
    logged as the client spelled it; `CandidateAlias::alias` is matched
    case-insensitively everywhere else a name is looked up (`Snapshot::
    candidate_alias`, §12 entry 55), so `candidates::deferrals::for_alias`
    does the same rather than silently under-counting a request spelled in
    another case. The wire type is `Option<u64>` only so a cached frame from
    before this field existed still decodes (`#[serde(default)]`); every live
    builder (`ops::status`, `ops::models`, `GET /api/models/full`) always
    fills `Some(n)`, `Some(0)` included — `None` never means "unknown" here,
    unlike `peak_extra_bytes`'s own use of the same shape. One grouped query
    (`candidates::deferrals::deferrals_24h`) answers every alias at once, run
    at most once per caller and skipped outright when no candidate alias is
    configured, so an install without any pays no extra query and its
    `lmgw__status` output is byte-identical to before this phase.

Phase 4, U (`feat/candidate-aliases`, 2026-09-28): the dashboard — the
candidate-alias editor (`pages/candidate_alias_editor.rs`), the "Candidate
aliases" group on Models, and owner/draining chips on the runtime displays.
Taken during implementation (not yet reviewed), under "a desktop app for one user".

78. **`/api/op/candidate_alias_set` was missing** (only `/mcp` had it) and
    was added during the UI work; a fix, not a decision.

79. **A `preview` action, not a second endpoint.** `ops::candidate_alias::
    preview` (`crates/lmgw-core/src/ops/candidate_alias.rs`) reuses
    `candidates::derive::derive` and `candidates::validate::resolve_enabled`
    over a probe built from the patch — never `store::insert`/`update` — and
    never returns `Err` for a refusal a real save would raise: a fresh
    "New candidate alias" draft with zero candidates, or a facet a
    just-added candidate does not support, is the editor's normal mid-edit
    state, not a fetch failure. Every check that would `?` out in `create`/
    `update` instead does `error.get_or_insert(..)` (first message wins) and
    keeps deriving with the most useful stand-in: a `resolve_enabled` refusal
    falls back to `common.minus(requested_disabled)`, a bad fallback name
    skips the `fallback_supports` check rather than aborting the rest. The
    one field with no save-time equivalent, `addable` (every enabled local
    chat model not already a candidate, and which of the draft's enabled
    facets it lacks), reuses the same cached `exposed::local_row_entry` reads
    `derive` already pays for its own candidates — one request per keystroke
    pause answers both "what's wrong" and "what can I add", never one request
    per row a client-only picker would otherwise cost. Also accepted over MCP
    (`lmgw__candidate_alias_set`'s `action` enum) since nothing about it
    writes; tested in `tests/it/candidate_aliases.rs` (a fresh draft, an empty
    one, a facet-dropping add, and a bad fallback — each asserting the
    request still succeeds with `ok:false` and a coherent body, never an
    error response).
80. **The editor's `capabilities_disabled` is `Option<HashSet<String>>`,
    mirroring the op's own `None`-only-on-a-fresh-draft split (§12 entry 49)
    instead of always sending a concrete set.** `None` only for a brand-new
    draft that has never had its checkboxes touched — the save then omits
    the field entirely, so a fresh alias gets the same lenient "enable
    everything common" default a bare MCP `create` gets, with no need to
    enumerate today's uncommon facets first. The first manual toggle
    materialises `Some` as an **empty** set, then applies the one flip —
    never today's non-common facets (corrected after review: a greyed,
    unsupported facet must never be sent as disabled unless the owner
    actually switched it off, entry 49's fix) — so from then on every save
    states exactly the owner's own switches, nothing auto-filled.
81. **Reordering is two controls over one list, not two lists.** `candidates`
    stays a single `Vec<String>`; the primary is `[0]`, rendered as its own
    fixed row (a "Remove" that is refused, with a reason, while it is the
    only entry — the list may never save empty), and the alternates are
    `[1..]`. Only the alternates reorder among themselves: up/down buttons
    always work, and native HTML5 drag-and-drop (`dragstart`/`dragover`/
    `drop`/`dragend`, the dragged index kept in a plain signal — no
    `DataTransfer` payload, since source and drop target are the same page)
    layers on top. "Make primary" swaps an alternate into `[0]` directly
    rather than requiring N drags past the primary's own fixed slot.
    Nothing else in this codebase reorders a list (grepped), so there was no
    existing pattern to match either way. Verified past the design stage:
    a synthetic `dispatchEvent(new DragEvent(...))` sequence (no real
    `DataTransfer` payload, matching what the handlers actually read)
    reordered the alternates correctly both in headless Chrome
    (`scripts/ui-drive.py`) and in a real WebKitGTK `WebView` (a one-off
    GTK4 + WebKit 6.0 Broadway-backend script, the same technique
    `scripts/webkit-check.py` uses, thrown away after) — confirming leptos's
    typed `on:dragstart`/`on:dragover`/`on:drop` event wiring behaves the
    same in both engines. What neither proves is a real OS-level pointer
    drag gesture (cursor drag image, drop-effect chrome), which no
    tool here can simulate headlessly; the up/down buttons are the
    guaranteed-working path regardless, so both stayed rather than cutting
    the buttons once DnD "worked".
82. **The fallback picker's "must not be another candidate alias" grey-out
    needs no new catalog field.** `CatalogEntry` (the shared model picker's
    row shape) has no marker distinguishing a candidate alias from a plain
    one — both currently publish through `owned_by: "lmgw"` — so instead of
    widening that shared type for one caller, the editor's own `disallow`
    closure cross-references the candidate-alias names already sitting in
    `ModelsFull.candidate_aliases` (loaded once for the whole Models page)
    case-insensitively, alongside the picker's existing `e.local` check. The
    real refusal is still server-side (`Snapshot::usable_fallback`, §12 entry
    56) regardless of what the picker greys out.
83. **"Add candidate" reads the preview's `addable`, never the shared model
    catalog.** The catalog (`crate::catalog`, built from `/v1/models`) only
    ever lists `public` local rows, but a candidate need not be public at all
    (§12 entry 58) — the catalog-backed `ModelPicker` would silently hide a
    private one from the very list meant to offer every eligible row. Reusing
    `ModelsFull.local` (already loaded for the page) plus the preview's
    `addable` — filtered to `missing.is_empty()`, per §6's "listing only
    enabled local chat models that support every enabled facet" — covers
    every candidate the alias could actually take, public or not.
84. **Candidate aliases share the Models table's row shape** (`Row`,
    `rows_of`) instead of a second table, so grouping, filtering and the row
    menu stay one mechanism.

85. **The API-key scope editor was left alone:** its scope is free-text
    globs, which already reach a candidate alias by name.

Phase 4, F1 (`feat/candidate-aliases`, 2026-09-28): fixes after review R.
Taken during implementation (not yet reviewed), under "a desktop app for one user"; findings 1, 4, 6
rewrote entries 62, 70 and 74 in place.

86. **A request carries its alias to the end** (review R, finding 8).
    `CandidateCtx` gained `row`, the alias as the walk that picked the
    request read it. Every lookup made once the request holds a model — a
    climb's outside-VRAM fallback, the GPU hold during a climb, the
    outside-VRAM swap of an owner's primary, a re-pick's walk — reads the
    alias as it is now while it is still an enabled candidate alias (an edit
    made meanwhile applies), and the carried row once it is disabled or
    deleted. Before, those lookups went by name and fell through to the
    candidate's own row fallback, which the alias never uses (§4.1) — for
    an alias set to `none`, possibly a cloud fallback. The fallback is still
    re-validated at use (`usable_fallback`) and held to the alias's enabled
    facets, now passed in (`fallback_serves`'s `facets`) rather than looked
    up by name. A re-pick of a deleted alias derives its pick once, outside
    the pick cache: the cache is keyed by name within a snapshot, and a new
    alias of the same name must not be served the old one's pick.
87. **The pick cache follows the files a pick is derived from** (review R,
    finding 3; amends entry 57). A pick is derived from the snapshot and from
    files — each enabled candidate's weights header, its configured
    projector's header, its chat template file — and those change without a
    snapshot swap (an HF update in place, a file still being written, a
    mount not up yet). Each cached pick now carries the size and mtime of
    those files (`candidates::stamps`); every call stats them — one to three
    `stat`s per candidate, together on the blocking pool, nothing read — and
    derives again when they differ, the revalidation `GgufSummaryCache`
    does for the headers. A pick derived while any of them could not be read
    is never cached, matching that cache's "errors are never cached": before,
    one unreadable GGUF at the first request after a reload kept the
    candidate out of `routable` until the next config write, which for a
    background alias meant sending its traffic to a cloud fallback for
    hours. Not stamped: a ladder's higher rungs (they feed only the
    published context, which a pick does not carry), and a sibling `mmproj`
    that is present but not configured (it enables nothing). The alias
    fallback's capabilities are not in the pick the gate reads at all:
    `fallback_serves` checks them at use, from the live catalog cache
    (`CandidatePick::fallback_usable` is informational).
88. **A re-pick gives the old candidate's pool reservation back first**
    (review R, finding 9). `gate::send`'s re-pick used to keep the lease's
    pool ticket until the site dropped the lease, i.e. for the whole walk
    that followed — which can start the primary for a guest or wait in
    admission for an owner — so requests on the old candidate's guarded
    pool queued FIFO behind a request that had already left.
    `TurnLease::reroute` now drops it before the walk: at once when the
    cause is a context refusal (llama-server refused before any work, or the
    fit refused before the send), through the deferred release otherwise (a
    candidate lost mid-send may still have a slot to let go of).
89. **The owner's claim and a guest's decision are ordered by one lock**
    (review R, finding 10; makes entries 63 and 66 exact). Two windows let a
    guest act on the owner's model after they claimed it:
    - a guest's join read the draining mark before taking the registry's map
      lock, and the mark was armed without it; now the mark is armed under
      the map lock (`Registry::drain_for_owner`) and read inside the join's
      lock hold, so a background claim either landed before the owner's wait
      began (it is in flight, and the wait sees it) or refuses;
    - a guest's climb checked `owner_of` and then claimed the new rung's
      start in a second lock hold; now `ClimbTicket::start_for_guest` asks,
      in the lock hold that claims the start, that the entry is still
      `Background`-owned and no owner admission waits for room. Refused, the
      ticket drops, the mark clears and the running rung serves on
      (`Climbed::Denied`, the gate re-picks). The owner's own climb start is
      unchanged (`ClimbTicket::start`).
90. **A guest never stops the owner's model** (review R, finding 11). The
    dead-container recovery force-stops the generation that stopped
    answering, whoever's hold saw it; for a guest's hold on the owner's
    model that was background traffic killing the owner's in-flight requests
    over what may be one failed connection. A guest's hold now stops a dead
    container only while it is still `Background`-owned, decided in the lock
    hold that marks it stopping (`Registry::stop_dead_background`); on the
    owner's model it stops nothing and ends in `candidate_lost`, so the gate
    picks again — if the model really is down, the owner's own next request
    recovers it as it always has. No `/health` probe was added: the owner
    case stops nothing at all, and a guest's own model keeps today's forced
    stop.

**Live check of phase 4 (2026-09-28, dev instance, RTX 4090, Qwen3.5-0.8B
`q08` and Qwen3.5-2B `q2`, ctx 4096, `parallel` 1; aliases `bg` = [q2, q08]
background, `own` = [q2, q08], both fallback `none`):**
- A background request to `bg` while `q2` ran as the owner's model was
  answered by `q2` (`x-lmgw-candidate: q2`), and `q2` stayed the owner's.
- With `vram.budget_mb` 4300, `q2` could not start next to the owner's `q08`:
  `bg` and `own` were both answered by `q08`, nothing was evicted, and `q2`
  never started.
- With `q2` started as a guest and streaming 1500 tokens, a direct owner
  request for `q08` queued ("waiting for a busy model to finish") and `q2`
  showed `owner: background, draining_for_owner: true`. A `bg` request in
  that window got `503` code `gpu_hold` ("'bg' deferred: GPU in use by the
  owner — 'q08' is waiting for room"), logged `gpu_deferred`. The stream
  finished normally; `q2` was then evicted, and `q08` answered the owner
  after 8.1 s.

Final pass (`fix/final-pass`, 2026-09-28): fixes after the final cross-phase
review. Taken during implementation (not yet reviewed), under "a desktop app for one user"; X1
revised entries 46 and 64 in place, and X5 added cross-references to §4.3
and entries 66, 73 and 74.

91. **Background waits for background, never for the owner** (final review
    X1). A background job fires its requests in parallel — the case the
    feature exists for — and two paths sent all but one of them to the
    fallback while their own primary was being loaded or climbed for them:
    - a guest's start try-locked the admission gate, and the gate was held by
      the first request's start of the same primary while it decided
      (measuring, and evicting an idle guest takes seconds);
    - every queue row counted as the owner waiting, and a guest's climb
      enqueues one, so the next trigger of that same climb was denied, and
      every guest start was blocked while any guest climb drained.

    Now each queue row carries its origin (a start's is the owner's, a
    climb's is its hold's), and only the owner's make a guest give way. A
    guest's start of a model first takes that model's **guest turn**, a
    per-model lock only guest starts take: it waits for another guest's
    start of the same model to decide, then tries the gate as before, and
    the model is by then up or reserved, so the next look joins that one
    start. The turn is held only across the decision, never while waiting
    for the gate or for a claim, and the wait for it is bounded by
    `vram.queue_timeout_seconds` from the start of the call (0: until the
    guest before decides, which waits for nothing). A guest's climb trigger
    rides another guest's climb of the same model (one reload serves both),
    and `Registry::mark_climb_for_guest` refuses, in the lock hold that
    marks, a model that is the owner's or while the owner waits for room: a
    `Background`-owned model has never had an owner claim in this
    container's life, so every climb on it is background traffic's own, and
    a guest never marks, joins or raises the owner's climb.

    Unchanged: an owner row in the queue, or the draining mark, still blocks
    every guest start and denies every guest climb (the gate re-picks). An
    owner who has to wait for room while a guest start is under way gets
    entry 47: the start finishes and serves the request that made it, and
    the requests waiting on it are new background work and are refused. A guest start of
    a *different* model is still not waited for: it holds the gate only for a
    decision, and a guest does not queue behind any admission.

92. **A guest's climb that fails goes on like a primary that cannot be
    brought up** (final review X5). Only `Climbed::Denied` used to re-pick; a
    guest's climb that failed any other way ended the request with that
    error, although another loaded candidate or the alias fallback could
    have answered — the opposite of what entry 74 decided for the guest's
    primary start. Now, for a guest's hold with a candidate context
    (`candidate::repicks_climb`), these exclude the model and run the walk
    again, exactly like a denial:
    - a rung no card could hold (`vram_too_large`, checked before
      `may_climb`, entry 66) — warned about as the configuration problem it
      is;
    - a running rung that never went quiet within the queue timeout
      (`LadderDrainTimeout`);
    - the climb's 502s: a rung that would not start, a joined climb whose
      rung failed, a stop that won against the climb;
    - llama-server's second context refusal on a ladder, after the climb its
      first one made (the count undercounted twice): held back for guests
      as `ContextExceeded`, as the non-ladder backstop is (entry 73), rather
      than handed over as the `400`.

    With no other candidate loaded and no fallback that can take the
    request, the climb's own error is the answer rather than a deferral, as
    in entry 74: a configuration problem or a failed load is not traffic
    declined on purpose, and must not count as a deferral. The second
    backstop refusal is a context refusal like entry 73's and defers as
    those do. Owner requests get every one of these errors exactly as
    before.

93. **Owner climbs that need each other's memory: one gives way at once**
    (final review X7). Two ladders in use, each needing a rung that does not
    fit next to the other's running rung: each climb's admission waited for
    the other's model, which stays busy until its own climb ends (its
    trigger's claim holds it, and eviction never takes a model being
    climbed). Ladder spec entry 53's yield hands the gate to a climb queued
    for it, so the two handed it back and forth every `POLL` until the queue
    timeout ran out — never, at 0 — with `owner_drain` armed throughout, so
    all background traffic was deferred meanwhile. Entry 53 breaks the
    decide-versus-climb cycle; this is climb against climb.

    Now a climb's admission holding the gate, with nothing it may evict,
    asks `climbs_deadlocked`: counting itself and every other climb queued
    for the gate, can any of them fit in what the card holds less the
    others' running rungs? If none can, none can ever finish first, and the
    one holding the gate gives way at once: `vram_queue_timeout` naming the
    other climbs, the rung sizes and the card. Its trigger's request ends
    with that 503, its claim goes, and the other climb evicts its model and
    climbs. The card's total is used, not the free figure: VRAM outside lmgw
    and lmgw's other models only take room away, and the running rungs'
    estimates are lower bounds, so it never fails a climb that could have
    finished after the other — when one of them can fit, nobody gives way
    and the yield orders them as before. Which one gives way is whichever
    holds the gate when both are queued; there is no better claim to it.
    A climb whose queue row cannot be found is not judged.

    Rejected: yielding only to a climb this one does not wait on (it keeps
    the gate and waits for a model that cannot become idle, so it ends only
    by the budget, and never at 0); abandoning the climb and retrying (the
    trigger's claim stays, so the other model never becomes idle either).
    Bounded otherwise: a climb waiting for a model that is busy for another
    reason (a long generation, an in-process run holding its claim) waits
    as before, up to the queue timeout, forever at 0 — the owner's own "no
    limit", not a cycle. Guest climbs never wait at the gate, so they are
    never part of this.
