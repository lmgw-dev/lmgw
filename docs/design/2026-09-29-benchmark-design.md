# Benchmark for models and llama-server builds — design (2026-09-29)

**Status:** Implemented and merged to main on 2026-09-29 (WP1–WP6, two
review rounds, live-verified on the RTX 4090). Decisions are in §13; 25, 44,
48 and 57 are approved, the rest are implementation decisions.

Builds on [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
(how a row becomes a container), [2026-09-04-gpu-hold-design.md](2026-09-04-gpu-hold-design.md)
(the card is the owner's; a run must yield to the hold),
[2026-09-26-container-builds-design.md](2026-09-26-container-builds-design.md)
(images and their provenance labels) and
[2026-09-27-ladder-models-design.md](2026-09-27-ladder-models-design.md)
(whose WP0 measurements this automates).

## 1. Summary

A **benchmark run** starts one local chat model, as its row is configured
(plus optional overrides), in a container of its own, with the whole GPU to
itself. It drives llama-server's own HTTP API through a fixed, versioned
**suite**, and stores everything it measured. Runs can then be set side by side,
and each run is compared with the previous comparable run.

A run measures:

- **load**: time from `podman run` to a healthy server, and the VRAM it took;
- **prefill** throughput and time to first token, at several prompt lengths;
- **decode** throughput at several context depths;
- **concurrent decode**: aggregate and per-stream throughput with N streams;
- **prefill under decode**: what a long prompt arriving costs streams that are
  already decoding;
- **VRAM, power and energy** throughout, so every point also has tokens per
  joule;
- **behaviour probes**: does chat work, does thinking toggle, do tool calls and
  JSON schemas parse, does the template keep reasoning history, does vision
  answer, does the model find a needle at its full context.

It answers:

- which quant or which build to use, on numbers instead of impressions;
- whether a llama-server update made a model slower, or broke a behaviour lmgw
  relies on;
- the per-rung load time and VRAM of a ladder model;
- how much energy a model spends per token.

**Core, not an extension.** Its results decide core configuration (rows,
images, rungs), and it needs admission-level control of the card.

**Not in this design:** `llama-bench` inside the image (§11), quality evals
beyond the behaviour probes, benchmarking cloud aliases, aux/audio/image
classes.

## 2. Facts this rests on (checked 2026-09-29)

### 2.1 llama-server

Probed with throwaway containers, Qwen3.5-0.8B on
`localhost/lmgw-llama-server:official-master` (llama.cpp `b11226-0c6a6a7`) and
gemma-4-E4B on `localhost/lmgw-llama-server:ik-main` (ik_llama.cpp `7ff619c`).
WP1 re-checked every row with Qwen3.5-0.8B on **both** images (CPU, `-c 8192
--parallel 2`, and `--jinja` where noted), and ran the whole engine against
both:

| | official | ik_llama |
|---|---|---|
| `GET /props` `build_info` | `"b11226-0c6a6a7"` | absent (build only in the log) |
| `GET /props` `model_ftype` | `"Q4_K - Medium"` | absent |
| `GET /props` `total_slots` | top-level | top-level |
| `GET /props` `n_ctx` | per slot, under `default_generation_settings` only (4096) | per slot under `default_generation_settings` (4096), **and** a top-level `n_ctx` that is the **whole** context (8192) |
| `GET /props` `chat_template_caps` | booleans (`supports_tools`, …) | `{}`, even with `--jinja` |
| `POST /completion` with `prompt` as a token-id array | works: same `prompt_n` as the text | works |
| streamed `/completion` token chunks | `"tokens": [id]` on every chunk | no `tokens` field, one chunk per token |
| `timings` in the streamed final (`"stop": true`) chunk | yes: `cache_n, prompt_n, prompt_ms, prompt_per_second, predicted_n, predicted_ms, predicted_per_second` | yes: the same minus `cache_n`, plus `n_ctx, n_past` |
| `cache_prompt: true`, same prompt again | reused: `cache_n` 296, `prompt_n` 4 (of 300) | reused: `prompt_n` 5 |
| `ignore_eos: true` | honoured (32 of 32 predicted) | honoured |
| `POST /tokenize` | works | works, same ids |
| `POST /detokenize` | works (round-trips the text) | works |
| `POST /apply-template` | works | 200 with `--jinja` (Qwen3.5 and gemma-4-E4B); without it gemma-4's template answers **500** "this custom template is not supported, try using --jinja" (Qwen3.5's still renders) |
| `GET /slots` | per slot `id, n_ctx, speculative, is_processing` | per slot `id, n_ctx, state` and the sampling params; no `is_processing` |
| a streamed response's connection | **closed by the server** after the final chunk, although the headers say `Keep-Alive: timeout=5, max=100`; a non-streamed response keeps it (decision 62) | not probed; its source (checkout `1aaf7105`) has the same `sink.done(); return false` provider |

- The Qwen3.5 template drops an earlier assistant turn's `reasoning_content`
  (its `loop.index0 > ns.last_query_index` guard). That is exactly what the
  history probe reports (§5); it reports "dropped" on both engines.
- `/slots` `n_ctx` is the per-slot context: `-c 16384 --parallel 2` gave 8192
  per slot.
- ik needs `--jinja` for `chat_template_kwargs.enable_thinking`: without it
  the Qwen3.5 answer carries an empty `<think></think>` in `content` whatever
  the flag says, and there is never a `reasoning_content`.
- The ik image links `libcuda` directly, so even a CPU-only (`-ngl 0`) run
  needs `--device nvidia.com/gpu=all`; it then still offloads large prompt
  batches to the GPU (5 000 tok/s prefill at `-ngl 0`).
- **Prompt-cache reuse is not bit-deterministic** (checked live
  2026-09-29): gemma-4-E4B (`-c 32768 --parallel 4`, official image), the
  `deterministic` probe's request sent four times at temperature 0 with a
  seed gives two distinct answers with `cache_prompt` at its default (on),
  with and without the MTP drafter, and four identical ones with
  `cache_prompt: false`. At the time (the probe only sent the default pair),
  that row's `deterministic` failed (runs 3 and 12); Qwen3.5-0.8B passed
  either way. This is the finding behind decision 43: the probe now sends
  both pairs and reads this exact shape as `info`, not `fail`.
- After a long prompt, the next task on that slot waits for the slot's
  reset outside the prompt timer (862 ms after 250k tokens, Qwen3.5-0.8B);
  decision 41.
- The client's TTFT is 30–60 ms above `prompt_ms` on the GPU (Qwen3.5-0.8B,
  512–8190 tokens), and a hand-run non-streamed request shows the same gap:
  the server spends it outside the prompt timer. TTFT and prefill throughput
  are therefore separate numbers, as §4.3 intends.

### 2.2 Images

lmgw-built images carry their provenance as labels: `dev.lmgw.repo`,
`dev.lmgw.base` (full commit), `dev.lmgw.ref`, `dev.lmgw.slug`,
`org.opencontainers.image.version` (`b11226`) and `.revision`. A pulled image
(`ghcr.io/ggml-org/llama.cpp:server`) has only whatever OCI labels upstream set.
**A run's build identity is therefore image ref + image ID + labels, plus
`/props` `build_info` when the engine reports it.**

### 2.3 GPU

- RTX 4090, driver 615.71.09, 450 W limit, ~11 W idle.
- NVML (`nvml-wrapper` 0.12.1) is already loaded. lmgw reads memory only today.
- `nvidia-smi` has no query field for the total energy counter, but NVML has
  `nvmlDeviceGetTotalEnergyConsumption` (mJ since driver load, Volta and newer).
  When a card lacks it, energy is integrated from power samples, and the run
  records which source it used.
- **Verified by WP1 on this card** (`vram::nvml` `live_nvml_power_readings`):
  the energy counter answers and moves about every 50 ms (20 changes in a
  second of 5 ms reads); over one idle second it gained 13.0 J while
  `power_usage` read 12.8 W. Power, GPU temperature, the clock-event reasons
  (`0x1`, GPU idle, with nothing on the card; `0` once a CUDA context holds
  it) and the enforced power limit
  (450 W) all answer. One read of all five takes about 5 ms.
- An end-to-end run of Qwen3.5-0.8B (`-ngl 99 -c 16384 --parallel 2`,
  2 repetitions) agreed with a hand-run `/completion`: prefill at 2048 tokens
  35.0k tok/s (hand: 32.0k and 33.1k), decode at depth 64 524.5 tok/s (hand:
  517.3 and 522.1). Decode drew 184 W, 2.8 tokens per joule; two streams
  aggregated 806 tok/s; the 8190-token injection stalled a decoding stream
  for 54 ms.
- **Live verification, 2026-09-29** (dev instance, WP2 plus decisions
  39–42, 2 repetitions):
  - Qwen3.5-0.8B `-c 16384 --parallel 2`, official `b11226-0c6a6a7`
    against ik `b4972` (`7ff619c`): prefill at 2048 33.9k vs 23.3k tok/s,
    decode at depth 64 503 vs 367 tok/s, 2-stream aggregate 771 vs 578,
    2.86 vs 2.23 tok/J, load 1.5 vs 1.3 s, VRAM +1.63 vs +2.37 GB, stall
    31 vs 62 ms; every probe the same on both.
  - The same row as configured (`-c 262144`, one slot): prefill 34.4k at
    2048 down to 13.4k tok/s at 262 142 (TTFT 19.6 s), decode 503 tok/s at
    depth 64 down to 204 at 261 887, 3.97 GB after load; throttled (the
    power cap) in the long prefills.
  - gemma-4-E4B, the `gemma4-e4b-mm` row (`-c 512000 --parallel 4`, MTP
    drafter, projector): prefill 12.2k tok/s at 2048, decode 381 tok/s at
    depth 64 (acceptance 1.0) down to 69 at 127 743 (0.43), 4-stream
    aggregate 793, 1.66 tok/J, load 2.6 s, +13.4 GB, stall 175 ms; vision
    passes, `deterministic` fails (§2.1).

### 2.4 lmgw code

These are the seams the design reuses (scouted 2026-09-29):

- `runtime::descriptor::ModelRuntime` → `render_spec` / `preview_spec` →
  `RenderSpec` → `runtime::argv::podman_run_argv`. These are pure, in-memory
  structs, so a row plus overrides renders without being saved.
- `Registry::run_throwaway` and `modelinfo::probe_runtime` are the existing
  ephemeral-container pattern (named, `--rm --replace`, best-effort `rm -f`).
- `ops::hold::hold_set` → `lifecycle::hold_sweep`. Every start path re-checks
  `snap.settings.hold.active` (`vram/queue.rs`, `vram/climb.rs`,
  `vram/background.rs`). Request-level rerouting to a row's hold fallback
  happens in `Snapshot::resolve_for_request`.
- `jobs`: `JobKind` + `JobExecutor` + `jobs::spawn` give dedup on `(kind, key)`,
  progress on the live feed, cooperative cancel, and a durable row.
- `ir::Timings` and `egress::openai::parse_timings` already parse llama-server
  timings.
- `gate::count` already talks to a container's native root (`/apply-template`,
  `/tokenize`) with `state.http`.
- `vram/peak.rs` has the only periodic NVML sampler (image class).
- `modelinfo::local_model_test` classifies load failures from the log tail
  (OOM, unknown architecture, clip load). The runner reuses that classifier
  (WP2 factored it out as `modelinfo::load_failure_hint`).

## 3. Semantics

### 3.1 A run

- A run targets one **local chat row** (class chat), plus optional
  **overrides** (§3.5), plus a **rung** for a ladder row (default: base rung).
- At most **one run at a time**. The job key is `gpu`, so a second start is
  refused while one is running, with the running run's id.
- A run is a `JobKind::Benchmark` job with a durable `bench_runs` row (§7),
  created when the run starts.

### 3.2 The card is the benchmark's

A benchmark needs the whole card. On start, in this order:

1. **Refuse under the hold**, with the hold's own message. Hold means lmgw uses
   no VRAM at all, and a benchmark is lmgw using VRAM.
2. **Take the GPU lease.** From then on, no local model of any class (chat, aux,
   audio, image) is admitted, until the run ends. Requests for them are
   treated like requests under the hold:
   - the row's hold fallback answers when one is configured, with
     `x-lmgw-fallback` and `fallback_reason = "benchmark"`;
   - otherwise the request is refused with **503, code `gpu_benchmark`**, whose
     message names the run and says it ends when the run finishes or is
     canceled.

   The lease is checked at the same places as the hold: the request-level
   reroute, and the admission net in `vram/queue.rs`, climb and background.
   That way no path can start a container the lease forbids. WP2 put both
   behind one predicate, `Snapshot::gpu_block()` (decision 23), and every
   site that refused under the hold now refuses under the lease too:
   the candidate-alias pick and walk, `local_model_test` (every class), the
   runtime probe of `local_model_plan`, quickdoc's batch runners and pinned
   embedder, a build's GPU verify, and `lmgw__container start/restart/apply`
   (decision 25).
3. **Empty the card.** Every lmgw container on the card is stopped, all classes:
   - an idle one right away;
   - a busy one after its in-flight requests drain. No new ones arrive,
     because of the lease.

   The stage reads "waiting for X to finish its request". Nothing is killed
   mid-request, as with the hold's draining. The owner can cancel the run
   instead.
4. **Measure the baseline**: device VRAM used, outside lmgw, and idle power
   over 2 s.
5. **Start the bench container** (§3.4) and time it to healthy (the **load**
   phase).

The confirmation dialog shows everything step 3 will stop (confirm, then do).
`bench_plan` returns the same list for MCP callers.

### 3.3 Ending

The run ends when it completes, fails, or is canceled. In every case lmgw:

- stops and removes the bench container;
- releases the lease;
- finalises the row with the points measured so far. Partial results are kept
  and marked incomplete.

It does not restart the models step 3 stopped. They load again on demand, as
after the hold.

- **The hold switching on mid-run aborts the run.** `hold_set` cancels the
  benchmark and force-stops its container itself, because the bench container
  is not a registry entry that `hold_sweep` would see. Status `aborted`, reason
  `hold`.
- **Cancel** is the job's cooperative cancel. In-flight HTTP requests to the
  bench container are dropped, so a long prefill doesn't delay the cancel.
  Status `canceled`.
- **Boot:**
  - a `running` row left behind by a crash becomes `interrupted`;
  - leftover bench containers (label `lmgw.bench`, this instance's
    prefix) are force-removed before admission starts.

### 3.4 The bench container

- It is rendered from the row through `ModelRuntime` with the overrides
  applied. It uses the same argv renderer, so a run measures what the row
  would really run.
- Name: `<prefix>-bench-<run id>`. Labels: `lmgw.instance=<prefix>` and
  `lmgw.bench=<run id>`.
- Host port: a free port, found by binding loopback. It is published the way
  every model container's is, by the shared argv renderer: on loopback only
  (`-p 127.0.0.1:<port>:8080`). Until 2026-09-29 every model container was
  published on all interfaces (`0.0.0.0:35535->8080/tcp`, seen live), which
  put an unauthenticated llama-server on the LAN; it was confirmed that nothing
  reaches a model's port except through lmgw, so the renderer now binds
  loopback for every class.
- It is not a registry entry. The runner owns its lifecycle.
- VRAM attribution counts its processes as lmgw's, so Overview shows a run as
  lmgw's share and outside use stays measured (decision 40). Once loaded, the
  container gets a generation of its own from the registry's counter and joins
  the attributed containers; while it loads, the share is unavailable, as for
  a `starting` entry; once removed, a process the driver still lists is a
  tombstone. The `vram` frame carries `benchmark: {run_id, model_id}`.
- The command line is stored on the run.
- The run reads the **real** slot count and per-slot context back from the
  live server (`/props`, `/slots`), and derives every point from those, not
  from the row's numbers. This matters because `fit`, `-np` auto and
  kv-unified can all differ from what the row says.

### 3.5 Overrides

Overrides are flat, so they fit an MCP tool's scalar arguments. They change the
bench container only, never the row:

- `image`;
- `ctx_size`, `parallel`, `ubatch_size`, `batch_size`;
- `cache_type_k`, `cache_type_v`;
- `flash_attn`, `kv_unified`, `n_gpu_layers`;
- `no_draft`: run without the row's speculative drafter;
- `rung`: a ladder row's rung, `0` = base.

Comparing two builds is then two runs of the same row with different `image`
overrides. Comparing two quants is two rows, or a ladder's rungs.

## 4. The suite (v1)

`SUITE_VERSION = 1`. Changing any constant or method below bumps it. Runs of
different suite versions are shown but never compared (§6).

### 4.1 Corpus

- **The corpus is a frozen text asset**, `crates/lmgw-core/assets/bench/corpus-v1.txt`:
  a concatenation of lmgw's own design specs as of 2026-09-29. It is English
  prose, tables and code, which is representative of the traffic this gateway
  sees, and the text is the project's own. It is compiled in. It is never
  regenerated, because a new corpus is a new suite version.
- The run tokenizes it once through the bench server's `/tokenize`. Prompts are
  **token-id arrays** sliced from that sequence (wrapping around when a length
  exceeds it). A prompt of *P* tokens is therefore exactly *P* tokens on every
  engine and every model.
- The corpus is tokenized as text: the chat-template markers the specs quote
  never become control tokens, and every prompt starts with the
  vocabulary's BOS, counted inside *P* (decision 58).
- Repetitions of a point start at different offsets, so no two repetitions
  share a prefix.

### 4.2 Points

Let *S* be the per-slot context and *N_slots* the slot count, both read from
the live server.

| Phase | Points | Per request |
|---|---|---|
| prefill | *P* = 512·4ᵏ while *P* < *S* − 1, plus *P_max* = *S* − 2 | `/completion`, `n_predict: 1`, `cache_prompt: false`, streamed |
| decode | depth *D* = 64, then 1024·4ᵏ while *D* + *G* < *S*, plus *D_max* = *S* − *G* − 1 | `/completion`, `n_predict: G`, `ignore_eos`, `cache_prompt: true` |
| concurrent | *N* = 1, 2, 4, … < *N_slots*, plus *N_slots* | *N* simultaneous streams, prompt 256, `n_predict: G`, `ignore_eos` |
| mixed | needs *N_slots* ≥ 2: *N_slots* − 1 decoding streams, plus one injected prompt of *P_inj* = min(8192, *P_max*) — on a shared KV pool, what fits beside the streams (decision 57) | streamed |

- *G* = 256 generated tokens.
- **Repetitions:** default 3, set per run and stored with it. Each point
  stores **median, min and max**.
- **Sampling is pinned for every request:** temperature 0.7, top_p 0.9,
  top_k 40, seed 1234. It matters for speculative decoding, whose speed depends
  on draft acceptance.
- **The top points come from the real context**, so no lengths are capped
  (the no-hidden-limits rule). A long-context row costs a long prefill.
  The plan shows every point before the run starts, and the owner can drop a
  phase.

### 4.3 Methods

- **Prefill.**
  - Server-side: `timings.prompt_ms` and `prompt_per_second`.
  - Client-side: **TTFT**, from sending the request to the first streamed
    token, on the same request. Every streamed request opens its own
    connection (decision 62); a measured one effectively always did, since
    each follows another stream.
  - `cache_prompt: false` and distinct offsets keep the KV cache out of it.
  - Every prefill, concurrent and mixed repetition, and the first
    repetition at every decode depth, is preceded by an unmeasured slot
    reset (decisions 41 and 61).
- **Decode.**
  - `timings.predicted_per_second`.
  - The first repetition at a depth also primes the cache. The later ones
    reuse it (`cache_prompt: true`), which saves re-prefilling the depth
    twice, for energy's sake.
  - Draft acceptance (`draft_n`, `draft_n_accepted`) is stored whenever the
    server reports it.
- **Concurrent.**
  - The *N* streams are released together.
  - **Aggregate** = Σ `predicted_n` / (last stream's end − first stream's first
    token). That is decode throughput with the prefills excluded.
  - **Per-stream** = the median of each stream's `predicted_per_second`.
- **Mixed.**
  - One unmeasured request shaped like a stream reads a stream's decode and
    prefill rate first. A slot that cannot keep a stream decoding through
    the steady window and the injection at that rate, or a shared pool with
    no room for an injection beside the streams, makes the phase a planned
    skip with a note (decision 57).
  - *N_slots* − 1 streams decode with a large `n_predict` (on a shared pool:
    what each was counted with). After 2 s of steady decode, one prompt of
    *P_inj* is injected.
  - Every streamed token is timestamped on the client.
  - Stored:
    - each decoding stream's rate in the 2 s before the injection, and its rate
      from the injection to the injected request's first token;
    - the **longest inter-token gap** in that window, i.e. the stall;
    - the injected request's TTFT, next to the solo TTFT at the same length
      from the prefill phase.
  - The decoding streams are then closed. llama-server stops generating when
    the client disconnects.
- **Load.**
  - From `podman run` to `/health` 200, which includes container start.
  - VRAM after load, minus the baseline.
- **VRAM, power, temperature, throttling.**
  - A sampler runs for the whole run: 100 ms for peaks, stored at 500 ms as a
    timeline marked with phases.
  - It records device VRAM used, power (W), GPU temperature, and the OR of
    NVML's clock-event (throttle) reasons.
  - A run that throttled says so. Its numbers are then the throttled numbers.
- **Energy.**
  - The energy counter is read at each point's start and end, so each point
    gets joules, **tokens per joule**, and average power.
  - A window shorter than two sampler ticks (200 ms) is not measured: the
    point says "too short to measure" instead (decision 56).
  - Idle power (§3.2 step 4) is stored, so a view can show net energy.
  - NVML measures the whole card, the desktop's few watts included. That is
    why idle power is recorded.

### 4.4 Phase order

The order is load → probes (§5, needle last) → prefill → decode → concurrent →
mixed. One unmeasured warm-up request (decision 39) runs after load and before
the first selected phase.

The owner may deselect phases. Load always runs.

The probes run first because a build that cannot answer a chat makes every
throughput number suspect: the run keeps going, but the probe row shows red.

## 5. Behaviour probes

They run against the bench container's own `/v1/chat/completions` (and
`/apply-template`). They exercise exactly the knobs lmgw relies on.

Every probe ends in one of these outcomes:

- `pass`;
- `fail` (with evidence);
- `info` (a finding, not a verdict);
- `skipped` (with the reason, e.g. "no projector");
- `error` (the HTTP status and body, e.g. ik's 500 on `/apply-template`).

| Probe | When | Pass condition |
|---|---|---|
| `chat` | always | 200, non-empty content, `finish_reason` stop |
| `thinking_off` | reasoning-capable | `chat_template_kwargs.enable_thinking=false` → no `reasoning_content`, non-empty content |
| `thinking_on` | reasoning-capable | `enable_thinking=true` → non-empty `reasoning_content` |
| `tool_call` | template supports tools | one `get_weather` call whose arguments parse as an object with `city` |
| `json_schema` | always | `response_format: json_schema` → content parses and has the required typed fields |
| `deterministic` | always | the pair sent twice — cache reuse allowed, then `cache_prompt: false` — all four identical → `pass`; the no-cache pair identical but the cached pair differing → `info`; the no-cache pair differing → `fail` (decision 43) |
| `vision` | row has a projector | a generated solid-red 64×64 PNG, "what colour, one word" → names red as a whole word, not right after a negation (decision 54) |
| `reasoning_history` | always | `info`: whether an earlier assistant turn's `reasoning_content` marker survives `/apply-template` |
| `needle` | always | a passcode at the start of *P_max* − 512 tokens of corpus → the answer contains it |

- **Capabilities.** "Reasoning-capable" and "supports tools" come from the
  row's derived capabilities. The live `/props` `chat_template_caps` is used
  where the engine reports it.
- **Thinking during other probes.** It is switched off
  (`enable_thinking=false`) for every probe except `thinking_on`, so answers
  stay short.
- **Evidence.** Each probe stores the response content, its reasoning (if
  any), and its status.

## 6. Identity and comparison

Each run stores:

- **model**: row id, `gguf_path`, file size and mtime, quant (from the GGUF
  header's file type, which works on every engine), and the rung;
- **build**: image ref, image ID, engine slug, repo, commit, version (from the
  labels, §2.2), and `/props` `build_info` when present;
- **settings**: the effective `LlamaParams` + args after overrides, plus a
  **settings hash** over them. Port and container name are excluded;
- **gpu**: name, driver, VRAM total, power limit, temperature at start and end,
  and whether it throttled;
- **suite**: version, repetitions, phases, *G*, and the corpus token count.

**Comparable runs** have the same model file (path + size), settings hash,
suite version and GPU name. The build may differ, since that is the point.

**Previous comparable run**: the latest `done` run with that key before this
one. `bench_run` returns the comparison against it.

**Regression rule**, per headline metric:

- *delta* = (new − old) / old;
- *noise* = the larger relative spread ((max − min) / median) of the two
  points;
- the metric is a **regression** when *delta* < −max(threshold, noise), and an
  **improvement** when *delta* > +max(threshold, noise); for tokens per joule
  the noise is the spread of its windows' own figures (decision 42);
- the threshold defaults to **5 %** and is a visible, per-view argument
  (`threshold_pct`) that is echoed in the answer, never a hidden constant.
  "Lower is better" metrics (TTFT, load, VRAM, stall) flip the sign.
- only the same point is judged: when the two runs measured a headline at
  different points (a run that stopped early), its delta is shown as
  information with the verdict `not_same_point`, which counts for neither
  side (decision 53).

**Headline metrics:**

- prefill tok/s at 2048 (or the nearest point);
- TTFT at 2048;
- decode tok/s at depth 64 and at *D_max*;
- aggregate tok/s at *N_slots*;
- decode tokens per joule at depth 64;
- load time;
- VRAM after load;
- the mixed stall.

**Probes** compare as outcome changes: a pass that turns into a fail is flagged
like a regression.

## 7. Storage

Migration `0045_benchmarks.sql`: one table, `bench_runs`.

| Column | |
|---|---|
| `id`, `job_id`, `created_at`, `finished_at` | |
| `status` | `running`, `done`, `failed`, `canceled`, `aborted`, `interrupted` |
| `status_reason`, `error` | |
| `model_id`, `gguf_path`, `gguf_size`, `gguf_mtime`, `quant`, `rung` | `gguf_mtime` added by WP2, so §6's model identity reads back whole |
| `image_ref`, `image_id` | |
| `build` | JSON, §6 |
| `settings` | JSON: effective params + overrides |
| `settings_hash`, `command_line` | |
| `gpu` | JSON |
| `suite_version`, `params` | `params` is JSON: repetitions, phases, *G*, sampling |
| `results` | JSON: points per phase, energy, VRAM, load, phase errors, requests sent again (decision 62) |
| `probes` | JSON |
| `timeline` | JSON: samples at 500 ms |
| `notes` | the owner's free text |

- The JSON shapes are typed DTOs in `lmgw-api-types::bench`, so the UI and the
  store cannot disagree about them.
- A run is written once at start, updated after every phase (a crash keeps the
  phases done so far), and finalised at the end.
- Nothing prunes runs. They are deleted only by hand. A run is tens of KB.

## 8. Surfaces

### 8.1 Ops

All ops are on `/api/op/{name}`, `Cap::Admin`. DTOs are in
`lmgw-api-types::bench`.

| Op | Writes | Does |
|---|---|---|
| `bench_plan` | no | For a row + overrides + phases + repetitions, it returns: the effective settings; the rendered command line; the build identity; the provisional points (from the row's numbers, labelled provisional); the applicable probes; everything that would be stopped (and whether it is busy); and `blocked` with the reason (hold on, a run already going, row missing, disabled or not chat, rung out of range, the image not on this machine, no weights) |
| `bench_start` | yes | Starts a run as in §3.2, with the same arguments plus `notes`. Answers `{run_id, job_id}` |
| `bench_runs` | no | Run summaries with headline numbers, newest first, filterable by `model_id` |
| `bench_run` | no | One whole run, plus the comparison with its previous comparable run (`threshold_pct` optional) |
| `bench_cancel` | yes | Cancels the running run (`run_id` optional; refused when it names another) |
| `bench_run_set` | yes | Edits a run's notes |
| `bench_delete` | yes | Deletes a finished run |

- MCP tools: `lmgw__bench_plan`, `lmgw__bench_start`, `lmgw__bench_runs`,
  `lmgw__bench_run`, `lmgw__bench_cancel`, `lmgw__bench_delete`.
  - They have flat arguments, since the overrides are flat (§3.5).
    `phases` is a comma-separated string.
  - `bench_run_set` is dashboard-only.
- The new error code `gpu_benchmark` goes wherever `gpu_hold` is documented:
  the OpenAPI error codes and the README's hold section.
- **As built (WP2).** The DTOs are in `lmgw-api-types::bench_ops` (`bench`
  was full): `bench_plan` and `bench_start` both take `BenchArgs` (the flat
  overrides, `phases` as an array, `repetitions`, `notes` — ignored by the
  plan) and answer `BenchPlan` / `BenchStarted`; `bench_runs` takes
  `BenchRunsArgs` → `BenchRunsResponse`; `bench_run` takes `BenchRunArgs`
  (`timeline` defaults to true on the op, false on the tool) →
  `BenchRunDetail`; `bench_cancel`, `bench_run_set`, `bench_delete` answer
  `BenchDone`. The job kind is `benchmark`, key `gpu`; its `detail` is
  `BenchJobDetail {run_id, model_id}`. `phases` differs between tool and op,
  which `openapi/ops/divergence.rs` records.

### 8.2 Dashboard

A new page, **Benchmarks** (`/benchmarks`), in the **Serve** group after
Backends. It sits there because it serves choosing rows, images and rungs.

- **Runs**:
  - a table: model, quant, build, date, headline numbers, probes as n/m
    passed, a regression badge against the previous comparable run, and
    status;
  - a running run shows its stage and progress live from the jobs feed.
- **New benchmark** (a modal):
  - pick a row (and rung) and an image (the class default or any llama
    image from the Backends list);
  - the overrides; the phases; repetitions; notes;
  - the plan preview: points, probes, command line;
  - the confirmation lists what will be stopped.
- **Run detail** (`?run=<id>`):
  - identity (model, quant, build, GPU, settings, command line);
  - the probes, with their evidence;
  - one chart per phase: prefill tok/s and TTFT against prompt length (log
    x-axis); decode tok/s against depth; aggregate and per-stream against *N*;
    mixed as before/during bars plus the stall;
  - tokens per joule per phase;
  - a timeline of power, VRAM and temperature, with phase bands;
  - the comparison with the previous comparable run: a per-metric delta table
    with the threshold control.
- **Compare** (`?compare=<id>,<id>,…`): the selected runs' curves overlaid (one
  colour per run, `slot_color`), a delta table, and the probes side by side.
  When the runs are not comparable (§6), it says why, but still overlays them.
  Comparing two quants is exactly that case.
- **Entry points elsewhere:**
  - a Models row menu item **Benchmark** (the modal, with the row
    preselected);
  - on Backends, an image's **Benchmark with this image** (the modal, with the
    image preselected).

## 9. Testing

- **Pure functions, unit-tested:**
  - point derivation (§4.2) from *S* and *N_slots*, including tiny contexts
    and one slot;
  - the aggregate/per-stream arithmetic;
  - the mixed-phase windows and stall over synthetic timestamps;
  - median/min/max;
  - the regression rule, including the noise band and lower-is-better
    metrics;
  - the settings hash (stable across port and name);
  - the probe verdicts over canned responses.
- **Runner against a fake server** (`tests/it/bench.rs`, wiremock): `/health`,
  `/props`, `/slots`, `/tokenize`, `/detokenize`, a streamed `/completion` with
  timings in the last chunk, `/v1/chat/completions`, and `/apply-template`
  (both as 200 and as ik's 500). A launcher seam (a trait, like
  `CommandRunner`) lets tests start "the container" as that server. The GPU
  comes from a fake `GpuProbe` with a scripted energy counter. The engine
  suite's own fake (`tests/it/support/llama_fake.rs`, axum) streams token by
  token and can hang up on the n-th `/completion` before any response
  (decision 62).
- **Lease and admission:**
  - under a running run, a chat request gets 503 `gpu_benchmark`, or the hold
    fallback when one is configured;
  - an aux request is refused likewise;
  - admission's net refuses too;
  - the hold switching on aborts the run;
  - boot turns `running` into `interrupted`.
- **Plumbing guards:** `op_names` count, dispatcher arms, `OpDoc` rows, and
  tool/op argument agreement all pass.
- **Live, on a dev instance** (`scripts/dev-instance.sh`):
  - Qwen3.5-0.8B and gemma-4-E4B;
  - official-master against ik-main;
  - probes checked against §2.1, and the numbers checked for sanity against a
    hand-run `/completion`.

## 10. Work packages (sequential)

1. **Engine.** Module `bench/`:
   - corpus, points, the llama client (props, slots, tokenize, detokenize,
     streamed completion with timestamps, chat, apply-template);
   - the phases and probes against a `BenchTarget { base_url }`;
   - the sampler, with NVML power/energy/temperature/throttle added behind
     `GpuProbe` (energy counter, with integration as fallback);
   - the result DTOs in `lmgw-api-types::bench`;
   - unit and wiremock tests;
   - verifying on this card: the energy counter, token-array prompts,
     `/detokenize`, and timings in the streamed last chunk, on both engines
     (CPU throwaway containers are enough for the API facts).
2. **Integration:**
   - the launcher (render + podman + health + log-tail classification);
   - the lease and its checks (§3.2), the `gpu_benchmark` error, the hold abort,
     the boot sweep;
   - `JobKind::Benchmark`;
   - migration 0045 and `store/bench.rs`;
   - the comparison and regression rule;
   - the seven ops, `op_names`, the dispatcher arms, `OpDoc`s, and the MCP
     tools;
   - `tests/it/bench.rs`.
3. **Dashboard:** the Benchmarks page (§8.2), the nav item, and the two entry
   points.
4. **Docs:** a README section, `docs/release-notes.md`, and this spec's status.
5. **Live verification** on a dev instance, and fixes.
6. **Review:** an adversarial pass over the whole branch, then fixes, then merge.

## 11. Later

- **`llama-bench`** inside the run's image, for raw pp/tg without server
  scheduling. It would be a second engine behind the same result shape.
- **Auto-benchmark after a build:** when a Backends build finishes, run the
  suite on a chosen reference row and flag a regression on the build's card.
- **All rungs in one go** for a ladder row, as a batch of runs.
- **Request replay as a quality suite** (see `docs/ideas.md`, "Request
  inspector and replay"): quality numbers next to the speed numbers.
- Multi-GPU: per-device energy and VRAM.

## 12. Out of scope

- Cloud aliases, since their speed is the provider's.
- Aux, audio and image classes.
- Changing a row from a run's result. The owner decides and edits the row.

## 13. Implementation decisions (for review)

Taken without owner input. Each can be reverted on its own.

1. **Other local traffic during a run is treated like traffic under the hold**
   (hold fallback, else 503 `gpu_benchmark`), rather than queueing. A full run
   of a 27B at 131k context takes minutes, and queued clients would time out
   anyway.
2. **The whole card, all classes.** Embeddings and audio are stopped too.
   Anything left on the card skews both VRAM and power.
3. **Busy containers drain; nothing is killed at start.** This mirrors the
   hold's draining.
4. **The corpus is lmgw's own specs, frozen.** It is the project's own text,
   representative of the traffic, and has no licence question.
5. **Pinned sampling** (0.7 / 0.9 / 40 / seed 1234) rather than the row's
   sampler, so builds compare. Decode speed hardly depends on sampling,
   except through draft acceptance.
6. **Probes run before the throughput phases**, and a failing probe does not
   stop the run.
7. **Regression threshold 5 %**, widened by the measured noise, as a visible
   argument rather than a setting.
8. **The page sits in Serve, not Observe.** It is a tool for choosing
   configuration, not a view of past traffic.
9. **The stopped models are not restarted after a run**, the same as after a
   hold. They load again on the next request.
10. **Decode repetitions share one prompt.** The corpus offset is drawn per
    depth, not per repetition, because §4.3's cache reuse needs the same
    prompt. §4.1's distinct offsets hold for prefill, concurrent and mixed.
11. **Energy is measured over windows, not whole points.** Prefill: each
    request's TTFT window. Decode: first token to last token, so the priming
    prefill stays out. Concurrent: the aggregate's window. The counter is
    interpolated from the 100 ms series at both ends of a window. The probes
    and the mixed phase get no energy figure.
12. **A failed phase keeps its points, and the run goes on** to the next phase
    while `/health` still answers. The run then ends `failed` with the first
    error. When the server stopped answering, the run stops there.
13. **Mixed: the stall is measured whole.** After the injected first token,
    the engine waits for every decoding stream's next token (or its end)
    before closing them. A gap that overlaps the window counts at its full
    length, and the "during" rate runs from the injection's send.
14. **Reasoning inside content is judged.** `thinking_off` fails on a
    non-empty `<think>` block in `content`; `thinking_on` names that case when
    it fails. An empty `<think></think>` (ik without a parser) is harmless.
15. **Probe answer budgets** are 128 tokens, 1024 for `thinking_on` and 256
    for `tool_call`/`json_schema`, all at temperature 0 with the suite seed.
    A capability the row does not state (`None`) runs its probe instead of
    skipping it.
16. **"Throttled" means a limiting reason**: power cap, hardware slowdown,
    software or hardware thermal slowdown, power brake. GPU idle, application
    clocks, sync boost and display clock are kept in the stored bitmask but do
    not count.
17. **Several GPUs are summed.** The sampler adds VRAM, power and energy over
    every device, takes the hottest temperature and ORs the clock events. A
    device that lacks a reading makes that sum unknown, not smaller.
18. ***S* comes from `/slots` first** (the smallest slot), then
    `default_generation_settings.n_ctx`, then top-level `n_ctx / total_slots`
    — ik's top-level `n_ctx` is the whole context (§2.1).
19. **The stored timeline** holds, per 500 ms bucket, the maximum VRAM and
    temperature, the mean power and the OR of the clock events.
20. **The suite constants are stored in `params`** (*G*, stream prompt,
    injection cap, steady window, needle margin), so a run describes itself.
    The engine reads them from there; only suite v1's values are used
    outside tests.
21. **The engine's fake server is an axum app, not wiremock**
    (`tests/it/support/llama_fake.rs`, suite `tests/it/bench_engine.rs`).
    wiremock sends a body at once, and the phases measure the time between
    streamed tokens. WP2's runner suite keeps the name `tests/it/bench.rs`.
22. **The settings hash covers the llama-server flags only**: the effective
    `LlamaParams`, the freeform `args` and the `podman run` args. The image
    is left out (it is the build identity, and comparable runs may differ in
    their build — that is what comparing builds is), the weights file is the
    model identity, and the overrides are left out because the same effective
    flags reached with or without an override are the same settings.
23. **The lease is runtime state carried on the `Snapshot`** (`gpu_lease`),
    read through one predicate, `Snapshot::gpu_block()` (hold or lease; the
    hold wins). Every admission site already holds a snapshot and already
    asked it about the hold, and `resolve_for_request` is a pure snapshot
    method. A publish from the store carries the lease over, and setting it
    is a publish too — both through `ArcSwap::rcu`, so a racing reload can
    neither drop nor revive it. A guard releases it on every way out of a
    run, a panic included.
24. **`gpu_benchmark` carries the hold's `detail`** (why a configured
    fallback did not help), and the fallback reason is `benchmark`
    (`x-lmgw-fallback-reason`, `request_logs.fallback_reason`).
25. **The lease reaches further than §3.2 names**, wherever the hold already
    did: the candidate-alias pick and walk, `local_model_test` of every
    class, `local_model_plan`'s runtime probe (deferred), quickdoc's batch
    runners and pinned embedder, a build's GPU verify (left unverified), and
    `lmgw__container start/restart/apply` (refused before anything stops).
    A request that was already **queued for room** when the lease (or the
    hold) came on is refused on its next pass instead of starting a
    container once room appears — before WP2 that queue never looked at the
    hold again. *(The queued-request change: approved 2026-09-29.)* WP2 also switched the outside-VRAM verdict off while a
    lease was held; that was rejected, and decision 40 replaces it.
26. **The drain waits for decided starts too**: a start admission decided just
    before the lease (a reservation not yet in the registry) is waited out
    like a busy container.
27. **A container that will not stop fails the run** (`failed`, naming it):
    it is still on the card, and every number beside it would be wrong.
28. **The image must already be on the machine** (`blocked: image_missing`):
    `podman run` would pull it inside the load phase and time the download.
    `no_weights` (models dir unset, file unreadable) is the other reason §8.1
    did not list.
29. **`no_draft`** clears the typed drafter fields (`draft_gguf_path`,
    `spec_type`, `spec_draft_*`) and drops `--model-draft`/`-md`/
    `--spec-type` from the freeform args.
30. **Probe capabilities from the row**: reasoning is unknown without derived
    reasoning caps and false only for a `fixed` kind that is disabled; tools
    are false only for tool kind `none`; the projector is `mmproj_path` (not
    `no_mmproj`) or derived `vision: true`.
31. **The run rewrites its identity at start**: `bench_start` stores the plan's
    identity with an empty command line, and the job, once it has its real
    port, stores what it renders then (so a row edited in between is recorded
    as run). `/props` `build_info` is added to the build once the server
    answers.
32. **The bench container is removed with `podman rm -f`**, on every end:
    the run is over, and a load failure's log tail is read before removal.
    The container is removed **before** the lease is released, so nothing is
    admitted onto a card it still occupies.
33. **`bench_cancel` is an op of its own** (optional `run_id`), so no caller
    needs the run's job id.
34. **`lmgw__bench_run` omits the timeline samples** unless `timeline=true`
    (the op includes them): they are the bulk of a run, and an agent rarely
    needs 500 ms samples.
35. **`bench_runs` judges each run against its previous comparable run** at
    the answer's `threshold_pct`, for the table's regression badge.
36. **Status reasons**: `aborted` carries `hold` or `shutdown` (a graceful
    quit removes the bench container too — `stop_all` would not see it),
    `canceled` carries `canceled`, `interrupted` carries `shutdown`.
37. **Headline points**: prefill and TTFT at the prompt length nearest 2048,
    decode at the depth nearest 64 and at the last depth (one point is both),
    aggregate at the last *N*; a metric whose earlier value is 0 is `missing`
    rather than an infinite change. Regressions and improvements count
    metrics and probes together.
38. **Boot**: `running` rows become `interrupted` in `AppState::init`, beside
    the orphaned jobs; leftover bench containers are removed at the start of
    `lifecycle::boot`, before reconciliation, which skips any container with
    `lmgw.bench` (the sweep re-checks both labels on what podman lists, so a
    model's container can never be taken for one).
39. **One unmeasured warm-up request after load** (512 prompt tokens, 16
    generated, `cache_prompt: false`, stored in `params` as
    `warmup_prompt_tokens` / `warmup_generate_tokens`), before the first
    selected phase. Found live: with the probes deselected, prefill at 512
    was the fresh server's first request and read 12.0k and 18.7k tok/s
    against 26k warm, and `phases` is not part of the comparability key, so
    such a run would have flagged a false regression. A failed warm-up is
    only logged; the phases report their own errors. Suite v1 was not bumped:
    no run existed outside a dev instance.
40. **The bench container's processes are lmgw's share** (owner decision,
    2026-09-29, replacing WP2's "outside verdict off under the lease"). The
    run's `CurrentRun` carries where its container is (`OnCard`: off,
    loading, ready with a generation from the registry's counter); the
    ledger adds a ready one to the containers it attributes, to the
    generations a `podman inspect` answer is kept for, and to the roster a
    measurement compares before and after. A loading one makes the share
    unavailable, as a `starting` entry does. The lease no longer switches
    §4.7's trigger off; admission under the lease is unchanged.
41. **An unmeasured slot reset before every prefill, concurrent and mixed
    repetition**: one request per slot (the warm-up's shape, each from its
    own corpus offset), released together so each takes a slot of its own,
    and waited for. Found live: llama-server resets a slot's previous context
    before it starts a new task on it, outside the prompt timer — 862 ms
    between "selected slot" and "processing task" after a 250k-token prompt
    on Qwen3.5-0.8B (a throwaway container, `-c 262144 --parallel 1`). In
    run 6 that put prefill at 512, right after the needle probe, at a TTFT
    of 972 ms (41 ms on the next repetition), and every later repetition of
    a long prefill paid the previous one's reset (TTFT at 262 142: 20.2 s,
    then 20.7 s). A concurrent stream landing on a slot the deepest decode
    left would start late and stretch the aggregate's window. Decode needs
    none: its rate is the server's own, and its window starts at the first
    token. (Not on ik_llama.cpp: decision 61.) Also measured: after a
    single warm-up, the next few 512-token requests still ramp by 10–15 %
    (the card's clocks under bursts of 20 ms of work); a longer warm-up
    (256 generated) did not remove it, so it is left to the comparison's
    noise band.
42. **Tokens per joule is judged against its windows' spread.** Each point's
    energy keeps every window's own tok/J (`tokens_per_joule_each`, a
    `Stat`), and the comparison reads its (max − min) / figure as the noise,
    where before an energy figure had none and met the bare 5 % threshold.
    Found live: runs 6 and 7 (identical settings, same build) read 2.47 and
    3.01 tok/J at depth 64 — 202 W against 167 W at the same 500 tok/s —
    and the comparison called it a 22 % improvement. The half-second decode
    windows follow the idle slot reset, while the card's clocks ramp back up
    (the timeline shows 112–150 W, then 200 W+). The headline figure is
    still the whole point's tokens over joules.
43. **`deterministic` sends its pair twice, cached and uncached, rather than
    once.** Live testing (§2.1) found prompt-cache reuse (`cache_prompt`'s
    default) is not bit-deterministic on gemma rows: two identical
    temperature-0, seeded requests differ when the second reuses the first's
    KV cache, and are identical either way with `cache_prompt: false`. A
    single cached pair could not tell "this build broke determinism" from
    "this build's cache reuse recomputes a reused prefix in a different batch
    shape" — the first is a real regression, the second is expected and not
    worth a red probe. The probe now sends both: cache reuse allowed (as a
    client would send it) and `cache_prompt: false`, and judges on the
    no-cache pair first. All four identical is `pass`; the no-cache pair
    identical but the cached pair differing is `info`, naming the batch-shape
    cause, with both cached answers as evidence; the no-cache pair differing
    is `fail` — that is non-determinism no cache setting explains.
44. **The registry enforces the lease too, and the drain settles the
    admission gate before it looks** (review finding 1). Every admission site
    asked the lease when a start arrived and then awaited — a footprint, the
    gate, a ledger read — before it started the container, so a start decided
    just before the lease could reach `podman run` after the drain had found
    the card empty. Now:
    - the registry keeps a copy of the lease, set with the snapshot's under
      its map lock, and refuses to *create* an entry while one is held
      (`RuntimeError::GpuBenchmark`, answered as 503 `gpu_benchmark`). A claim
      on an entry that exists is still a join, which the drain waits for;
    - the operator/warm pre-flight and a guest's start ask again after their
      ledger read, under the gate; a request asks again right before
      `acquire`;
    - the drain takes the gate once before it looks, so every decision in
      progress finishes first.

    Only the lease is enforced in the registry; the hold still drains and
    sweeps rather than refusing there. The re-checks ask `gpu_block()`, so a
    start that the hold came on during is refused too, where before it
    started and was then swept. *(Approved 2026-09-29.)*
45. **A run waits for boot, and the boot sweep spares this process's
    containers** (review finding 2). Boot runs unawaited. Until
    reconciliation has adopted what a previous lmgw left running, those
    containers are not in the registry, and the drain would leave them on the
    card beside the run. So until `boot_settled`, `bench_plan` answers
    `blocked: booting` (a new `BlockedReason`) and `bench_start` refuses. The
    boot sweep also skips a bench container that podman created at or after
    this process started (the guard agents' `reconcile_since` has), and the
    running run's container by name. With the refusal no run can start
    before the sweep, so these two are only a second net.
46. **Once a run stops measuring, nothing from outside ends it** (review
    finding 3). The run marks itself `ending` before it removes its
    container. From then on the hold's (or the shutdown's) abort finds no
    run: `hold_set` reports no abort, and the run's own end removes the
    container moments later. A run whose suite ended `done` stays `done`,
    even if an abort or a cancel landed after its last phase had finished.
    It measured everything, so nothing was cut short.
47. **A panic in a run is caught at the run, and the run ends `failed`**
    (review finding 4). The job's task catches no panic, so before this
    change a panicking run released only the lease, through the guard's
    `Drop`. The container stayed on the card, and the row and the job stayed
    `running`, which refused every later start until a restart. The run's
    body now runs under `catch_unwind`, and the end path runs on every way
    out: remove the container, finalise the row with the panic's message,
    finish the job. Catching panics in `jobs::spawn` for every job kind would
    be broader, and is left for its own change.
48. **A bench container that will not go is retried, reported and removed
    by the next run** (review finding 6). The end of a run tries
    `podman rm -f` three times, pausing 1 s and then 2 s; the error names
    the count. If the container is still there:
    - the row's `error` names it, and the status stays what the run earned;
    - the lease is released anyway. Keeping it would refuse every local model
      until a restart;
    - the container is remembered: `bench_plan` warns about it, and the next
      run removes it after its drain and before it measures. That run fails,
      naming the container, if it still cannot (decision 27's rule);
    - the next boot's sweep collects it.

    Its VRAM never read as outside use, as the review feared. Once the run
    no longer lists the container, its processes become attribution's
    tombstones, which count as lmgw's for as long as the driver lists them.
    *(Approved 2026-09-29.)*
49. **The cancel bounds every podman call before the container is up**
    (review finding 5). The run's `podman image inspect`, its `podman run`
    and the load phase's `podman inspect` are each raced against the job's
    cancel, with no timeout of their own. podman can stall on its storage
    lock, and before this change `bench_cancel` looked dead while it did.
    The cancel is also checked again right before `podman run`, after the
    last await: a hold that aborted the run a moment earlier had nothing to
    remove, and the run would otherwise create the container anyway. A
    `podman run` dropped mid-call may or may not have created the container,
    so the end removes it by name either way. The end's own `podman rm -f`
    is not raced: the lease is released only after the container is gone
    (decision 32).
50. **A request that fails while the cancel is raised was canceled** (review
    finding 9). The hold's abort raises the job's cancel and removes the
    container at once. The request in flight breaks (a connection reset)
    before the guard's next look at the flag, which comes at most 250 ms
    later (`CANCEL_POLL`). So a failure that arrives with the cancel raised
    is read as the cancel. The client does this for every request, which
    also stops a probe in flight being judged `error`, and the suite does it
    again before it records a phase error. An aborted run therefore lists
    no phase error that the abort caused.
51. **A stored run that does not decode degrades; it no longer fails the
    op** (review finding 7).
    - Each JSON column that does not parse reads as its empty default and is
      named in the run's new `unreadable` list (on `BenchRun` and on the
      runs table's summary). `bench_runs` lists the run with what could be
      read, and `bench_run` shows it.
    - `not_comparable` gives a reason for an unreadable run, so it is
      compared with nothing, in the ops and in the Compare view alike.
      Before this change one bad row failed the whole list, and every
      comparison that reached it.
    - The previous comparable run is now one lookup: `ORDER BY id DESC
      LIMIT 1`, with the GPU name matched in SQL and guarded by
      `json_valid`. The lookup steps past an unreadable candidate. Before,
      each `done` run in a `bench_runs` page decoded every earlier
      candidate, which was quadratic.
    - `limit: 0` is refused. It used to answer an empty page with
      `more: true`.
52. **Error wording** (review finding 8).
    - The operator start's refusal under the pre-flight is passed on as its
      own sentence. That sentence names the hold or the benchmark run, and
      how each ends. Under a lease the old "GPU hold is active —" prefix
      was wrong.
    - The `image` override is refused when it starts with `-` (podman would
      read it as a flag) or holds whitespace. `bench_plan` then answers
      `blocked: invalid_image` (a new `BlockedReason`), and `bench_start`
      refuses before podman is asked anything.
53. **A headline measured at two different points is no verdict** (review
    finding 1). The headlines read the nearest prompt length, the deepest
    depth and the largest stream count a run has. A canceled run that
    stopped at 2 of 4 streams compared with a complete one read "aggregate
    at all slots" 2 against 4 streams as a −45 % regression, and decode
    stopping at 16k against 130k as +100 %. Both counted in the tally and
    coloured the tiles. Now such a metric keeps its delta and noise, with a
    note naming both points, and its verdict is `not_same_point`. That
    verdict is not counted as a regression or an improvement. The dashboard
    shows it as an uncoloured "other point" chip, and the tiles mark the
    delta "≠". The tally names how many metrics were not at the same point.
54. **The vision probe reads whole words** (review finding 2). A substring
    match passed "colored", "covered", "rendered" and "not red". The answer
    is now split into words (letters and apostrophes, lowercased). It
    passes on `red`, `reddish`, `crimson` or `scarlet`, unless the word
    right before it is a negation (`not`, `no`, `never`, `nor`, `neither`,
    any `…n't`, or the same typed without the apostrophe). The detail names
    the word that passed, or the negated mention that failed.
55. **The first failed mixed stream ends the repetition** (review finding
    5). The decoding streams were joined with `join_all`. When one failed
    at once, the phase waited for the others to decode to their end: up to
    *S* − 257 tokens, about twenty minutes at 262k. They are joined with
    `try_join_all` now, so the first error ends the wait and drops the
    others, which closes their connections.
56. **A window shorter than two sampler ticks has no energy figure**
    (review finding 3). The counter is interpolated between the 100 ms
    ticks at both ends of a window. A prefill TTFT window at 512–2048
    tokens (15–60 ms on the 4090) lies inside one tick, and NVML's counter
    itself moves only every ~50 ms. Its "tok/J" was the neighbouring ticks'
    average power times the window's length, not a measurement. The bound
    comes from the sampler's own tick (`MIN_WINDOW_TICKS` = 2, so 200 ms):
    two ticks put at least one whole tick of readings inside any window, so
    at most half of it rests on interpolation. A point with a shorter
    window (any repetition's) gets `energy: null` and `energy_unmeasured`,
    which says the shortest window, the bound and the tick. The dashboard's
    energy chart keeps that point's category and writes "too short to
    measure" in the bar's place, in the tooltip and in the table. The same
    rule covers decode windows (256 tokens at more than 1 280 tok/s) and
    concurrent windows. The probes and the mixed phase have no energy
    figure, and the 2 s baseline is far above the bound. Suite v1 is not
    bumped: this removes numbers that were not measurements, and changes
    none that were.
57. **A shared KV pool is sized for, and a mixed phase that cannot fit is a
    planned skip** (review finding 4). With unified KV (`parallel` left on
    auto, or `kv_unified` on), `/slots` reports the whole pool as every
    slot's context, so *S* is the pool. Requests running together draw from
    it side by side, and a full pool aborts every running request
    (unified-KV design §2.1 fact 3). The mixed phase injected min(8192,
    *P_max*) beside *N_slots* − 1 streams that had decoded for 2 s and more.
    An auto row with a pool of about 12k overflowed, and the run failed. A
    split slot too small to decode through the steady window (*S* ≲ 1.3k)
    failed every repetition "ended during the steady window".
    - **Whether the pool is shared** comes from the server where it says:
      ik's top-level `n_ctx` against the per-slot one. Official llama.cpp's
      `/props` and `/slots` do not say, so the runner passes the row's
      `effective_kv_unified()`. `ServerFacts.kv_unified` and
      `kv_unified_source` record which. Unknown reads as split.
    - **The pool's size** is ik's whole context, else *S*. A per-slot cap
      can make *S* smaller than the pool, which only errs small.
    - **One request at a time uses all of it** (prefill, decode, probes):
      llama-server clears idle slots from a unified pool when a task starts
      (fact 5). The concurrent points stop where *N* streams of prompt + *G*
      no longer fit, with a note.
    - **The mixed phase measures before it runs.** One unmeasured request
      shaped like a stream (its prompt, *G* generated, no cache) reads one
      stream's decode rate *r* and prefill rate *p*. One stream on an idle
      server is the fastest a stream decodes, so the reserve below errs
      large. A stream's reserve for an injection of *P* tokens is ⌈*r* ×
      (lead + steady + *P* / *p*)⌉ + 2. The lead is the other streams'
      prompts, prefilled one after another at worst. The + 2 is its first
      token and the one the phase waits for after the injected first token.
      - Split KV: a slot that cannot hold prompt + reserve(*P_inj*) makes
        the phase a planned skip.
      - Shared pool: *P_inj* is the largest *P* up to min(8192, *P_max*)
        with (*N_slots* − 1) × (prompt + reserve(*P*)) + *P* + 1 ≤ pool. Each
        stream's `n_predict` is its reserve, so no stream can hold more than
        it was counted with. No *P* ≥ 1 fitting is a planned skip.
    - A planned skip, or a sized plan, replaces the plan's `mixed` entry and
      its `mixed:` note in the stored `results.points`, so the plan shows
      what ran. A skip is not a phase error.
    - Because *P_inj* now depends on measured rates, the mixed stall's
      headline is judged at its injection size: two runs that injected
      different lengths get `not_same_point` (decision 53).
    - The rate request is new traffic in the mixed phase. Suite v1 is not
      bumped: it is unreleased, and a split row with room keeps its old plan.
    *(Approved 2026-09-29.)*
58. **The corpus is tokenized as text, and prompts start with the BOS**
    (review finding 6). The corpus was tokenized with `add_special: false`
    and `/tokenize`'s default `parse_special: true`. The specs quote
    chat-template markers (`<think>` ×4, `<tool_call>` ×3, `<|channel|>`,
    `<|python_tag|>`, `<|tool_call|>`, `[TOOL_CALLS]`, …), so on a
    vocabulary that has them, prompts carried control tokens. A token-id
    prompt also gets no BOS from llama-server. Under `ignore_eos` that
    invites degenerate continuations, which flatter a drafter: the live
    gemma run read draft acceptance 1.0 at depth 64.
    - Official llama.cpp's `/tokenize` takes `parse_special: false`, and
      the engine sends it. ik_llama.cpp ignores the field: its `/tokenize`
      forces special parsing (`TMP_FORCE_SPECIAL`, checked in its source of
      2026-09-25). So the corpus also goes as an array of pieces, cut right
      after the opening bracket of every bracketed marker (`<` or `[`
      followed by its closer before any whitespace or other bracket: 316
      cuts in corpus v1). No piece holds a whole marker, so neither engine
      can match a control token. Both tokenize the pieces one by one and
      alike, so official and ik runs of one model get the same prompts. On
      an old SentencePiece vocabulary with a space prefix, each piece gets
      its own leading space, at those 316 points.
    - The BOS is what tokenizing one word with `add_special: true` puts
      before the same word without it. Every measured and unmeasured
      prompt starts with it, counted inside *P*, and `results.prompt_prefix`
      records it. It is empty when the vocabulary adds none. The needle's
      haystack stays plain corpus text: it goes through a chat template,
      which adds its own BOS.
    - Decode points keep `distinct_token_ratio` per repetition (distinct
      ids over generated ones), where the engine streams token ids (official,
      not ik). A low ratio is a loop, and the decode chart's tooltip shows
      it next to the draft acceptance.
    - This changes every prompt. Suite v1 is kept at 1: it is unreleased,
      and no run exists outside a dev instance. Dev runs from before this
      change compare with later ones as if nothing changed; delete them
      or read their deltas with that in mind.
59. **Compare names the base it used** (review finding 7). With
    `?compare=999,5,6` and run 999 unreadable, the view judged runs 5 and 6
    against run 5 while its subtitle said "against run 999". It still
    compares what loaded rather than refusing, since the other runs are
    worth seeing. Once the runs have loaded, the subtitle names the runs that
    loaded, with the first of them as the base (the legend marks it too),
    and names the runs that could not be read. A lost requested base is
    named as that: "(the requested base, run 999, could not be read)".
    Each failure's error stays in its own notice above.
60. **What the row records beside its status shows on every run**
    (review finding 8). The run detail showed `error` only for a failed or
    aborted run. A finished run whose container could not be removed
    (decision 48) had that note only in the runs table's tooltip.
    - Any run's `error` now shows under its status banner. A done run gets
      a "Recorded with the run" warning. An abort's own sentence, which
      the banner already says, is not repeated, but a note after it is.
    - A `status_reason` the banner does not explain is named.
    - A run with `unreadable` columns (decision 51) gets a warning in its
      detail listing them, and an "unreadable" chip in the runs table.
    - The plan view already showed every `BlockedReason` by its message.
      The ones that clear by themselves or by the owner's switch (`hold`,
      `run_going`, `booting`) now read "Cannot start yet" as a warning.
      The ones the request must change (`invalid_image`, `image_missing`,
      …) read "Cannot start" as an error.
61. **Each decode depth starts after a slot reset** (found in the final live
    check). ik_llama.cpp keeps every slot's cells in one KV buffer. Decode
    at depth 64 on Qwen3.5-0.8B (`-c 32768 --parallel 2`) read 383 tok/s
    right after the prefill phase, about the deepest point's 367, and 448
    in a decode-only run; a second run reproduced it (300 at depth 64
    against 364 at 1024). The previous phase's last prompt, 16 382 tokens,
    was still in the other slot. Official llama.cpp gives each slot its own
    KV stream, or with unified KV clears idle slots when a task starts
    (unified-KV design §2.1 fact 5), and read 503 at depths 64 and 1024
    alike. So the first repetition at each depth is now preceded by the
    decision 41 reset, one unmeasured request per slot. The later
    repetitions reuse the primed cache and get none, since a reset would
    evict it. Suite v1 is not bumped: official split rows measure the same,
    and ik runs from before this change read their shallow decode points
    low.
62. **A streamed request says `Connection: close`, and a request whose
    connection breaks before any response is sent once more** (found in
    the first production run, 2026-09-29).
    - What happened: gemma4-12b on official b11226 (`-c 512000 --parallel
      2`, MTP drafter, projector) lost prefill 32768's first repetition to
      `POST /completion: error sending request for url (…)`. llama-server's
      log has no trace of the request. The slot reset's two requests were
      released at 21:23:12.410 and .427, and the next task is the decode
      phase's reset at .443. The server stayed healthy, and the rest of the
      run finished.
    - The cause: official llama-server closes the connection after every
      streamed response, while the response's headers say `Keep-Alive:
      timeout=5, max=100`. `server-http.cpp`'s chunked content provider
      returns `has_next`, which is `false` after `sink.done()`. cpp-httplib
      0.58.0's `write_content_chunked` reads a `false` from the provider as
      `Error::Canceled`, so `process_request` fails, the keep-alive loop in
      `process_server_socket_core` ends, and `drain_and_close_socket`
      closes the socket. curl against the same image shows it: "Connection
      #0 left intact", then "Connection 0 seems to be dead" on the next
      request. A non-streamed request keeps its connection.
    - hyper believes the header. When it has read the final chunk before
      the engine drops the body (the engine returns at the `stop` chunk),
      it pools the connection. The measured request, sent at once after
      the slot reset, could take that connection before hyper had seen the
      server's FIN (forwarded by pasta). hyper then reports `client error
      (SendRequest): connection closed before message completed`, and the
      request never reached the server. It is a race: once in the
      production run.
    - Not the cause: cpp-httplib's thread pool (31 base threads here, up
      to 1055, no queue limit, so no socket is dropped for want of a
      thread), its listen backlog (512), the keep-alive count (100) and
      timeout (5 s). pasta only forwards the server's close.
    - Reproduced on the same image and model with `-c 65536 --parallel 2`,
      prefill only: the 14th measured request (2048 tokens, repetition 2)
      failed the same way, again with no trace in the server's log.
    - Fix, part 1: every streamed `/completion` sends `Connection: close`.
      cpp-httplib then answers `Connection: close`, and hyper never pools
      the connection.
    - Fix, part 2: a request that fails before any response on its
      connection is sent once more at once. That covers hyper's incomplete
      message, a closed or canceled connection, a reset, an abort and a
      broken pipe. A refused or timed-out connect is not resent: nothing is
      listening. The resend is logged and listed in the new
      `results.retried` (stage, request, and the first attempt's cause
      chain). The run detail shows it in a "Requests sent a second time"
      notice. What the step recorded is the second attempt's. A second
      failure is the phase's error and names both attempts. One resend,
      not more: the failure seen is one stale connection, and a server that
      fails twice in a row is failing.
    - Every `BenchError` built from a reqwest error now carries its whole
      `source()` chain. Before, only "error sending request for url (…)"
      was kept, which is why the production error did not say why.
    - Live check (same model, `-c 16384 --parallel 2`, prefill, 20
      repetitions, 181 streamed requests per arm, back to back on one
      server). Without the header, one request was resent (2048 tokens,
      repetition 7) with the production error, and its point was still
      measured. With the header, none was. TTFT − `prompt_ms` medians were
      187, 215 and 221 ms without the header and 207, 221 and 232 ms with
      it (512, 2048 and 8190 tokens), with overlapping ranges (179–233
      against 184–255 ms). Both arms opened a fresh connection for every
      measured request, and a loopback connect takes well under a
      millisecond, so the header should not move TTFT.
    - Reading each stream to its end was tried and is not done. Without
      the header it saw no failure in 145 streamed requests (hyper's
      delayed EOF gives the FIN time to arrive, but it stays a race). With
      the header the connection cannot be reused either way.
    - Test: the fake llama-server can hang up on the n-th `/completion`
      without reading it (`hang_up_completions`). One test resends and
      still measures the point; one fails twice and names both attempts.
