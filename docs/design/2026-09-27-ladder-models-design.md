# Ladder models — design (2026-09-27)

**Status:** Implemented on `feat/ladder-models` (phase 3, 2026-09-27), WP1–WP6, three review passes; merged. D4 (a first production ladder) and the 27B measurements are not part of this record. Decisions taken during implementation (not yet reviewed): §12.

Builds on [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
(a ladder changes what one model's container is started with) and
[2026-09-04-gpu-hold-design.md](2026-09-04-gpu-hold-design.md) (whose
`x-lmgw-fallback` header and pre-first-byte swap this borrows).

## 1. Summary

A local chat model can be switched to **ladder mode**. Its weights selector
becomes a table of **rungs**: each rung is a GGUF file and a context size.
Everything else on the row is shared by every rung: template, reasoning,
sampling, projector, drafter, cache types, `parallel`, image and run args.

- **One rung runs at a time**, in the model's one container.
- **A request that does not fit the running rung makes lmgw climb.** "Fit"
  means prompt tokens plus max output fit the per-slot context. lmgw goes
  straight to the smallest rung that fits, after the running rung has drained.
- **The whole model climbs, not the request.** Every other conversation on
  that model moves up with it, even ones that would have fit lower.
- **It only comes back down when the container stops.** That can be the idle
  reaper, an eviction, the hold, an apply or a restart. The next start is the
  base rung.
- **Max output is mandatory on a ladder model, and lmgw enforces it on every
  request** (§3.2). Without it no rung can promise that an answer fits.
- **`/v1/models` advertises the top rung's context.** The ladder delivers it
  by climbing.

The same table builds two shapes:

| Shape | Rungs | Buys | A climb costs |
|---|---|---|---|
| **Grow** | same GGUF, rising context | VRAM stays free while conversations are short | a container restart (weights reload) plus reprocessing each conversation's history; §10 turns it into an in-place resize |
| **Slider** | better quant + short context → smaller quant + long context, about the same footprint | the better quant while conversations are short | a weights reload plus reprocessing each conversation's history |

**Why.** From `request_logs`, 2026-08-28 to 2026-09-25:

- **`gemma4-12b`:**
  - 13,553 of 14,169 requests (96%) had prompts of 4k tokens or less; the
    largest was 59.5k.
  - The row reserves `-c 512000` (2 slots × 256k), about 4.5 GB of q8_0 KV.
  - A 4k conversation uses about 36 MB of it.
- **`qwen3.8-27b-reason-maxctx`** (IQ3_XXS, 130000):
  - It took all 3,382 qwen3.8 requests.
  - 1,910 of them (57%) had prompt + completion of 30000 or less, so they would
    have fit `qwen3.8-27b` (Q4_K_XL, 30000). That row has no traffic.
  - The largest prompt was 129,355.
  - `Qwen3.8-27B-UD-IQ4_XS.gguf` is on disk and unused.
  - That pair of rows is a hand-built two-rung slider.

**Not in this design:**

- routing *different* models per request (the "rule alias" idea, deferred);
- growing the KV cache inside a running llama-server (§10).

## 2. Facts this rests on (checked 2026-09-27)

### 2.1 llama.cpp

Upstream master `171e884` (2026-09-26), the base of the `official-master` image
lmgw runs.

1. **The server's `-n` is a default, not a cap.**
   - `slot.n_predict_max = task.params.n_predict != -1 ? task.params.n_predict : params_base.n_predict`,
     commented "the per-request limit takes priority over the global one"
     (`tools/server/server-context.cpp:1817`).
   - A client's `max_tokens` above the row's `n_predict` wins.
   - lmgw has to clamp (§3.2); setting `-n` guarantees nothing.
2. **Running out of context mid-answer truncates it.**
   - With context shift off (the default), a full slot stops generation with
     `truncated = true` and `STOP_TYPE_LIMIT` (`server-context.cpp:1885`).
   - The client sees `finish_reason: "length"`.
   - This is the failure a ladder has to prevent.
3. **An over-long prompt is refused before any work is done.**
   - The error is `exceed_context_size_error`, and its body carries the exact
     `n_prompt_tokens` and `n_ctx` (`server-context.cpp:3196-3214`,
     `server-task.cpp:1503`).
   - Nothing has been processed and nothing streamed.
4. **The limit is per slot.**
   - `slot.n_ctx = n_ctx_slot()`, and `GET /props` reports that per-slot value
     as `n_ctx` (`server-context.cpp:4606`).
   - With an explicit `-np N`, the KV cache is not unified (`--help`: unified
     is "enabled if number of slots is auto"), so each slot gets `-c / N`.
5. **Exact counting needs the running server.**
   - `POST /apply-template` renders a chat body through the same parser the
     completion path uses (`oaicompat_chat_params_parse`, `server-context.cpp:5060`).
   - `POST /tokenize` counts the result; `parse_special` defaults to true
     (`:5084`).
   - Neither counts media. An image's token cost depends on the projector
     (`--image-max-tokens`, `common/arg.cpp:2642`).
6. **Reasoning tokens are output tokens.** They are generated text and count
   against the limit. On a reasoning model, max output has to cover thinking
   plus answer.
7. **The prompt cache dies with the process.** `--cache-ram` lives inside the
   server, so a new container loses it, along with every conversation's KV.

### 2.2 lmgw (code)

8. **A rung switch reuses the model's container.**
   - The chat container is built by `chat_runtime(snap, m)`
     (`runtime/descriptor.rs:257`).
   - Its name is `{prefix}-chat-{slug}-{hash6(model_id)}` (`runtime/mod.rs:159`),
     so the name does not change between rungs.
9. **The climb move already exists as "apply".**
   - `apply` on a running model means stop, then a fresh cold start on the next
     request (`ops.rs:5357-5388`).
   - `stop` refuses with `Busy` while requests are in flight
     (`runtime/registry.rs:1326`).
   - A climb is the same move with the rung chosen.
10. **Admission is keyed by `(class, model_id)`, not by the running rung.**
    - The key is built in `vram/mod.rs:128-156`.
    - The ledger recomputes each resident's footprint from the *current* row on
      every build (`vram/mod.rs:543`). For a ladder it has to use the rung the
      entry is running.
    - `PlanCache` keys on gguf/ctx/cache types (`vram/plan.rs:263-268`), so
      footprints per rung come at no extra cost.
11. **`send_local` already retries before the first byte.**
    - It retries on `Transport` errors only, and only before the response is
      handed to the caller (`vram/mod.rs:936-995`).
    - The context backstop (§3.3) retries at that same point.
12. **Nothing caps `max_tokens` today.**
    - `Params.max_tokens` (`ir.rs:344`) is only *filled* by
      `with_defaults` (`ir.rs:366`).
    - The Anthropic route's defaulting and raising are the precedent for
      "lmgw changed your max_tokens, and says so": `x-lmgw-max-tokens-defaulted`
      and `x-lmgw-max-tokens-raised` (`proxy.rs:318-322`, annotations at
      `proxy.rs:397-398`).
13. **`/v1/models` already publishes per-slot context and max output.**
    - Local context is `ctx_size / parallel` (`capabilities/exposed.rs:302-307`).
    - `max_output_tokens` is `n_predict` (`capabilities/mod.rs:377`).
14. **`exceed_context_size_error` has no dedicated handling.** It currently
    lands as a generic `GatewayError::Upstream` (`egress/openai.rs:463-478`).
15. **lmgw counts with `/tokenize` only and never calls `/apply-template`**
    (`egress/openai.rs:497-520`).
16. **Row params and headers.**
    - `LlamaParams` is one JSON column (`config.rs:770-869`).
    - The latest migration is `0040`.
    - Headers are stamped at every terminal response via the `with_fallback`
      pattern (`proxy.rs:603`).

### 2.3 Measured on this machine

17. **Cold start of `gemma4-12b`**, as time to first token minus prefill:
    4.6, 4.7, 9.4, 11.0 and 11.6 s over five cold starts. This includes any
    eviction the start needed, so it is an upper bound on the load. The 27B
    rungs are unmeasured (WP0).
18. **Prefill:** 3.9–4.7k tok/s on the gemma4-12b rows. No timings exist for
    qwen3.8; its traffic predates the timings columns.
19. **KV per token at q8_0**, from the GGUF headers:
    - qwen3.8-27B: about 33 KB (hybrid, full attention on every 4th layer);
    - gemma-4-12B: about 8.8 KB (the sliding-window layers are fixed-size).

## 3. Semantics

### 3.1 Outcomes

| Situation (ladder model) | Outcome |
|---|---|
| Nothing running | Start the **base rung**, then continue as below. The first request of a long conversation after a reset pays two loads (open decision D2). |
| Prompt + max output fits the running rung | Forward it with `max_tokens` clamped (§3.2). |
| It does not fit, but a higher rung fits | **Climb** (§3.4) straight to the smallest rung that fits, then forward. |
| It does not fit the top rung | `400`. OpenAI code `context_length_exceeded`, Anthropic `invalid_request_error`. The message states prompt tokens, max output and the top rung's per-slot context. No climb, no truncation. |
| A request arrives while a climb is in progress | It waits, the same way acquirers wait on `Starting` today. It is then re-checked against the new rung and served there. |
| A request is still in flight on the old rung when a climb is triggered | It finishes normally on the old rung; the climb waits for it (drain). |
| The pre-check undercounted and llama-server refuses (fact 3) | Climb to the smallest rung that fits `n_prompt_tokens` from the error body plus max output, and retry once, before the first byte. |
| Admission cannot fit the target rung | The triggering request fails with the existing VRAM error naming the rung and the numbers. **The old rung is not stopped** and keeps serving. |
| The container stops (reaper, eviction, hold, apply, restart, crash) | The ladder resets; the next start is the base rung. |
| GPU hold active | Unchanged: fallback or `503 gpu_hold`, like any local model. No climb. |
| Row without a ladder | Unchanged: no clamp, no pre-check, no header. |

### 3.2 Max output

- A ladder row **requires** `n_predict > 0`; saving a ladder without it is
  refused (§4.3).
- Every request is sent with
  `max_tokens = min(client max_tokens or n_predict, n_predict)`.
- **When lmgw lowered a client value**, the response carries
  `x-lmgw-max-tokens-clamped: <n>`, alongside the existing defaulted/raised
  headers. The request log records it too.
- **A client value below the cap is kept, and it is what the fit check uses.**
  A client that asks for less stays on a lower rung longer.
- `/v1/models` already publishes `n_predict` as `max_output_tokens` (fact 13).
  The cap is visible before anyone hits it.

### 3.3 The fit check

It runs on the running rung, after acquire, before forwarding:

1. `POST /apply-template` with the exact chat body egress is about to send
   (messages, tools, `chat_template_kwargs`, reasoning fields), giving the
   prompt text.
2. `POST /tokenize` on that prompt, giving the text token count.
3. Plus an upper bound per media part. Whether that bound can be read from the
   projector's header or has to come from `--image-max-tokens` on the row is
   checked in WP0. A ladder row with a projector but no known bound is refused
   at save time, for the same reason max output is mandatory.
4. `fits(rung) ⇔ tokens + max_tokens ≤ per_slot_ctx(rung)`.
   `per_slot_ctx = ctx / parallel`, the same number `/v1/models` publishes.

If llama-server still refuses (a counting error, a template quirk), fact 3's
error body is the backstop (§3.1). `POST /v1/count_tokens` on a ladder model
counts on the running rung, or starts the base rung, as today.

### 3.4 Climbing

1. Mark the entry **climbing to rung k**. New acquires for the model wait.
2. Wait until `in_flight == 0`. The wait is bounded by the existing
   admission-queue timeout, and a timeout fails the triggering request visibly.
3. **Admission as a replacement.** Fit rung k's footprint with the current
   rung's footprint counted as freed. The eviction candidates are the usual
   LRU idle ones, never the ladder model itself. If the answer is no, clear
   the mark and fail the trigger; the old rung keeps serving.
4. `stop` the old rung and start rung k: same container name, argv rendered
   for rung k.
5. Release the waiters. Each one is re-checked (§3.3) and forwarded.

Every conversation on the model then reprocesses its history on its next turn,
and the RAM prompt cache is gone (fact 7). That is the price of "the whole
model climbs", and it is accepted.

### 3.5 Coming down

Only through a container stop. The idle reaper already re-reads `idle_seconds`
from the current row on every pass (`runtime/lifecycle.rs:396-448`), so the
ladder's "silence timeout" is simply the row's `idle_seconds`.

The container actions do the rest:
- `restart` is **"back to base"**: stop, and the next request starts the base
  rung.
- A background client that calls the model more often than the idle timeout
  keeps it on the top rung. The dashboard shows the rung (§6) so this is
  visible, and `restart` is the manual way down.

## 4. Config model

### 4.1 Row

- **Base rung.** The row's own `gguf_path` and `params.ctx_size`, unchanged.
  Everything that reads those fields (inspect, plan, test, argv) keeps working
  for rung 1.
- **Higher rungs.** A new column `local_models.ladder TEXT NOT NULL DEFAULT '[]'`
  holds a JSON array of `{gguf_path, ctx_size}`.
  - There is one source of truth per rung.
  - An empty array means "not a ladder".
  - `LocalModel.ladder: Vec<Rung>`.
- `ctx_size` keeps its existing meaning on every rung: it is `-c`. The per-slot
  context and the switchover are derived (§4.2).
- `chat_runtime(snap, m, rung)`: rung 0 renders exactly as today. Rung k > 0
  overrides `-m` and `--ctx-size` and nothing else.

### 4.2 Derived per rung (shown, never stored)

- per-slot context = `ctx_size / parallel`;
- **switchover** = per-slot context − `n_predict`, the largest prompt this rung
  takes with the full max output;
- footprint = `PlanCache` for (gguf, ctx, cache types);
- whether the GGUF has MTP layers (from the header).

### 4.3 Validation at save time

Refused, with the reason:

1. A non-empty ladder with `n_predict` unset or ≤ 0.
2. A non-empty ladder whose KV cache is effectively unified. That means the
   `kv_unified` toggle is on, or `parallel` is unset (auto slots unify it,
   fact 4). The toggle and its pool ledger are specified in
   [2026-09-27-candidate-aliases-unified-kv-design.md](2026-09-27-candidate-aliases-unified-kv-design.md)
   §3. A shared pool has no per-slot guarantee. That spec's §10 lifts this
   rule later.
3. Per-slot contexts that are not strictly increasing, base included, or a
   base `ctx_size` that is unset.
4. A rung whose per-slot context is ≤ `n_predict`, which gives no room for any
   prompt.
5. A rung GGUF that is missing, is not a weights file, or does not match the
   base's architecture and tokenizer header fields. The fit check counts with
   whichever rung is running, so every rung must tokenize identically.
6. `spec_type = draft-mtp` without a draft file, when a rung GGUF lacks MTP
   layers. That rung would not load.
7. A projector (`mmproj_path` set, `no_mmproj` false) with no known per-image
   token bound (§3.3).

Advisory only: a rung whose footprint exceeds the card's capacity minus
headroom. VRAM is measured at climb time, not at save time.

### 4.4 Capabilities

`/v1/models` for a ladder row:
- context = the **top** rung's per-slot context;
- `max_output_tokens` = `n_predict` (unchanged);
- notes say "ladder, N rungs; a request past a rung's switchover waits for a
  model reload".

Vision, tools and reasoning are row-level, so every rung shares them by
construction.

## 5. Runtime

- **The registry entry records the rung it was started with.** `acquire` takes
  the rung to start when the entry is absent (always the base, §3.1).
  `switch_rung(class, model_id, k)` implements §3.4 using the existing
  `Starting`/`Stopping` wait machinery.
- **The ledger charges an entry's running rung**, not the row's base (fact 10).
- **The replacement admission decision is new**: the target model is "up" but
  its footprint changes. Everything else (eviction order, `/slots` busy probe,
  hold check first) is reused.
- **Reaper, hold sweep, apply and eviction are unchanged.** Each ends in a stop,
  and a stop is the reset.

## 6. Surfaces

- **Response headers on ladder rows:**
  - `x-lmgw-rung: <k>/<n>; ctx=<per-slot>; gguf=<file name>` (the `gguf` part
    is omitted if the name is not ASCII, as `with_fallback` does);
  - `x-lmgw-max-tokens-clamped` (§3.2).
- **Request log:** a new column `request_logs.rung INTEGER NULL` (migration
  0043, together with `local_models.ladder` — §12 entry 1), and the clamp.
- **Dashboard runtime and `lmgw__status`:** each ladder entry shows
  `rung k/n` and a `climbing` state with its reason ("prompt 41,210 + 8,192 >
  30,000").
- **Editor:** a "Ladder" toggle.
  - When it is on, the "Weights (GGUF)" field becomes the first row of a rung
    table.
  - Each row has a GGUF picker (the existing `GgufField` browse modal) and a
    context field.
  - Derived columns: per slot, switchover, footprint, MTP ✓.
  - "Max output" becomes required and is shown next to the table.
- **MCP:**
  - `lmgw__local_model_set` gains `ladder` (array of `{gguf_path, ctx_size}`)
    and `clear: "ladder"`.
  - `lmgw__local_model_get` shows the ladder and every rung's command line.
  - **`lmgw__local_model_test` tests every rung in turn and reports each
    rung's load time.** That is also WP0's instrument.
- **README:** a "Ladder models" subsection.

## 7. Testing

Uses the `tests/it/vram_admission.rs` fixture (fake podman, FakeGpu, wiremock
containers). The llama-server mock adds `/apply-template`, `/tokenize` and the
`exceed_context_size_error` body.

1. **Fits the base.** No climb, `max_tokens` clamped to `n_predict`, header
   `x-lmgw-rung: 1/3`, log `rung = 1`.
2. **Client `max_tokens` below the cap.** Kept, and used for the fit (it stays
   on the lower rung where the cap would have climbed).
3. **Too big for rung 1, fits rung 3.** Climbs directly to 3 (one stop, one
   start), header `3/3`.
4. **A request in flight on rung 1 when a climb triggers.** It finishes on
   rung 1; the trigger and a queued short request are both served on the new
   rung.
5. **Too big for the top rung.** `400 context_length_exceeded` with the three
   numbers, no podman call.
6. **Admission refuses the target rung.** VRAM error; rung 1 is still running
   and serves the next request.
7. **Backstop.** The mock undercounts, llama-server refuses: one climb, one
   retry, and the client sees a 200.
8. **Reaper stops a rung-3 model.** The next request starts rung 1.
   `restart` does the same.
9. **Hold on.** A ladder model falls back or returns 503 like any local model.
   No climb, no start.
10. **Validation.** Each refusal in §4.3; `clear: "ladder"` round trip.
11. **`/v1/models`.** Top rung's per-slot context and `n_predict`.
12. **Ledger.** It charges the running rung's footprint, not the base's.
13. **A row without a ladder.** Byte-for-byte unchanged request body, no
    headers, no `/apply-template` call.

`bash ci/check.sh` is the gate.

## 8. Work packages (sequential)

0. **Measure (needs the GPU free).**
   - Load time per qwen3.8 rung (Q4_K_XL / IQ4_XS / IQ3_XXS).
   - 27B prefill tok/s.
   - The cost of `/apply-template` + `/tokenize` on a 100k-token conversation.
   - Real VRAM at each rung's intended context.
   - Where the per-image token bound comes from.
   - This decides the rung contexts for the first ladder and confirms that the
     fit check is cheap enough to run on every request.
1. **Model + plumbing:**
   - migration 0043 (`local_models.ladder`, `request_logs.rung` — §12 entry 1);
   - config, store and api-types;
   - `LocalModelPatch.ladder` plus `clear`;
   - §4.3 validation;
   - `chat_runtime(.., rung)`;
   - capabilities.
2. **Runtime:**
   - the rung on registry entries;
   - `switch_rung`;
   - the ledger charging the running rung;
   - replacement admission.
3. **Request path:**
   - the ladder gate (clamp, fit check, climb) called at every chat send site
     (the GPU-hold spec's site inventory is the starting list);
   - the backstop retry;
   - a `ContextExceeded` error variant;
   - headers and the log column.
4. **Tests** (§7).
5. **UI:**
   - the editor's ladder table;
   - the dashboard runtime rung and climbing state;
   - the request-log rung column.
6. **MCP + docs:**
   - `local_model_set`/`get`/`test`;
   - `status`;
   - README.
7. **Adversarial review** of the whole diff, then `ci/check.sh`, then merge to
   main.

**Order across specs.** The clamp, the fit check and the `ContextExceeded`
error are shared with the unified-KV pool ledger. They are built once, as WP2
of the candidate-aliases / unified-KV spec, before this spec's WP3.

## 9. Open decisions

- **D1 — client asks for more than max output:** clamp, or refuse? The spec
  clamps: clients send generous `max_tokens` out of habit, so a refusal would
  break them. The cap is advertised in `/v1/models` and stamped when applied.
- **D2 — cold start:** base rung, then climb, or estimate first? The spec
  always starts the base: it is exact and simple. It costs a long conversation
  returning after an idle reset two loads instead of one. A byte-length
  estimate could pick the starting rung instead, and would climb only on a
  miss.
- **D3 — the table edits the context, and the switchover is derived.** This
  keeps `ctx_size` meaning `-c` everywhere and makes footprints exact. The
  alternative is to edit the switchover and derive the context.
- **D4 — the first ladder.** Merge `qwen3.8-27b` (Q4_K_XL, 30000) and
  `qwen3.8-27b-reason-maxctx` (IQ3_XXS, 130000), with IQ4_XS as the middle
  rung. The two rows differ, and one row has to win on each of these:
  - **Reasoning:** the maxctx row has reasoning on, preserve on, effort low;
    the other has it unset.
  - **Projector:** the maxctx row runs with `no_mmproj`.
  - **Name:** which model id the merged row keeps.

## 10. Later

- **In-place resize (llama.cpp patch, built via the Backends page).**
  - Most of the pieces are already upstream: the sleep/wake path tears down
    and rebuilds the context (`handle_sleeping_state` → `load_model`), and
    per-sequence state save/load exists.
  - The patch rebuilds only the `llama_context` on the same weights, then
    restores the slots.
  - A **grow** transition (the same GGUF on both rungs) would then take about
    a second instead of a reload. Only **slider** transitions would still
    reload weights.
  - The transition kind is decidable from the table today (same file vs
    different file), so the patch slots in without a config change.
- **Carrying KV across a reload.** Slot save/restore
  (`--slot-save-path`, `POST /slots/{id}?action=save|restore`) could skip the
  history reprocessing. It is unverified for hybrid models and for the MTP
  draft context, which the save does not include.
- **Pre-climb between turns.** When a finished response pushed a conversation
  past the switchover, climb while nobody is waiting. Deferred: it guesses
  that the conversation continues.
- **Per-rung KV cache types**, e.g. q4 V on the top rung, as an extra column.
- **KV cache backed by CUDA virtual memory**, growing token by token with no
  rungs. ggml-cuda already uses the VMM API for its pool (`ggml-cuda.cu:587`),
  and attention only reads up to the highest used cell
  (`llama-kv-cache.cpp:1250`). It is a large ggml change, and upstream's
  appetite for it is unknown.
- **Rule aliases** (routing between different models).

## 11. Out of scope

- Ladders for aux, audio and image models.
- Cloud rungs.
- Per-conversation rung assignment.
- Stepping down while the model is busy.
- Mixing models with different tokenizers in one ladder.

## 12. Implementation decisions (for review)

Phase 1 (`feat/gate-core`, 2026-09-27) built the shared core this spec's WP3
reuses: the clamp, the count and `ContextExceeded`. Its decisions are listed
in the unified-KV spec §12. These concern this spec:

1. **Migration numbers.** `0041` is `request_logs.max_tokens_clamped` (the
   clamp's log record, §3.2). Phase 2 (`feat/external-vram-fallback`) took
   `0042` for `request_logs.fallback_reason` (unified-KV spec §12.32) while
   this spec was still a draft, so this spec's migration
   (`local_models.ladder`, `request_logs.rung`) becomes **`0043`**.
2. **Where the fit check lives.** `gate::fit_chat` / `fit_text` run per send,
   after admission. The climb hook is `climb_if_needed` in `gate/fit.rs`. It
   gets the `LocalHold` (so it can swap the claim in place, like
   `LocalHold::recover`), the count and the params. `guarded()` in the same
   file must start returning ladder rows. Headers go through `GateHeaders`
   in `gate/open.rs`.
3. **The clamp binds every way of asking for output**, not only
   `max_tokens`: a passthrough `n_predict` (which llama-server prefers) and,
   on legacy completions, `max_completion_tokens`. The header is stamped only
   when a client value was lowered; a missing value is filled silently, as
   llama-server's `-n` would.
4. **The per-image bound (§3.3 step 3, WP0's question):**
   `gate::count::image_token_bound`. A fixed-size projector's exact count
   comes from its header, Gemma 4's measured ceiling (raised by the row's
   `--image-max-tokens`), and otherwise only the row's `--image-max-tokens`.
   §4.3 rule 7's save-time refusal should call the same function.
5. **Counting cost** (WP0, qwen3.5-0.8b tokenizer): 21 ms at 1.6k tokens,
   456 ms at 37k, 1.7 s at 123k. On 100k-token conversations the fit check
   costs over a second per request, most of it `/apply-template`. Worth a
   look before ladders go live (for example, skip the count while a
   byte-length upper bound already fits the running rung).
6. **The backstop's input:** `egress::openai::parse_exceed_context` reads
   `n_prompt_tokens` / `n_ctx` from llama-server's
   `exceed_context_size_error`. WP0 saw it as a plain JSON 400 before any
   streaming, for streaming and non-streaming requests alike, so the retry
   point in §3.1 always has it before the first byte.

Phase 3 (`feat/ladder-models`, 2026-09-27): WP1–WP6. Taken during implementation,
under "a desktop app for one user"; each is local and reversible.

7. **Counting cost: the count runs beside the send, not before it.** Phase 1's
   split (entry 5) has `/apply-template` at about three quarters of the cost.
   A proven cheap bound (templated bytes: every text token covers at least
   one byte) can only replace `/tokenize`, the smaller quarter, and only for
   prompts under about a quarter of the rung, where counting is already
   cheap; nothing short of rendering bounds the template's output, so the
   template call stays. Instead, on a ladder row the exact count is started
   together with the forward to the running rung, and the response is held
   until the count's verdict is in: fits, it is handed over; does not fit,
   the send is dropped (no byte has reached the client), the model climbs
   and the request is sent again. llama-server renders and tokenizes the
   same prompt itself before its first token, so the count finishes about
   when prefill starts and costs almost no latency. Still exact, so never an
   under-count. Its one cost: a prompt that fits the running rung but whose
   prompt + max output does not gets a partial prefill on the old rung
   before the send is dropped. Accepted: it cannot happen on a short chat,
   and the climb reprocesses the history anyway. The pool ledger (unified
   rows) keeps counting before it reserves, because it has to reserve
   before it sends; ladder rows never have a pool (§4.3 rule 2).
8. **A climb blocked by VRAM outside lmgw's control.** The replacement
   admission takes phase 2's verdict. External with a usable fallback: the
   triggering request is answered by the fallback with
   `x-lmgw-fallback-reason: external_vram`, and the old rung keeps serving.
   External without one: §3.1's VRAM error naming the rung and the numbers;
   the old rung keeps serving. lmgw's own idle models in the way are evicted
   LRU-first (never the ladder model itself); busy ones are waited for within
   `vram.queue_timeout_seconds`, then the trigger fails visibly. Same rule as
   a cold start: a fallback means "does not fit what lmgw may use right now".
9. **Background traffic (phase 4) must never climb an Owner-owned ladder.**
   Ownership is not built here; the climb goes through one named permission
   check that allows every climb today and is where phase 4 plugs in
   (unified-KV spec §9, "background may climb only the primary, and only
   when it owns it").
10. **"Drain" waits for sends, not for claims.** A registry claim is not a
    request in flight: the in-process runners (`/v1/responses`, agent chat,
    MCP sampling, batch runs, quickdoc) hold one claim across a whole tool
    loop, idle between turns, so "wait until `in_flight == 0`" (§3.4 step 2)
    could never end while one of them is open. A climb therefore waits for
    the sends in flight on the old rung, counted per send by the gate, and
    blocks new sends and new claims while it runs. Idle claims are not
    waited for; they move to the new rung on their next send. A stale
    claim's dead-container recovery never stops a newer container.
11. **Rung numbers are 1-based outside lmgw.** `x-lmgw-rung: 1/3`, the
    `request_logs.rung` column and every surface count the base as rung 1
    (§6, §7 item 1); code indexes from 0.

WP1 (`feat/ladder-models`, 2026-09-27), taken during implementation under the same
"desktop app for one user, local and reversible" standing permission entry 6
gave phase 3 as a whole:

12. **`Rung.ctx_size` is `i64`, not `Option<i64>`.** A rung with no context at
    all is not a rung — the config type should not be able to express the
    nonsense §4.3 rule 3 refuses anyway. `gguf_path`/`ctx_size` both required
    keeps every derived helper (`per_slot_ctx`, `switchover`) a plain
    arithmetic expression instead of an `Option` chain for a case that can
    never legitimately occur.
13. **`chat_runtime`/`model_runtime_at`'s `rung` stays 0-indexed in code**
    (entry 11 already settles the convention: only external surfaces are
    1-based). An out-of-range rung — a stale caller after the ladder shrank —
    falls back to the base rung rather than panicking, the same defensive
    stance the rest of this codebase takes toward a caller that raced a
    config change; WP2's `switch_rung` is expected to have already checked
    `LocalModel::top_rung` before ever asking for one that far out.
14. **§4.3 rules 5 and 6 only run against the ladder's higher rungs, never
    re-checking the base.** Rule 5 is inherently a "does this rung match the
    base" comparison, so it has no base case to run. Rule 6 already has a
    base-row check today, in `ops::model_warnings` — non-blocking (a
    warning, not a refusal). Turning that pre-existing check into a hard
    refusal for every row that happens to also carry a ladder would be a
    behavior change outside what "a row without a ladder must validate
    exactly as before" asked for, so WP1 leaves it alone and only adds the
    hard version for the rungs the ladder actually introduces.
15. **Rule 7's projector check runs once, against the row, not once per
    rung.** The projector (`mmproj_path`/`no_mmproj`) is row-level and shared
    by every rung by construction (§4.1), so there is exactly one bound to
    know and one place to ask `gate::count::image_token_bound` about it —
    the base's `gguf_path`, which is what the running rung's text width would
    resolve to on the (rare) conditional-gemma4v path too, since every rung
    shares the same architecture by rule 5.
16. **The footprint advisory (§4.3, non-blocking) builds its own throwaway
    `vram::plan::PlanCache` rather than reaching into `VramScheduler`'s
    memoized one**, which `ops` has no accessor for. Save-time is not a hot
    path, so the lost memoization costs nothing; the capacity/headroom
    numbers themselves come from `state.vram.view(state)`, the same live
    view the dashboard reads, so the advisory silently produces nothing
    (never a false refusal) whenever there is no GPU telemetry to measure
    against — matching how every other capacity-dependent surface in this
    codebase already treats an unmeasured card.
17. **`request_logs.rung` and the `LogParams`/`InProcessLog` struct fields
    are wired end to end now, but every call site sets `rung: None`.**
    Nothing in WP1 produces a real rung to log — that is WP3's climb — so
    this package only proves the column, both row structs, both log readers
    (`/api/logs`, `lmgw__logs`) and the live `RequestSummary` frame already
    carry the field, the same shape phase 2 built for `fallback_reason`
    ahead of the code that would ever set it to something other than `None`.
18. **`gguf::TokenizerSignature`'s array hashes are a plain
    `DefaultHasher`, valid only for one process's own comparison, never
    persisted or compared across a restart.** The vocabulary/merges/
    token-type arrays are exactly what `Reader::read_value`'s generic path
    deliberately does not retain (a quarter-million entries), so rule 5 reads
    them with a second, narrower reader that folds each element into a
    hash instead — cheap because it is still one forward pass over bytes
    already being read, not a second file read.

WP2 (`feat/ladder-models`, 2026-09-27, runtime), taken during implementation under
the same standing permission:

19. **The runtime carries its rung; the entry carries the charge.**
    `ModelRuntime.rung` (`RungPos{index, of}`) is set where `-m` and
    `--ctx-size` are chosen, `Some` on every ladder row (index 0 from
    `model_runtime`). The registry entry records `RungCharge` (index, of,
    file, context) at the claim and at adoption, the gate facts carry the
    rung, and `RuntimeView` publishes `rung {rung, of, gguf}` (1-based),
    `climbing {to, of, reason, stage, seconds}` and `sends`, each skipped when
    empty, so a row without a ladder keeps its frame byte for byte (pinned by
    a test). The api-types mirror (`RuntimeStatus`) is WP5's; serde ignores
    the extra keys until then. Boot hands each ladder row's higher rungs to
    reconciliation as adoption candidates only, never as warm starts.
20. **Generation-checked stops, and the latent recovery bug.**
    `Registry::stop_generation` checks, in the lock hold that marks the entry
    stopping, that it is still the container that was judged and that no climb
    is replacing it (`RuntimeError::Moved` otherwise; absent is success).
    Recovery, eviction, the reaper and the hold sweep use it: the reaper says
    nothing, the sweep lists the model as draining, eviction moves on to the
    next victim. On a row without a ladder this only differs when a newer
    container already replaced the judged one. For recovery that was a bug. A
    tool loop's stale hold, after an override stop and a fresh start by
    another request, force-stopped the newer container and cold-started a
    third. The fix is pinned by a test. Plain `stop` (override, delete,
    disable, shutdown, apply, restart) still stops whatever runs, a climb
    included: forced stops win.
21. **Claim and start are one synchronous step** (`ClimbTicket::start`). The
    plan claimed under the gate and spawned the run afterwards, with a drop
    in between that could only forget the entry and leave the old rung
    running unaccounted. Now the flip (new generation, `starting`, charged at
    the new rung, sends reset) and the `tokio::spawn` of stop, start and
    settle happen in one call, made while the admission gate is held. A guard
    inside the task settles the entry as a failed climb if the task panics or
    dies with the runtime. `StartSpec` is `AcquireSpec`, owned.
22. **A failed climb sends its waiters back to admission.** The plan's
    `Gone(None)` would have let each waiter loop inside `Registry::acquire`
    and cold-start the base **unarbitrated**, because admission skipped the
    VRAM decision when the model was up on their arrival. Instead, a new
    `Phase::ClimbFailed` makes `acquire` return `RuntimeError::ClimbFailed`,
    and `admit_local` starts over: the model is not up any more, so it
    arbitrates, then cold-starts the base once for everyone. The trigger's 502
    names the rung and carries the start's error and log tail. Stale claims
    re-admit the same way through `sync`.
23. **A stop during a climb fails its waiters.** A stop that lands on a marked
    or starting climb tells parked acquirers "was stopped while it was
    climbing to rung k/n", as a stop during a start does, so nothing parked in
    the registry brings the model back. The climb aborts: the drain sees the
    entry gone, or the settle finds it not ours and removes the container it
    started. It also checks ownership between the old rung's stop and the new
    rung's load, so a lost race does not load a rung nobody wants. A held
    claim that syncs afterwards finds its entry gone and re-admits at the
    base, which is exactly what today's dead-container recovery does after an
    override stop.
24. **Entry 9's permission hook lives inside `vram::climb`** (`may_climb`,
    allows everything today), not in a `gate::ladder` that WP3 must remember
    to call. Every climb passes it, WP6's per-rung test included. Phase 4's
    denial is documented at the hook (fallback with `background` when the
    request may fall back, else a 503); no code is written for it yet.
    **(Superseded by unified-KV §12 entry 73:** a denied climb is a re-pick —
    the walk goes on to a loaded alternate, then the alias fallback — not a
    bare fallback-or-503.)
25. **`LocalHold.policy` is `Option<AdmissionPolicy{check, images}>`**, a
    named struct in `gate::open` rather than a tuple. `admit_or_fall_back`
    sets it once, for admissions that may fall back. Pinned callers and
    direct `vram::admit` callers get `None`. A re-admission swaps only the
    claim, so the hold keeps its policy. Phase 4's background flag becomes a
    field. **(Superseded: it lives on `LocalHold::origin` instead**
    (unified-KV §12 entry 61; `gate/open.rs`), not on `AdmissionPolicy` —
    `AdmissionPolicy` is present only when the request may fall back, while
    origin is a fact of every hold, background or not.)
26. **The drain's `/slots` check is in `vram::climb`, not the registry.** The
    registry waits for lmgw's own sends: a watch counter, closed when the
    entry goes. Then vram polls the container's `/slots` every `POLL` until
    idle within the same deadline. This is eviction's probe, with the same
    `CONTROL_TIMEOUT`, and an unanswerable `/slots` counts as idle. Busy slots
    past the deadline name `slots` instead of `sends` in the error.
27. **`GatewayError::LadderDrainTimeout {model, rung, waited_seconds, sends,
    slots}`**: 503, `vram_queue_timeout`, OpenAI `api_error`, Anthropic
    `overloaded_error`, like `KvPoolTimeout`. The message names the target
    rung `k/n (<gguf>)`, what was still busy, and `vram.queue_timeout_seconds`.
    Admission's own refusals stay `VramQueueTimeout` and `VramTooLarge`, with
    the model named `<id> rung k/n (<gguf>)`.
28. **Budgets, inactive admission, the hold.** One budget from the mark,
    `vram.queue_timeout_seconds` (0 = none), covers the drain and the
    admission. The old rung's stop is bounded by its unload timeout and the
    new rung's load by the load timeout; nothing new is invented. With
    admission inactive (switched off, or no telemetry and no budget), a climb
    still marks and drains, then starts unarbitrated, as a cold start's
    `acquire` does then. The hold is checked before the mark, after the
    drain, and on every pass of the admission wait up to the claim. After
    the claim the start completes, like any start admitted before the toggle.
    Under the hold the fallback is judged as the resolve-time hold swap
    judges it: the route check runs, the image rule does not.
29. **The climb's verdict is `verdict_for(needed)`.** `verdict` keeps its
    callers and its "the model is up" exit. The measurement moved into
    `verdict_for(state, snap, target, needed, fill)`, which it calls and
    which a climb calls with the new rung's footprint plus headroom. Rechecks
    while waiting use the same number. `external_armed` did not need
    splitting: the climb reads `trigger_off` and `Snapshot::fallback_route`
    directly.
30. **A target that is not a climb is `Done`, not an error.** Three cases get
    `Climbed::Done` ("sync, judge again") without marking anything: `to` at
    or below the rung the hold's container runs, `to` above the row's current
    top, and the row gone. In each the caller's judgement is stale. `Marked`
    has `Joined{wait, raised}` for any climb in progress (raised only before
    the claim) and `Stale` (no receiver) for a claim on an older container or
    a gone entry. A raised target is planned again, and size-checked, on a
    fresh snapshot after the drain. WP3 must drop its own `SendGuard` before
    calling `climb`; otherwise the drain waits for it until the budget runs
    out.

WP3 (`feat/ladder-models`, 2026-09-27, request path), taken during implementation
under the same standing permission:

31. **One send helper, and the pair is its own unit of retry.**
    `gate::send::send_gated` replaces `vram::send_local` at every fitted chat
    site. A row without a ladder takes one `Option` check and then the very
    same `send_local` call, so its body, headers and retry are unchanged. On
    a ladder row the helper runs its own copy of "recover once, then fail",
    with the same wording and the same `mark_failed`, instead of nesting
    `send_local`'s retry inside the pair. A nested retry would have moved
    the send to a fresh container while the count stayed on the dead one.
    The recovered pair is recounted and resent on the base (§3.5).
32. **A ladder that cannot be judged is a 500 naming the fix.** The fit
    refuses a ladder start with a KV cache shared by its slots (entry 7's
    precondition), with no positive `n_predict` (§4.3 rule 1) or with no
    context (rule 3). Validation refuses these rows on save, so only an old
    row or a hoisted freeform flag gets here. The plan asked for this in
    the unified case only. The other two cases have the same shape, and
    sending those rows unclamped and uncounted would quietly drop every
    promise a ladder makes.
33. **Climbs only go up, the backstop included.** The target is the
    smallest rung *above* the one the send was judged on, even when the
    row's arithmetic says the running rung would hold the request. After a
    backstop, llama-server has refused on that rung itself, and its `n_ctx`
    beats the division. The plan's `debug_assert` (target per-slot above
    the judged per-slot) became `to > running`, which holds by
    construction. The per-slot comparison can fail legitimately: the target
    comes from the current row, and the judged number from the running
    container's start facts.
34. **What waits for the verdict.** Every answer except a `400` is held
    until the count is in, error statuses included: a `500` from a rung
    the request does not fit is not the request's answer. A `400` is read
    at once. `exceed_context_size_error` is the backstop. Any other `400` is
    handed over straight away, rebuilt from the bytes already read, since
    no climb fixes a malformed request. After the one backstop retry, a
    second context refusal is handed over the same way, and the site turns
    it into `context_length_exceeded`, as before.
35. **A climb's fallback re-enters each site once, with no hold.**
    `/v1/chat/completions` and `/v1/messages` go through `serve_opened`, the
    legacy path through `serve_legacy`, the dashboard chat through `relay`,
    and `sample_once` / `stream_once` recurse. Each is served on the
    fallback route with its reason. The re-entry has no hold, so it cannot
    climb or fall back again: "once" is structural, not a counter. The
    first pass writes no log row and balances its in-flight gauge. On the
    legacy path the fallback gets the body as the client sent it (a copy
    taken on ladder rows only). The dashboard chat takes its
    `timings_per_token` back out, because a cloud upstream refuses it. The
    load test's probe is admitted without a fallback policy, so a
    `Fallback` there is reported as lmgw's own bug.
36. **The rung on refusals.** `x-lmgw-rung` and `request_logs.rung` name
    the rung a request was judged on even when it is refused: a
    `context_length_exceeded` above the top rung, a climb's VRAM or drain
    error, a fit's media refusal. §6 asks for the header on every terminal
    response of a ladder row, and the log row agrees with the header. Both
    are empty when a fallback answered (`GateHeaders::fall_back` clears the
    rung) and on rows without a ladder. `/v1/responses` stamps the rung
    running at open, like `planned_clamp`, and each turn logs its own rung.
37. **`ContextExceeded` gains `top_rung`.** Above the top rung the message
    reads "exceeds the top rung's per-slot context of N tokens (rung 3/3
    (top.gguf))". It keeps the existing `400 context_length_exceeded` /
    Anthropic `invalid_request_error` mapping. Every other refusal keeps its
    wording.
38. **Bounds.** The count is bounded by the route's request timeout, over
    the whole count (template and tokenize, or every legacy prompt).
    `on_running_server` used to bound each call the same way. An in-process
    caller's deadline bounds the whole ladder send: sync, count, climb and
    retry. A client request's climb is bounded by the climb's own budget
    and the load timeout. A model that comes back without a ladder
    mid-send (the row was edited, then a recovery restarted it) is sent
    like any row without one.
39. **The verdict test climbs 1→2, then 2→3.** With `n_predict` 16 and
    rungs of 64, 128 and 512, a prompt the base accepts (at most 64 tokens)
    can never need rung 3. So "the answer comes from the rung the model
    climbed to, never the one that answered first" is shown once per step:
    unary, then streamed. The mock's context rules are opt-in, on the
    ladder fixture only: the pool fixtures render `--ctx-size` too, and
    they keep their own mock.

**Live check (2026-09-27, dev instance, RTX 4090, image
`localhost/lmgw-llama-server:official-master`).** A three-rung row:
Qwen3.5-0.8B UD-Q4_K_XL at 4096, the same file at 16384, then Qwen3.5-2B
UD-Q4_K_XL at 65536 (`parallel` 1, `n_predict` 256, no projector). The 2B
file passed rule 5 against the 0.8B base (same tokenizer and template).
Qwen3.8-27B load times, 27B prefill and real VRAM per rung are not part of
this record (D4).
- Save refusals live: auto slots (rule 2), a projector as a rung (rule 5),
  a projector without `--image-max-tokens` (rule 7).
- Cold start at the base: answered in 1.4–1.7 s.
- Climb 1→2 (same file, 4096 → 16384): 1.75 and 1.97 s from mark to ready,
  2.1–2.2 s for the trigger's whole answer (a 3,898-token prompt). A short
  request sent 1 s into the climb waited and was served on rung 2 (0.93 s).
- Climb 2→3 (the 2B file, 65536): 2.2 s mark to ready. The trigger (16,820
  tokens) answered in 3.1 s; a short request sent mid-climb was served on
  rung 3 (1.4 s).
- A 4,640-token prompt on the 4096 base: llama-server refused it, one climb
  to rung 2, one retry, 200 in 2.2 s. Above the top rung (65,820 tokens):
  400 in 0.10 s with the three numbers.
- Streamed climb: `x-lmgw-rung: 2/3`, first line after 1.98 s, nothing from
  rung 1. A client `max_tokens` of 1000 came back clamped to 256 with the
  header.
- Back to base: `restart` (the op restarts at rung 1), an edit applied to
  the idle model, and the idle reaper (gone 16 s after a 10 s idle limit);
  each next request ran rung 1. `request_logs.rung` read 1, 2, 1, 2, 1 and 3
  on the 400; `/v1/models` published 65536.
40. **The counting cost, measured again on the running image.** Directly
    against the container, repetitive text: `/apply-template` 2, 11 and
    31 ms, `/tokenize` 3, 43 and 138 ms at 1.6k, 37k and 120k tokens. Varied
    words with reasoning in the history: 2 / 7 ms at 2.7k, 14 / 114 ms at
    67k, 42 / 354 ms at 216k. So on this image `/tokenize` is most of the
    cost and the whole count is an order of magnitude below entry 5's
    figures, which were taken on an older image. Entry 7 stands: the
    overlap is exact and hides even this. A byte bound would now skip the
    larger half, but under the overlap it would save only llama-server CPU,
    so it is not built. lmgw's own JSON handling of the count was not
    timed separately.

WP5 (`feat/ladder-models`, 2026-09-27, UI), taken during implementation under the
same standing permission:

41. **`LocalModel::ladder` reaches the editor now.** WP1 declared the
    api-types mirror (`Rung`) but `ops::local_model_get`'s hand-built `json!`
    never named the field, so `/api/local-model` dropped it and
    `LocalModelDetail` had nowhere to put it. Both are added here
    (`#[serde(default)]` on the new `LocalModelDetail::ladder`), and
    `RuntimeView.rung`/`climbing`/`sends` get their promised api-types mirror
    (`RuntimeStatus`, plus `RungStatus`/`ClimbStatus`), the same
    `#[serde(default)]` shape WP2 entry 19 specified — an older frame still
    decodes.
42. **Footprint and MTP get one small new endpoint, `/api/ladder-rung-plan`**,
    because neither existing read fits: `model_inspect` sizes KV at the
    GGUF's own trained `context_length`, not the rung's chosen `ctx_size`, and
    `local_model_plan` derives a whole parameter set, not one rung's numbers.
    The handler (`ops::ladder_rung_plan`) takes the row's shared fields
    (cache types, projector, drafter, GPU layers) bundled in a `RungPlanInput`
    — a plain struct instead of the seven separate arguments clippy's
    `too_many_arguments` refused — builds a throwaway probe `LocalModel` and
    reuses `vram::plan::PlanCache`, the same arithmetic the save-time
    footprint advisory already runs. Per-slot context and switchover need no
    round trip; they are plain arithmetic in the editor, mirroring
    `ladder::LocalModel::per_slot_ctx`/`switchover` by hand since that module
    is core-only and not reachable from wasm.
43. **The ladder toggle and its rung list live outside the generic
    `entries`/`Val` dirty system.** That system is one flat string signal per
    field; a rung list is a dynamic `Vec`, so it gets its own baseline
    (`ladder_on_baseline`, `ladder_baseline`) and folds into the `dirty` Memo
    as a single `("Ladder", "ladder")` entry when the toggle or any rung
    differs from what was loaded — one line in the dirty count and the
    SaveBar detail, not one per rung. Discard rebuilds `rungs` from that
    baseline with fresh row keys; Save either sends `ladder` (the higher
    rungs, `Rung`-typed) or names `"ladder"` in `clear`, the same convention
    every other optional field on this form already uses.
44. **The footprint/MTP fetch is debounced 300 ms per rung**, the same
    one-request-per-pause idea `traffic.rs`'s filter boxes use, so typing a
    context size does not fire one request per keystroke. The query-string
    assembly is split into a pure `rung_plan_pairs` (which fields, and
    whether there is anything to ask yet) and `rung_plan_query` (the same
    plus `urlenc`, which calls `js_sys` and needs a JS host) — this crate has
    no `wasm-bindgen-test` harness, every other `#[cfg(test)]` module here is
    plain-Rust helpers, so the split is what keeps the decision logic
    host-testable at all. The runtime rung/climbing badges have the same
    limit for the opposite reason (they need a live registry entry, which
    the test suite cannot start): both were checked live on a dev instance
    instead (a three-rung Qwen3.5 row; save refusals, the table's derived
    columns against `/api/ladder-rung-plan` directly, the dirty/discard round
    trip) rather than by a test in this crate.
45. **Max output (`n_predict`) is shown twice**, once in its usual
    Template & reasoning card and once beside the ladder table, both bound to
    the same `fields["n_predict"]` signal — editing either place edits the
    same value. Simpler than moving the field, and it is what §6 asked for
    ("shown next to the table") without inventing a second source of truth.
46. **The request-log rung is a badge on the Model column, not a new
    column.** `request_logs.rung` is a lone integer (the rung a request was
    judged on, not "k of n" — the log never stored a rung count), so
    `traffic.rs` follows the `fallback_reason` badge precedent exactly rather
    than adding an eighth column and updating both `colspan="8"`s. The
    dashboard runtime rows (Overview's table, the GPU popover, the editor's
    `ContainerStatusRow`) show `rung k/n` instead, since `RuntimeView` does
    carry the total there.

Review (phase 3), for review. These are fixes to the runtime and the climb
after the adversarial review of WP1–WP4, under the same standing permission:

47. **A stop under way wins against the climb task** (review finding 2).
    `stop_where` marks the entry `stopping` long before `podman wait` lets it
    remove the entry. The climb task's ownership checks now require the
    entry to still be `starting`, not only the same lineage and generation.
    Three checks change: the stage check before the load, the
    start-failure cleanup, and the unsettled guard. A stop under way
    therefore keeps the climb from loading a rung nobody wants, which would
    otherwise be unaccounted and under the name the stop is taking down. A
    start that fails under such a stop ends as `Aborted`, not `ClimbFailed`,
    so no waiter is sent back to admission to bring the model up again, and
    the trigger hears "stopped while climbing". A `wait` gate on the fake
    podman pins both interleavings. What remains is `StartClaim`'s own window
    (a stop that begins after the check, during `podman run`), which the
    settle's `rm_force` closes as before.
48. **An arrival after a stop took a climbing entry waits for the key**
    (finding 9). Parking a new acquire on a `stopping` entry as on a start
    made it inherit the climb's "stopped while climbing" failure. It now
    waits for the key like after any other stop, then starts the base.
    Waiters that were already parked on the climb still fail with it
    (entry 23).
49. **The climb judges the holder on its own rung; a joiner's raise is
    opportunistic** (finding 3). After the drain, the holder's own rung is
    what the request is judged on: the card-size check, what admission waits
    and evicts for, the outside-VRAM recheck that can answer with its cloud
    fallback, and the name in a timeout. The joiner's raised rung is claimed
    only when it fits at a ledger read. Otherwise the holder climbs to its own
    rung, and the joiner (woken by the settle) judges again on its own
    verdict. A raised rung that is larger than the card is dropped at once.
    If the joiner's rung is taken and fails to start, the holder, which never
    needed it, gets `Done` and climbs to its own rung from the base
    (entry 52).
50. **The climb always plans again after the drain** (finding 4). The holder's
    rung is rendered from the snapshot current after the drain, not only when
    a joiner raised the target. An edit saved while the climb waited (a
    rung's file or context, `parallel`) takes effect in the climb, which is
    what the plan's race table promised. A rung the edit removed ends the
    climb as `Done`: judge again.
51. **A failed climb's waiters re-read the hold and the row** (finding 5).
    `admit_local`'s retry after `ClimbFailed` reads the snapshot again. A GPU
    hold switched on meanwhile refuses the request with `gpu_hold` instead of
    cold-starting the base under it. The base is rendered from the current
    row, with a 502 when the row is gone.
52. **A joiner of a failed climb gets that failure, when it needed exactly
    that rung** (finding 7). `Phase::ClimbFailed` carries the rung and the
    start's error, and `PhaseWait::outcome` says how a wait ended. A trigger
    that joined a climb to its own target rung, which then would not start,
    returns the same 502 instead of re-admitting the base only to try the
    broken rung again. The review proposed `to ≤ failed` as the rule; the
    rule is `==`. A joiner that needed a lower rung (it joined without
    raising) may well start that rung, so it judges again. Acquirers parked
    on the climb keep entry 22's re-admission.
53. **An admission waiting for a busy model yields the gate to a pending
    climb** (finding 8). After each wait for a busy model, `decide` and the
    climb's admission give the gate up and queue for it again whenever
    another model's climb is marked but not yet started. The FIFO mutex then
    lets the climb decide first. This breaks the cycle in which the waiting
    admission needs the ladder model's memory while that model is busy only
    because its trigger waits on the gate; with no queue timeout that cycle
    was a deadlock. Two rejected alternatives:
    - Failing fast refuses a request that the climb might soon make room for.
    - Taking the gate before the mark holds the gate through the drain, which
      can last as long as a generation and would block every other model's
      admission meanwhile.

    The cost of the yield: while a climb is pending, a waiting admission loses
    its FIFO place to requests that queued after it. The loop that takes the
    gate (in slices, with the verdict retaken) is one `take_gate`, shared by
    `decide` and the climb.
54. **A drain timeout's busy slots may be a send lmgw just dropped**
    (finding 16). The message now says "a client on its own port, or a send
    lmgw just dropped". A "does not fit" verdict (entry 7) or a hung-up
    client leaves a slot busy until llama-server notices the closed
    connection.
55. **The queue row shows what the climb's admission waits for** (finding
    18). The waiter's `needs` is set again after the post-drain plan
    (`VramScheduler::set_needs`). Under entry 49 that is the holder's own
    rung, never a joiner's raise, so the pre-raise size was not wrong in
    itself; an edit during the drain (entry 50) is what can change it.
56. **Test gaps closed** (finding 14):
    - `vram::climb`'s joined-raise path runs twice with direct calls: a raise
      that does not fit now, and one that is short of outside VRAM with a
      fallback.
    - A forced stop lands during the climb's own stop of the old rung, and
      during its load, on a fake `podman wait` that parks.
    - An edit during the drain takes effect in the climb.
57. **A rung is judged and published on the slot its trained context
    leaves** (review finding 1). llama-server caps every slot at the GGUF's
    trained context. §4.3 gains rule 3b: it refuses a rung, the base
    included, whose per-slot context is above its GGUF's
    `context_length`. The message names both numbers and the largest
    `ctx_size` that fits. A row saved before this rule is judged on the
    real slot anyway: a ladder start now reads its trained context, and
    `ladder::slot_ctx` caps the per-slot in the rung's facts, so
    `x-lmgw-rung`, the fit and the backstop's climb use it. The climb target
    caps each rung of the current row the same way, through the GGUF
    summary cache. `/v1/models` publishes the capped top rung, quickdoc
    sizes a ladder's windows on the capped base, and
    `/api/ladder-rung-plan` returns `trained_context`. The editor shows
    the capped slot, and says a rung past it cannot be saved. This is not
    an lmgw-invented limit: it is the number the server enforces, shown
    wherever the configured one was. Rows without a ladder are unchanged.
    A split row configured past its trained context still publishes the
    uncapped number, as the review noted, and that stays out of scope.
58. **One read for the container and its facts** (finding 6).
    `LocalHold::attempt_with_facts` reads the port, the generation and the
    start facts under one lock of the claim. The send helper judges with
    that triple, and after `begin_send` it checks that the claim is still on
    that attempt, otherwise it judges again. A shared hold that another
    task moves between two reads can no longer get a send judged on one
    rung and answered by another. The race cannot be forced from a test,
    so the test pins that the one read agrees with the registry before
    and after a climb.
59. **`/v1/responses` names a fallback on the unary answer** (finding 13).
    `sample_once_noting` reports a turn that a climb handed to the
    fallback, and the unary answer, which leaves after its last turn,
    carries `x-lmgw-fallback` with its reason instead of the rung stamped
    at open. A stream's headers left before its first turn, so it keeps
    that rung. The `lmgw.headers` block says so. Each turn's log row
    records its own fallback, as before.
60. **Stale comments** (finding 17). Fixed in api-types and in the
    round-trip test. Migration 0043's own comment stays: sqlx checksums a
    migration's text, comments included, and a dev database that already
    applied it would refuse to start.
61. **The ladder send's loop has no iteration cap** (finding 19,
    rejected). The review proposed a visible error after `of + 2` climbs.
    That number is not provably right. Another request's climb that fails
    at a joiner's rung sends this one back to the base, legitimately, and
    this one then climbs again. So a cap derived from the ladder would
    refuse a request that is still making progress. Instead the loop's doc
    states why it ends:
    - every pass climbs strictly higher, spends the one recovery or the
      one backstop, or follows another actor's change;
    - every wait is bounded by a real setting (the climb budget, the load
      and unload timeouts, the route timeout, an in-process caller's
      deadline).
62. **The gate's side of "two triggers at once"** (finding 14). A request
    test drives two admitted requests whose overlapped counts both say
    "does not fit". The second joins the first's mark and raises it before
    the claim, one reload to rung 3 serves both, and a send in flight
    finishes on rung 1. The mock gains a per-prompt `/apply-template` delay
    (by word tag) so the two verdicts land in a set order. The runtime
    halves of finding 14 are entry 56's.

Review follow-up (phase 3, second pass), for the four findings the runtime
review fixes did not reach — under the same standing permission, taken
during implementation:

63. **`disable` skips `validate_ladder`; every other action still runs it**
    (finding 10). A rung file going missing or unreadable after save used
    to refuse `disable` along with everything else, and `clear: "ladder"`
    was the only way out — exactly the "never block the owner from turning
    their own row off" rule this desktop app runs on. `disable` alone is
    exempted (both the per-rung `check_path` loop and `validate_ladder`);
    `enable` still validates, since it is bringing the model back into
    service, and a plain `update` still validates too, since telling here
    whether it touched anything ladder-relevant would need re-deriving the
    effective row twice. A stale rung path now surfaces as a problem in
    `chat_model_problems` (`lmgw__local_model_check`'s group pre-flight)
    instead of only ever showing up as a climb's 502. Contract change: a
    ladder row can now be disabled while carrying a validation-refusing
    ladder; every other mutating action refuses exactly as before.
64. **The tokenizer signature closes three of finding 11's holes.**
    `tokenizer.ggml.scores` joins the hashed arrays — SPM/UGM tokenizers
    merge by score and carry no `merges` list at all, so a `merges`-only
    comparison missed them entirely. `add_space_prefix`/
    `remove_extra_whitespaces` (SPM/UGM input normalization) are compared
    as plain booleans. Every `tokenizer.chat_template*` key is now captured
    and compared, not only the bare one: `common_chat_templates_init`
    picks a named variant (e.g. `.tool_use`) when a request carries tools,
    so two rungs that agree on the default template but differ on a named
    one still rendered a tool-calling request differently.
    `precompiled_charsmap` (the UGM normalizer's character map) was read as
    a GGUF string here, with a documented "lossy UTF-8" gap accepted — that
    described the wrong mechanism (see entry 77: it is an array upstream,
    never a string, so the field was always `None` and the check a no-op,
    not merely imprecise). Contract change: none — every new comparison
    here can only turn an existing "identical" verdict into a refusal,
    never the reverse, and a row without a ladder never calls any of this.
65. **A freeform `--spec-type`/`--model-draft` reaches rule 6; a
    projector's embedding width is compared per rung** (finding 12).
    Neither flag was in `PROMOTED_ARGS`, so a freeform `--spec-type
    draft-mtp` or `--model-draft foo.gguf` reached rule 6 unfolded and
    bypassed it. **Superseded by entry 75:** the first version of this fix
    promoted both flags into `PROMOTED_ARGS`, which second-pass review
    finding S5 found changes every row that carries them freeform, not
    only a ladder's — entry 75 reverts the promotion and has
    `validate_ladder` read the effective value itself instead. The
    embedding-width half of this entry stands as written: with a projector
    loaded, each rung's `{arch}.embedding_length` is compared against rung
    1's — llama.cpp refuses to pair a projector with a text model of
    another width, and rule 5 (architecture + tokenizer only) had no way to
    catch a rung that changed it; `None` on either side stays unrefused,
    the same "the header does not say" stance rule 3b takes. Contract
    change: a ladder with a projector now also compares embedding widths —
    it can only turn a previously-accepted (and previously load-failing)
    row into a refusal.
66. **`validate_ladder`'s own doc comment named the wrong mechanism**
    (finding 15, nit). It credited `hoist_promoted_args_into` with folding
    freeform `--ctx-size`/`-c`/`-m`/`--parallel` into the typed fields;
    none of the three is a promoted arg, so nothing there ever touched
    them. What actually keeps a freeform spelling of any of them inert is
    the argv renderer's own dedup (`push_freeform_args`): it claims
    `ctx-size`/`parallel` for the typed field whenever that is set
    (mandatory on a ladder by rules 2–3) and always takes `-m` from
    `gguf_path`, so llama-server never sees the freeform copy regardless of
    what `validate_ladder` does. The behaviour was already correct — only
    the comment was wrong, now naming both mechanisms (hoisting for the
    genuinely-promoted flags, dedup for the never-promoted three) and
    citing this entry. No contract change.

WP6 (`feat/ladder-models`, 2026-09-27, MCP + docs), taken during implementation
under the same standing permission:

67. **`ladder` is a flat string in the MCP schema, like every other argument
    in this module — a first draft made it a genuine JSON array, and
    `mcp::selfadmin::tests::all_parameters_are_flat_scalars` said no.** §6's
    "gains `ladder` (array of `{gguf_path, ctx_size}`)" reads naturally as a
    real JSON-schema array, and `LocalModelPatch.ladder` being already
    `Option<Vec<Rung>>` (not `Value`, unlike `capabilities_override`) made
    that look free: `ops::patch_from_args`'s generic `serde_json::from_value`
    deserializes a real array straight into it, no parsing step needed. That
    draft built, compiled and passed every test it came with — the crate's
    one blanket check that every tool argument stays a flat scalar lives in
    `mcp/selfadmin.rs`'s own test module, not `ladder_models.rs`, and nothing
    up to that point had run it. `ladder_p` now just wraps `str_p`; a new
    `hoist_ladder_arg` (`mcp/selfadmin.rs`) runs before `patch_from_args`,
    parsing a JSON-encoded string into the array `Vec<Rung>` still expects —
    the one field in this module where the string has to be unwrapped a step
    earlier than `capabilities_override`'s (parsed downstream, at save time,
    because its patch field is `Value`) since here the target type is
    already the array itself. A caller that sends the array directly anyway
    is still accepted (the same leniency `parse_capabilities_override` gives
    the other way, an object where a string is declared), which is what let
    the first draft's own tests keep passing unchanged once the schema
    changed under them. Contract (final): `ladder` is a JSON-encoded string
    argument, e.g. `'[{"gguf_path":"top.gguf","ctx_size":65536}]'`.
68. **`local_model_get`'s per-rung detail lives in a new `"rungs"` key,
    `null` on a plain row** — not appended onto the existing `"ladder"` key,
    which stays the raw stored rungs (§4.1's `Vec<Rung>`, unchanged since
    WP1) so a caller that only wants to know whether a row is a ladder still
    reads one small array. `command_line_preview_at` (`ops.rs`) is
    `command_line_preview` plus a rung argument, threaded down to
    `model_runtime_at`; rung 0 renders byte-identical to the existing
    top-level `command_line`, so the base's numbers reach a caller two ways
    now (the row's own field, and `rungs[0]`) rather than only through the
    top-level one. `LocalModel::all_rungs`/`per_slot_ctx`/`switchover`
    (`ladder.rs`, WP1) already had every number this needed; nothing new was
    derived.
69. **`local_model_test`'s per-rung mechanics: stop, admit, climb, climb,
    …, stop** (§6: "that is also WP0's instrument"). `vram::climb`'s own doc
    already named this use ("a per-rung test can call it directly"), so the
    test drives the same primitive a request's climb does, rather than a
    parallel path that starts each rung's container by hand. A running
    container is stopped first — refused, touching nothing, while it is
    busy (`Registry::stop`'s existing `Busy` refusal, not a new check) — so
    every run starts from the base regardless of what real traffic left the
    model on. A rung that fails to climb (or whose probe fails) ends the
    run there, since nothing above a broken rung is reachable through it —
    rejected the alternative of skipping ahead to try a rung above the
    broken one directly, which would leave an untested gap silently
    reported as untested-because-never-reached, easy to misread as "fine".
    The closing stop is deliberately not forced, unlike the opening one:
    forcing it would kill a request that started mid-test, so
    `reset_to_base` can come back `false` on a busy box — named in the
    result, not hidden. Building this caught a real bug in the draft: the
    test's own claim (`hold`, kept alive to probe every rung) was still open
    when the closing stop ran, which then refused it as "busy" against
    itself — fixed with an explicit `drop(hold)` first, pinned by the happy
    path test asserting `reset_to_base: true`.
70. **The generation probe is factored out (`generate_probe`), the rest of
    `local_model_test` is not.** Only the `Probe::Generate` arm is common
    between the plain test and a ladder's per-rung one — aux rows (embed /
    rerank) never carry a ladder (design §11), so `Probe::Embed`/`Rerank`
    have no ladder counterpart to share code with, and the three probes'
    request-building and response-parsing were already interleaved through
    one `send_gated` call and one match on `resp`. Splitting `Generate` out
    of that union would have cost roughly the same lines the standalone
    helper does, for a riskier diff against a well-covered existing path.
    `generate_probe` sends exactly the existing probe's body ("hi",
    `max_tokens: 1`) — always far under every rung's switchover, so it is
    never itself a reason to climb — and the plain path's own `Generate` arm
    is untouched code, not a call to the new helper.

Review, second pass (runtime and climb), for review:

71. **The gate is yielded only to a climb queued for it** (S1). Entry 53's
    yield fired for as long as another model's climb was marked, its whole
    drain included, although a draining climb cannot use the gate. Every
    `POLL`, the waiting admission gave its FIFO place to whoever queued
    behind it, for as long as the drain lasted. Now the climb's admission
    flags itself at the gate (`ClimbTicket::set_at_gate`) exactly while it is
    queued there, and only that makes another admission yield. The flag is on
    `ClimbStatus` but not published. The cycle stays broken: once the drain
    ends and the climb queues, the waiting admission yields at its next pass.
    (Climb against climb, where each needs the other's memory, the yield
    alone only hands the gate back and forth: candidate-aliases spec §12
    entry 93 has one of them give way at once.)
72. **A unary `/v1/responses` answer the fallback served drops the planned
    clamp** (S2). `x-lmgw-max-tokens-clamped` is planned at open and
    describes the local turns. When a ladder climb hands a turn to the
    fallback mid-run, that turn gets the client's own, unclamped value. The
    unary answer already drops the rung stamped at open for that reason, and
    now it drops the clamp too. A stream's headers left before its turns and
    keep both, as the `lmgw.headers` block says for the rung.
73. **After the drain a climb re-picks its rung by the request's need** (S6).
    `vram::climb_for(state, hold, to, need, reason)` is the send path's climb.
    After the drain it picks the smallest rung of the current row whose slot
    holds the request's prompt plus max output and is bigger than the running
    slot. Slots are capped at the trained context, as the gate judges them.
    The review's alternative, keeping the index and returning `Done` when
    that slot is not bigger, still reloads once to a rung too small for the
    need; `send_gated` knows the need, so passing it costs one parameter.
    `vram::climb` (an explicit rung, as `local_model_test` asks for) keeps the
    index. Either way, a rung no bigger than the running slot ends the climb
    as `Done` without a reload, and a joiner's rung is taken only when it is
    bigger than the holder's.
74. **A timeout after yielding the gate names what holds the memory** (S7).
    An admission that yielded the gate to a climb and ran out of time back in
    the queue was still waiting for busy models. Its refusal now names them
    (`describe_holders` on its last ledger) instead of "another request was
    still being admitted to the GPU", in `decide` and in the climb's admission
    alike.

Review, second pass (validation), for review — entries 65 and 66's own area,
taken during implementation under the same standing permission:

75. **`spec-type`/`model-draft` are reverted out of `PROMOTED_ARGS`; rule 6
    reads their effective value instead** (S5). The first version of finding
    12's fix promoted both flags so a freeform spelling could not bypass
    rule 6 — but `hoist_promoted_args_into` runs at every row's every load,
    so it also hoisted them on rows without a ladder: argv order changed
    (a first boot after upgrade fails the `Config.Cmd` match and replaces
    such a running container instead of adopting it), a relative
    `--model-draft` gained a `/models/` rewrite, the drafter joined the
    VRAM plan, and `modelinfo`'s typed-field warnings started firing for
    rows that never asked for them — exactly the "rows without a ladder
    are unchanged" rule this phase runs on. Neither flag is promoted now;
    `validate_ladder` reads the effective spec type / draft-path-is-set
    itself (`effective_spec_type`/`effective_draft_path_is_set`, typed
    field or `modelinfo::arg_value` over the freeform args, canonical
    spellings included — `-md` for `--model-draft`), without ever writing
    either back into `params`. Contract change: none beyond finding 12's
    own (rule 6 still catches a freeform bypass); the regression finding
    12 introduced on non-ladder rows is gone, pinned by a test that such a
    row with the same freeform flags saves with `params.spec_type`/
    `draft_gguf_path` unset, its `args` untouched, and its rendered argv
    unchanged.
    **Addendum (final cross-phase review, finding X2):** the same
    over-broad-canonicalization mechanism recurred later, for the *older*
    promoted flags this entry left alone. Unified KV's own hoist for
    `-kvu`/`-no-kvu` was routed through the argv renderer's full
    `SHORT_ALIASES` table instead of a table scoped to just that pair, so
    it silently started hoisting the unrelated pre-existing short flags
    `-ub`, `-n`/`-predict`, `-cram`, `-mm`, `-rea` too — the identical
    class of regression this entry describes, just via a different route
    in. Fixed the same way: `hoist_promoted_args_into` canonicalizes only
    `-kvu`/`-no-kvu` (`config::hoist_canonical_key`), and every other
    promoted flag matches only its literal long spelling again, pinned by
    `tests/it/config_hoisting.rs`.
76. **The ladder's `disable` exemption follows the resulting `enabled`
    state, not the action name** (S3). Finding 10 (entry 63) exempted
    `action == "disable"` from `validate_ladder`, but the editor's Save
    always sends `action: "update"` — including when the owner just
    unticks the checkbox — so the one path an owner actually uses to turn
    a broken model off from the UI was still refused; only the
    Models-list menu's own Disable action worked. The exemption now reads
    the same `enabled` value the row is about to be saved with (`enable`
    forces it `true`, `disable` forces it `false`, a plain `update` keeps
    it unless the patch says otherwise), so any save that leaves or makes
    the row disabled skips the check, and any save that leaves or makes it
    enabled still runs it — `enable` always validates, and so does a plain
    `update` on a row that was already enabled. A row already disabled
    that is merely updated (not re-enabled) stays exempt, which is the
    same "turning it off, or leaving it off, never needs its ladder to be
    healthy" rule, just reachable more than one way now. Contract change:
    a disabled ladder row can now absorb any update, not only a bare
    `disable`, while carrying a validation-refusing ladder; re-enabling
    (or any save that sets `enabled: true`) still refuses exactly as
    before.
77. **`precompiled_charsmap` is hashed as the array it actually is**
    (S4). Upstream (`llama-vocab.cpp`) requires it as a GGUF array of
    `INT8`/`UINT8`; finding 11's first pass read it with `meta.string(..)`,
    which only ever reads a `VT_STRING` value, so the field was always
    `None` on both sides of every comparison and the check was a silent
    no-op. It joins `HASH_ARRAY_KEYS` like `scores`/`tokens`/`merges`/
    `token_type` — hashed rather than materialized, since a real charsmap
    can run to hundreds of kilobytes. Contract change: none in practice
    yet (UGM/T5-style tokenizers are rare on chat rows, per the review),
    but the check now does what entry 64 already claimed it did.
78. **`/api/ladder-rung-plan` resolves its companion paths the same way it
    resolves `gguf_path`** (S10, nit). `mmproj_path`/`draft_gguf_path` went
    into the probe row exactly as the caller spelled them — no `..`/
    absolute-path guard, no existence check — while `gguf_path` already
    went through `modelinfo::resolve` for both. The route is admin-only
    (owner bearer), so this was never a real exposure on a single-user
    box, but it let an unresolved path (or a typo) produce a footprint
    estimate built from wherever `Path::join` happened to land, silently.
    Both companions now call `resolve` when set, and stay absent from the
    probe when not — an incomplete edit (no projector typed yet) still
    shows "—" rather than a refusal, the same UX the endpoint already gives
    an unset `ctx_size`. The optional half of the fix — passing the row's
    freeform args too, so a CPU-only `-ngl 0` row shows the right (zero)
    GPU footprint — is left out: it needs a new field on the HTTP query
    struct (`RungPlanQuery`) as well as `RungPlanInput`, a bigger change
    than the path-resolution fix for a footprint number that is already
    documented as an estimate everywhere else it appears. Contract change:
    a companion path that does not resolve (missing, or reaching outside
    the models dir) now shows as an error instead of a footprint computed
    against the wrong file.
79. **Three second-pass UI nits (S8, S9), plus the editor's own follow-on
    from S10.** S8: the ladder's dirty check compared the rung list
    unconditionally, so "on, add a rung, off again" still showed dirty even
    though Save's outcome (`clear: "ladder"`) is identical to a row that was
    never touched. The toggle changing is always real on its own (`ladder`
    vs `clear: "ladder"` differ regardless of what is in the rung list);
    only when the toggle reads the same both ways does the rung list get
    compared at all — extracted as `ladder_is_dirty`, a pure function, so
    the fix has a host-run unit test rather than only a manual round trip.
    S9: the "climbing to k/n" badge (Overview's runtime table, the editor's
    `ContainerStatusRow`) reused `.fallback-badge`'s exact class, whose
    amber means "a fallback answered" — a settled fact, not a transition.
    It gets its own `.climbing-badge` (same `--amber` role `.chip.live`
    already uses for a container mid-start/stop, outlined rather than
    filled so the two never look identical). S9 also flagged that a failed
    `/api/ladder-rung-plan` showed a bare "—" with no way to tell "still
    computing" from "this genuinely failed" — sharper now that S10 gave the
    endpoint a real way to fail (an unresolved companion path). The rung
    row keeps the whole `Result` instead of `.ok()`-discarding it, and the
    footprint/MTP cells show the error's own message as the tooltip on a
    red (`.status-err`, the same class the traffic log's status column
    already uses) dash instead of the same dim dash a merely-unresolved
    fetch shows — checked live (a typo'd `mmproj_path` while editing) rather
    than by a test, since it needs the resource actually resolving to an
    `Err`.

Third pass (review), taken during implementation under the same
standing permission — T1–T6, 0 blocker/major:

80. **`local_model_test` checks `hold.rung()` after `hold.sync()`, not just
    `Climbed::Done` (T1).** A per-rung `climb` returning `Done` does not
    always mean "reached rung k": a live request's own climb can join first
    (`Marked::Joined`/`Stale`) and settle on whatever rung *it* needed,
    including one above k, while the test's own mark was still unclaimed and
    got raised out from under it. Crediting the resulting probe to k would
    let a broken rung k pass as tested — exactly the WP0 instrument the
    design leans on. The fix compares `hold.rung().map(|r| r.index)` against
    `k` once `sync` returns: a match runs the probe as before; a mismatch
    records `"not reached: other traffic moved the model to rung m/n …"` and
    ends the run there, the same as any other failure, since nothing above
    an unreached rung was ever climbed to by this test. Pinned by driving
    the exact race directly (`vram::climb`/`spawn_climb` on two held claims,
    a `chat_delay` on the base's probe to buy the window to take them before
    the test's own mark exists) rather than trying to force it through the
    tool's own opaque call.
81. **The docs now say the closing stop is not forced, and what
    `reset_to_base: false` means (T2).** Neither stop ever kills a request —
    that was already true — but the tool description and the README both
    read as if the model always ends stopped. An idle claim (a tool loop or
    an agent run between turns) is "busy" exactly like a real request, so it
    can refuse the closing stop too, leaving the model up on the last rung
    reached; both texts now say so, name `reset_to_base: false` as the
    signal, and say real traffic on the model climbs along with the test
    rather than being dropped. The busy refusal's own wording changed from
    "still serving N request(s)" to "N claim(s) open (requests, or tool
    loops/agent runs sitting between turns)", since an idle claim is not a
    request and the old wording implied active generation. Contract change:
    the refusal message's wording (tests that matched on it are updated);
    the description text. New test: an idle claim taken while the top
    rung's own probe is in flight makes the closing stop fail and asserts
    `reset_to_base: false` with the model still resident.
82. **An empty `ladder` string means unchanged, like every other optional
    field (T3).** `hoist_ladder_arg` used to decode `""` into `Value::Array
    (vec![])`, which `local_model_set`'s update path then saved as "not a
    ladder" — silently, since `Some(vec![])` is indistinguishable from a
    deliberate clear once past the MCP layer. Every other optional string
    here goes through `ops::opt`, where blank means "leave unchanged"
    (`clear` is the explicit door), and an agent that fills every optional
    argument with `""` is a common, not a hostile, calling pattern. The fix
    removes the key entirely when the trimmed string is empty, so
    `patch_from_args` sees it as absent. Clearing stays `clear: "ladder"` or
    an explicit `"[]"`. The description's "Empty, '[]' or omitted means not
    a ladder" was simply wrong on *omitted* (an update keeps it) and is
    reworded. Contract change: an update with `ladder: ""` used to clear the
    ladder; it is now a no-op on that field, matching every neighbouring
    argument.
83. **`local_model_get`'s and the per-rung test's per-slot context and
    switchover are capped at each rung's own trained context (T4).**
    `LocalModel::per_slot_ctx`/`switchover` return the *configured* number;
    the gate judges (and publishes) a running rung on `ladder::slot_ctx`
    capped by `gate::ladder::trained_contexts` (§4.3 rule 3b, entry 57) — a
    row saved since rule 3b never differs, but an older row (or a
    hand-edited database) can still carry a rung above its own GGUF's
    trained context, and both readers showed the uncapped number. Both
    `ops::ladder_rungs_detail` (now `async`, one `trained_contexts` read per
    call) and `modelinfo::rung_facts` cap the same way `target_rung` does.
    New test (`ladder_models.rs`): a row written straight through the store
    (bypassing `validate_ladder`, the way a stale row would have gotten
    there) with a rung's `ctx_size` above its GGUF's trained context shows
    the capped number, not the configured one.
84. **The documented refusal lists now name every §4.3 rule, not a subset
    (T5).** The README and the `ladder` MCP description already named rules
    1, 2, 3, 3b, 5 and 7 but dropped rule 4 (a rung's per-slot context has to
    exceed `n_predict`, room for at least one prompt token), rule 6
    (`draft-mtp` needs MTP layers in every rung or a drafter) and the
    projector embedding-width check (entry 65, folded into rule 5's
    architecture/tokenizer match). A refusal already names itself at save
    time; this only matters to someone planning a ladder from the docs
    rather than by trial and error. No code change.
85. **README wording: a refusal past the top rung is not instant (T6).**
    "Refused at once" undersold entry 7's overlap: the send toward the
    running rung is already under way — a partial prefill — by the time the
    exact count's `context_length_exceeded` verdict comes back, even though
    it is fast (0.10 s in the live check) and nothing is ever streamed back.
    Reworded to "refused before any answer … even though the send toward the
    running rung is already under way (a partial prefill)". No code change.
