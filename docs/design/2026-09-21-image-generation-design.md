# Image Generation via stable-diffusion.cpp (lmgw) — Design

**Date:** 2026-09-21
**Status:** **Implemented 2026-09-21** (WP0–WP6, branch
`feat/image-generation`) — see §14 for the corrections found while building,
which are the record where they contradict the body above. The body is v2:
v1 was written from the sd.cpp source and docs at `c678dfe`; v2 folded in the
WP0 spike (§12, measured 2026-09-21 on the RTX 4090), which changed five
things: `--eager-load`, `--init` and the two directory flags are now mandatory
parts of the rendered argv, the health probe tolerates a 5xx, admission has to
reserve a generation's *transient* peak, and an edit request on a non-edit
pipeline is a segfault, so the `edit` gate is load-bearing

> Companion to [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
> ("containers §n" below), [2026-09-04-gpu-hold-design.md](2026-09-04-gpu-hold-design.md)
> and [2026-09-17-model-capabilities-design.md](2026-09-17-model-capabilities-design.md).
> This spec adds a **fourth local model class** beside chat / aux / audio:
> image (and, through the same server, video) generation served by
> [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp)'s
> `sd-server`, one Podman container per model, exposed through the gateway
> as OpenAI's `/v1/images/*`.

## 1. Summary

lmgw fronts llama.cpp (chat, embeddings, rerank) and audio.cpp (TTS, ASR and
eleven more tasks). The one modality a client of this gateway still cannot
reach locally is **image generation**. stable-diffusion.cpp is the ggml-family
engine for it: same GGUF weights and quant vocabulary, same CUDA/Vulkan
container images, and — since 2026 — an `sd-server` binary that speaks the
OpenAI image API natively. It is the natural fourth engine.

Decision in one paragraph: **image models become a fourth class (`image`),
engine `sdcpp`, one container per model exactly as containers §3 does for the
other three.** A row names the weight files that make up one pipeline
(diffusion model, VAE, text encoders, …), lmgw renders them into an
`sd-server` argv the way it renders llama-server argv, publishes the model as
`image/<id>` on `/v1/models` with `task: image_generation`, and routes
`POST /v1/images/generations` and `POST /v1/images/edits` to it through the
same acquire → admit → forward path every other local request takes. The
routes are passthrough for any `openai`-protocol upstream too, so a cloud
image alias (OpenAI's `gpt-image-*`, or anything OpenAI-shaped) works from
day one and doubles as a GPU-hold fallback target. Video generation rides
the same server and the same row and is deliberately **not** in the first
cut (§13).

## 2. What `sd-server` is — facts from the source, checked by the spike

Everything here is read from `examples/server/*.cpp`, `examples/server/api.md`,
`examples/common/common.cpp` and `docker/Dockerfile.cuda` at `c678dfe`.
§12 lists which of these must be confirmed on the RTX 4090 before WP1.

### 2.1 Process model

- `main()` parses three option groups — server (`--listen-ip`,
  `--listen-port`, `--serve-html-path`, `--log-level`), **context** (which
  files to load, backend placement, quantization) and **default generation**
  params (steps, cfg, sampler, size, seed, …) — then calls `new_sd_ctx` **once**
  and only then `svr.listen()`. There is no model switching, no lazy load and
  no second model: **one process = one loaded pipeline**, and the port is
  unreachable until the weights are up. That is exactly direct-mode
  llama-server's health semantics (containers §10.3): poll until a `200`,
  treat every other outcome the same.
- Health probe: `GET /sdcpp/v1/capabilities` (also `GET /v1/models`). Both
  answer only once the context exists. **Measured (§12.2): `capabilities`
  throws a filesystem exception and answers `500` whenever
  `--lora-model-dir` / `--hires-upscalers-dir` are unset** — the handler
  scans the (empty) dir setting as `.`, i.e. the container's `/`, and hits
  `/proc`. `/v1/models` is unaffected. Both flags are therefore always
  rendered (§3).
- **Weights load lazily by default.** The context exists and the port
  answers `200` about 0.9 s after `podman run`, with ~0.4 GiB in VRAM; the
  6.2 GB of weights are uploaded on the first request (§12.3). `--eager-load`
  moves the upload before `listen()` (ready at 2.2 s with the weights
  resident). lmgw renders `--eager-load` always: "ready" has to mean
  "resident" for the admission ledger's measured figure to mean anything,
  and the first request should not pay a load it cannot see.
- One generation at a time: every route takes `sd_ctx_mutex`. The OpenAI
  and `sdapi` routes are **synchronous** — the HTTP thread blocks until the
  image is encoded, a second request queues on the mutex. The native
  `/sdcpp/v1/*` routes go through an `AsyncJobManager` with a bounded queue
  (`429` when full) and a single worker thread.
- Default listen address is `127.0.0.1:1234`. Inside a rootless container
  that is unreachable from the host, so the argv must carry
  `--listen-ip 0.0.0.0 --listen-port <container port>` (spike item, §12).
- **SIGTERM is ignored by sd-server as PID 1** (no handler; §12.5):
  without `--init`, `podman stop` burns the whole grace and falls back to
  SIGKILL. With `--init` the signal reaches the process and it dies in
  ~0.3 s, idle or mid-generation, exit 143, VRAM released. `--init` is part
  of the rendered `podman run` line, not an option.

### 2.2 API families

| Family | Routes | Shape |
|---|---|---|
| OpenAI (`/v1/…`) | `POST /v1/images/generations` (JSON), `POST /v1/images/edits` (multipart), `GET /v1/models` | OpenAI image API; sync |
| A1111 (`/sdapi/v1/…`) | `txt2img`, `img2img`, `loras`, `upscalers`, `samplers`, `schedulers`, `sd-models`, `options` | WebUI compatibility; sync |
| native (`/sdcpp/v1/…`) | `GET capabilities`, `POST img_gen`, `POST vid_gen`, `GET jobs/{id}`, `POST jobs/{id}/cancel` | async jobs, full parameter schema |

`GET /v1/models` on the server returns one fixed id, `sd-cpp-local` — it is a
placeholder, not a catalog. lmgw never reads it for exposure (exposure is
table-driven, containers §5).

### 2.3 What the OpenAI routes actually read

`POST /v1/images/generations` reads **only**: `prompt` (required), `n`
(≥ 1), `size` (`WIDTHxHEIGHT`, falls back to the server's `-W/-H`
defaults), `output_format` (`png` | `jpeg` | `webp`), `output_compression`
(0–100). `model`, `response_format`, `quality`, `style`, `background`,
`user` are **ignored** — not rejected. The response is always
`{created, output_format, data: [{b64_json}]}`; there is no `url` mode and
no `revised_prompt`. `POST /v1/images/edits` is the multipart twin:
`prompt`, `image[]` (or legacy `image`), `mask`, `n`, `size`,
`output_format` (`png` | `jpeg`), `output_compression`.

Everything else — negative prompt, steps, cfg, sampler, seed, LoRA,
hires, tiling, cache — reaches the OpenAI routes through **one
extension**: a JSON block embedded in the prompt,

```
a lovely cat <sd_cpp_extra_args>{"negative_prompt":"blurry","seed":42,"sample_params":{"sample_steps":8,"guidance":{"txt_cfg":1.0}}}</sd_cpp_extra_args>
```

whose schema is the native `img_gen` request body. The server strips the
block before generation. `<lora:…>` prompt tags are refused on every
family; LoRA is the structured `lora: [{path, multiplier}]` field.

**CFG inside that block is `sample_params.guidance.txt_cfg`**, not
`sample_params.cfg_scale` — the latter parses and is then never read, measured
byte for byte while building WP5b (§14). The authority for every spelling here
is the `defaults` object a container publishes on
`GET /sdcpp/v1/capabilities`: it is the request struct's own serialization, so
it is also the parser's. (`cfg_scale` *is* the right key in a row's `args`,
where it renders as sd-server's `--cfg-scale` start-up default — a different
surface with a different vocabulary.)

Errors are **not** OpenAI-shaped: `400 {"error": "<string>"}` for a missing
prompt, and `500 {"error":"server_error","message":…}` for *anything*
thrown — including a JSON parse error, which is a 500 here, not a 400
(§12.6). lmgw normalizes them into its own error envelope like every other
passthrough (§6). A `model` field is ignored, so lmgw's rewrite is
harmless and nothing needs stripping.

### 2.4 Native API and video

The native body carries the full parameter surface (`sample_params`,
`guidance`, `hires`, `vae_tiling_params`, `lora`, `init_image`,
`ref_images`, `mask_image`, `control_image`, `ip_adapter_image`, cache
controls, `output_format`) and returns `202` + a job to poll. Job ids are
**per process** — a poll must reach the container that issued it. Video
(`vid_gen`) is the same server with a video-capable pipeline loaded
(`supported_modes` in `capabilities` says which); the result is one
encoded container (`webm` | animated `webp` | `avi`) as base64.

### 2.5 Container images

`ghcr.io/leejet/stable-diffusion.cpp` publishes exactly five tags (skopeo,
2026-09-21): `master-cuda`, `master-cuda-spark`, `master-vulkan`,
`master-sycl`, `master-musa`. **There is no plain `master` / CPU tag** despite
`docs/docker.md` naming one; a CPU image is a local `docker/Dockerfile`
build. All tags are **moving** (rebuilt per merge to master; the current
`master-cuda` was built 2026-09-20 from `c678dfe`). Pinning is by digest,
via the per-model `image` override that every class already has (containers
§3.1) — same posture as the llama class default `ggml-org/llama.cpp:server-cuda`.

`master-cuda` (spiked build: digest `sha256:8771c2d5…`, `--version` says
`version unknown, commit c678dfe` — the commit is the only version pin the
binary reports): `nvidia/cuda:12.6.3-cudnn-runtime-ubuntu24.04` base, ggml
built with `GGML_BACKEND_DL` + all CPU variants, both binaries under
`/sd.cpp/bin/` with `/sd-cli` and `/sd-server` wrapper scripts. The binary
links `libcuda.so.1` directly, so **even `-h` and `--version` need the GPU
device attached** (§12.7) — the help-vocabulary probe runs with the class's
`extra_run_args`, not bare. **The image
entrypoint is `/sd-cli`**; the server needs `--entrypoint /sd-server` (the
docs also pass `--init`). Expected mounts: the models dir at `/models`
(read-only is enough — the server writes nothing; `sd-cli` wants `/output`
but the server returns bytes). AMD/Intel: `master-vulkan` with the class's
`extra_run_args` swapped, exactly like docs/amd-arch.md does for the other
classes; no ROCm image is published.

### 2.6 Model files and flags

Weights are `.gguf`, `.safetensors`, or `.ckpt`/`.pt`/`.pth`; the server
converts and (with `--type`) re-quantizes on load. Two ways to name a
pipeline:

- **`-m/--model <file>`** — an all-in-one checkpoint (SD 1.x/2.x, SDXL,
  SD3 with bundled encoders). Optional `--vae` override.
- **`--diffusion-model <file>`** — a standalone DiT/UNet, plus its
  components: `--vae`, and the text encoders the family needs — `--clip_l`,
  `--clip_g`, `--t5xxl`, `--llm` (a plain llama.cpp GGUF: Qwen3-4B for
  Z-Image, Qwen2.5-VL-7B for Qwen-Image, Mistral-Small-3.2 for FLUX.2),
  `--llm_vision`, `--clip_vision`. Extras: `--taesd`, `--control-net`,
  `--ip-adapter`, `--photo-maker`, `--upscale-model`,
  `--high-noise-diffusion-model` (Wan 2.2), `--tokenizer`, and directory
  flags `--lora-model-dir`, `--hires-upscalers-dir`, `--embd-dir`.

Runtime flags that matter for a 24 GiB card: `--type` (load-time requant),
`--offload-to-cpu` (weights in RAM, streamed in on use), `--diffusion-fa` /
`--fa`, `--sage-attn`, `--vae-tiling`, `--backend` / `--params-backend`
(per-module device placement), `--max-vram` (per-device GiB budget for the
automatic graph cut), `--auto-fit`, `-t/--threads`, `--mmap`, `--rng`,
`--prediction`, `--vae-format`, `--lora-apply-mode`. Generation defaults
the server applies when the request is silent: `--cfg-scale`, `--guidance`,
`--steps`, `--sampling-method`, `--scheduler`, `-W`/`-H`, `--seed`,
`--clip-skip`, `--flow-shift`, `--negative-prompt`, `--batch-count`,
`--video-frames`, `--fps`.

`--help` is the vocabulary: `sd-server -h` prints all three groups. lmgw
validates a row's flags against the **image's** help text the way it does
for llama-server (containers §3.6, keyed by image).

### 2.7 Reference packages — what one working pipeline needs on disk

| Family | Files (source) | Notes |
|---|---|---|
| **Z-Image-Turbo** | `z_image_turbo-Q4_K.gguf` 3.9 GB (`leejet/Z-Image-Turbo-GGUF`) · `ae.safetensors` 335 MB (`black-forest-labs/FLUX.1-schnell`) · `Qwen3-4B-Instruct-2507-Q4_K_M.gguf` 2.5 GB (`unsloth/…-GGUF`) | 8 steps, `--cfg-scale 1.0`, `--diffusion-fa`; docs claim ≤ 4 GB VRAM. **The spike model.** |
| FLUX.1-schnell | `flux1-schnell-q8_0.gguf` (`leejet/FLUX.1-schnell-gguf`) · `ae.safetensors` · `clip_l.safetensors` · `t5xxl_fp16.safetensors` 9.8 GB (`comfyanonymous/flux_text_encoders`) | 4 steps, cfg 1.0, `--clip-on-cpu` in the docs |
| SDXL | `sd_xl_base_1.0.safetensors` · `sdxl_vae-fp16-fix.safetensors` | `-m` checkpoint form, 1024² |
| Qwen-Image | `qwen-image-Q8_0.gguf` (`QuantStack/Qwen-Image-GGUF`) · `qwen_image_vae.safetensors` · `Qwen2.5-VL-7B-Instruct-Q8_0.gguf` | strong text rendering |
| FLUX.1-Kontext | `flux1-kontext-dev-q8_0.gguf` (`QuantStack`) + the FLUX.1 encoders | **edit** model: `-r` reference → `/v1/images/edits` |

Measured detour (§12.1): `black-forest-labs/FLUX.1-schnell` is a gated
repo (`gated: auto`) and answers **401** without a token that accepted the
licence; the byte-identical `ae.safetensors` is un-gated under
`Comfy-Org/z_image_turbo/split_files/vae/`. FLUX.1-dev and Kontext are
gated the same way. Recipes name un-gated mirrors where one exists and mark
a component `gated` where none does; the downloader turns a 401 into
"gated repo — set the Hugging Face token in Settings and accept the licence
on the hub", not a bare HTTP error.

Three things follow for lmgw. First, **a package spans repos**: the VAE, the
encoders and the diffusion model live under different owners, so the
"download a weights file with its companions from one repo" model of
`lmgw__hf_add` does not describe an image model. Second, **safetensors are
mandatory** — every VAE and CLIP encoder above is one — so the GGUF-only
gate in the downloader and the `gguf_files` listing cannot stand for this
class (§7). Third, gating is per repo, so a recipe is a list of
*(repo, file)* pairs with an explicit mirror choice, not a family name.

## 3. Runtime model — the fourth class

Everything containers §3 built generalizes without a new mechanism:

- **Class** `Image` joins `Chat | Aux | Audio`; **engine** `sdcpp` joins
  `llama | audio`. The registry key stays `(class, model_id)`; container
  name `<prefix>-image-<slug>-<hash6>`; labels gain nothing new
  (`lmgw.engine=sdcpp`).
- **Synthetic upstream** `sdcpp`, id **−4**, beside `llama-server` (−1),
  `llama-aux` (−2), `audiocpp` (−3); never persisted; `base_url` overwritten
  with the acquired endpoint after `admit()` exactly as §5 there.
- **Descriptor**: `ModelRuntime` for an image row renders **argv**, not a
  config file — sd-server has no config file and the argv renderer with its
  switch-vs-value semantics already exists. Rendered shape:

  ```
  podman run -d --replace --init --name <n> --label … -p <host>:8080
    -v <image models_dir>:/models:ro <extra_run_args…>
    --entrypoint /sd-server <image>
    --listen-ip 0.0.0.0 --listen-port 8080 --eager-load
    --lora-model-dir /models/loras --hires-upscalers-dir /models/upscalers
    --diffusion-model /models/<…> --vae /models/<…> --llm /models/<…>
    <runtime flags…> <generation defaults…>
  ```

  Five of those are **not** the row's business and are rendered
  unconditionally, each for a measured reason (§12): `--init` (SIGTERM is
  otherwise ignored), `--listen-ip 0.0.0.0` (the default binds the
  container's loopback), `--eager-load` (ready must mean resident), and
  the two directory flags (unset, the capabilities route throws). The
  `loras/` and `upscalers/` directories are created under the class
  `models_dir` when the class is first configured; a row's own
  `lora_model_dir` / `hires_upscalers_dir` in `files` overrides the
  default paths, never removes the flags.

  Every file value is rewritten `<rel path>` → `/models/<rel path>` (the
  `/models` mount rewrite the llama renderer already does). Directory flags
  (`--lora-model-dir`) are rewritten the same way.
- **Health**: `RenderSpec::health_path` gains a third value —
  `/sdcpp/v1/capabilities` beside `/health` (llama) and `/v1/models`
  (audio) — polled by the existing `await_health()`; `load_timeout_seconds`
  applies unchanged. A failed start carries `podman logs --tail` as today.
  One refinement the spike forced: the poll treats **any HTTP response**
  as "the server is up" (before that the port answers connection-reset
  for ~0.6 s), and a `5xx` on `capabilities` makes the model *ready with a
  warning* rather than never-ready — generation still works when only the
  LoRA scan is broken, and the `EXCEPTION_WHAT` header the server sets on
  such a 500 is the warning text. A ready-with-warning start must not burn
  the whole load timeout.
- **Death mid-request is a real path here.** An edit request on a
  non-edit pipeline is a segfault (§12.8): the client's connection drops
  with no status, the container is `exited (139)`. The registry's inspect
  tick already turns an exited container into "absent" and the next
  `acquire` restarts it; the forwarding side maps a dropped upstream
  connection to the gateway's `upstream_error` with the container's last
  log lines, the same shape a failed start reports. Nothing new in
  mechanism — but the `edit` gate (§4) exists precisely so this path is a
  bug report and not a routine.
- **Boot adoption compares argv only.** The audio class needs the extra
  "does the mounted `server.json` still match" check because its argv is a
  constant; an image container's argv names every file and flag, so the
  chat-class rule (inspected `Cmd` == rendered argv) is complete here.
- **`idle_seconds` is a real column from day one.** Audio has none yet and
  reads `0` (never reap) — a known gap in `descriptor.rs`/`lifecycle.rs`;
  this class does not inherit it. A loaded FLUX pipeline is 12+ GiB of VRAM
  that nothing else can use, so idle unload matters more here than anywhere.
- **Busy probe**: sd-server has no `/slots`; like audio, the in-flight
  ledger is the only signal. The eviction/reaper refusal on
  `in_flight > 0` covers the synchronous routes fully, because the guard is
  held for the life of the response (§6). (This is the reason the async
  native API is a follow-up — a `202` would drop the guard while the job is
  still on the GPU, §13.)
- **Idle reaper**, **warm_start**, **apply = recreate if running**,
  **logs**, **stop refuses on in-flight unless override** — all inherited.
- **Capabilities probe at start** (new, small): after the health `200`, read
  `capabilities` once and keep `supported_modes`, `limits`, `samplers`,
  `schedulers`, `loras`, `upscalers` in the runtime entry. It is the only
  source for "is this an edit-capable / video-capable pipeline" that does
  not require lmgw to know every family's rules; it feeds the notes on
  `/v1/models` (§5) and the Image lab's pickers (§8). Not persisted —
  re-read on every start.

## 4. The image model row

Table `image_models`, class settings `settings.image`, both following the
audio precedent (own table, own dir, own prefix — no merge, containers
§3.1). Columns:

| Column | Meaning |
|---|---|
| `model_id` | client-facing id under the prefix (`image/<model_id>`) |
| `files` | JSON map **flag key → path relative to the image models dir**: `model`, `diffusion_model`, `vae`, `clip_l`, `clip_g`, `t5xxl`, `llm`, `llm_vision`, `clip_vision`, `taesd`, `control_net`, `ip_adapter`, `photo_maker`, `upscale_model`, `high_noise_diffusion_model`, `tokenizer`, `lora_model_dir`, `hires_upscalers_dir`, `embd_dir`. Keys are the long flag with `-` → `_`; exactly one of `model` / `diffusion_model` is required. A map, not nineteen columns: sd.cpp adds a family (and its flag) every few weeks, and the renderer validates keys against the image's `--help` rather than a Rust enum. |
| `args` | JSON map of runtime + default-generation flags, same canonical-key convention as `LocalModel.args` (`type`, `offload_to_cpu`, `diffusion_fa`, `vae_tiling`, `cfg_scale`, `steps`, `sampling_method`, `width`, `height`, …); booleans render as bare switches. Free-form so the whole vocabulary is reachable; the editor gives the dozen that matter their own widgets. |
| `modes` | `["img_gen"]` / `["img_gen","vid_gen"]` — what the operator says this pipeline does; checked against the probed `supported_modes` at start and the mismatch surfaces as a `model_warnings` entry, never silently. Drives `task` and `endpoints` on `/v1/models` **before** the container has ever run (exposure cannot wait for a probe). |
| `edit` | bool: the pipeline is an image-edit model (Kontext, Qwen-Image-Edit, Z-Image-Omni) — advertises `/v1/images/edits` and `input_modalities: [text, image]`. Same "operator says, probe warns" rule — and **load-bearing**: `/v1/images/edits` to a row without it is refused by lmgw with a 4xx naming the flag, because sd-server itself segfaults on that request (§12.8). |
| `enabled`, `image?`, `extra_run_args?`, `warm_start`, `idle_seconds`, `hold_fallback_mode`, `hold_fallback`, `capabilities_override?` | identical to the other three tables |

Class settings `ImageSettings { image, models_dir, extra_run_args,
public_prefix }` — the four every class has; default image
`ghcr.io/leejet/stable-diffusion.cpp:master-cuda`, default prefix `image`.
No engine-specific class fields (sd-server has no `server.json`; anything
per-process lives in `args`).

Pre-flight at start / apply, in the `models_with_problems` shape: every
`files` value must exist under `models_dir`; a `files` key or `args` key
outside the image's help vocabulary is a problem naming the key; a row with
neither `model` nor `diffusion_model` is refused at save time.

## 5. Routing, exposure, capabilities

- `exposed_models` enumerates enabled `image_models` under
  `settings.image.public_prefix` — table-driven, same tier as aux/audio:
  `Snapshot::resolve_image_local` / `image_public_name` /
  `enabled_image_models` beside their audio twins in `config.rs`,
  `UpstreamKind::SdCpp` for the synthetic upstream, a `for_image` arm in
  `capabilities/exposed.rs` and `capabilities/mod.rs`, `notes_for_image` in
  `capabilities/notes.rs`.
- Capabilities (`source: config`, like aux/audio): `task:
  image_generation` (or `image_edit` when `edit`; `video_generation` when
  `modes` has only `vid_gen`), `endpoints: ["/v1/images/generations"]` (+
  `"/v1/images/edits"` when `edit`), `input_modalities: [text]` (+ `image`
  when `edit`), `output_modalities: [image]`, no `reasoning`, no
  `tool_calls`, no `context_length`, no `max_output_tokens` — every absent
  field stays absent. **Notes** carry what an agent needs to use the model
  well, each true for *this* row: the default size/steps/cfg from `args`
  (or the family's if unset), the `sd_cpp_extra_args` extension with one
  example, "images come back as `b64_json` only", and — once the runtime
  has been probed — the accepted size limits, samplers and LoRA names.
- New capability vocabulary added to the capabilities spec's `task` table:
  `image_generation | image_edit | video_generation`; `output_modalities`
  gains no new value (`image` and `video` exist). The list-level
  `lmgw.notes` prefix sentence gains `'image/…' local stable-diffusion.cpp
  models`.
- Cloud image models: an alias onto an `openai`-protocol upstream whose
  catalog says `output_modalities: [image]` (OpenRouter/Kilo-shaped catalogs
  do) gets the same `task` and endpoints from `source: catalog`. Nothing
  else changes for cloud.

## 6. Ingress routes

Two new routes on the OpenAI router, mirroring `/v1/audio/speech` and
`/v1/audio/transcriptions` line for line — thin axum handlers in
`server.rs` beside the audio ones, the logic in `proxy.rs` next to
`resolve_audio` / `audio_json_call` / `handle_audio_transcription` /
`finish_audio` (a `resolve_image` guard, a JSON call, a multipart relay,
one `finish`), and a `RequestClass::Image` in `telemetry.rs`:

- `POST /v1/images/generations` — JSON. Resolve `model` (alias → route);
  refuse a model whose capabilities lack `/v1/images/generations` with the
  same 4xx the audio routes use for a wrong `task`; rewrite `model` to the
  concrete id; forward **verbatim** otherwise; stamp the usual headers.
  Passthrough for any `openai`-protocol upstream, local or cloud.
- `POST /v1/images/edits` — multipart, relayed as the audio transcription
  route relays uploads; `model` field rewritten in the form. Refused
  before forwarding when the resolved model does not advertise the route
  (a local row without `edit`, a cloud model without `image` among its
  input modalities) — for a local row that refusal stands between the
  client and a crashed container (§12.8).
- Both are unbounded by `max_body_mb` like the audio routes (an edit carries
  images; a generation body is small but the rule is per-family).
- Both hold the acquire/admission guard **for the life of the response
  body** — on the sync routes that is the whole generation. Timeouts: the
  synthetic upstream's `timeout_ms` (600 s) is the ceiling; FLUX at 1024²
  on this card is expected well inside it (§12 measures).
- **Error normalization**: sd-server's `{"error":"…"}` string becomes the
  gateway's `{"error":{"message","type":"upstream_error","code":…}}`
  envelope with the status preserved — a client must never see two error
  shapes from one route. A `429` from the native queue cannot occur on the
  sync routes.
- **Logging/usage**: one `request_logs` row per call, route
  `/v1/images/generations`, class local → `free_local`, zero tokens (there
  are none), latency recorded; the row's `timings` slot carries what the
  server reports (nothing today — sd-server returns no timing; the
  gateway's own wall-clock is the figure). Key policy: scope/budget/rate
  apply exactly as the audio routes had them added; a
  requests-per-minute limit is the meaningful knob for image traffic since
  tokens-per-minute sees nothing.
- **Not in v1**: `response_format: url` (sd-server cannot; lmgw could store
  and serve files but that is a new persistence surface), the native async
  job routes, `/sdapi/v1/*` (§13).

lmgw adds **no extension fields of its own** to the request. The escape
hatch is sd.cpp's, documented in the model's notes; inventing lmgw-only
top-level fields would have to be stripped before every cloud upstream and
would double the contract for no capability the extension does not already
give.

## 7. Downloader and recipes

Two changes to the Hugging Face path, both scoped by `target=image`:

1. **File kinds.** `hf_repo` / `hf_add` / `gguf_files` accept and list
   `.gguf`, `.safetensors`, `.ckpt`, `.pt`, `.pth` for the image target;
   the other targets keep their GGUF-only rule. `classify_repo_file` gains
   image kinds by filename — `vae` (`ae.`, `_vae`, `vae_`), `text_encoder`
   (`clip_l`, `clip_g`, `t5xxl`, `qwen`, `mistral` …), `diffusion`
   (everything else `.gguf`/`.safetensors` in a `diffusion_models/` dir or
   a `-GGUF` repo), `lora`, `upscaler` (`esrgan`, `realesrgan`),
   `checkpoint` (a single big safetensors at the repo root) — heuristics,
   labelled as such, overridable in the editor. `lmgw__gguf_files
   target=image` returns every accepted kind; the tool description says so
   (the name is kept; renaming a tool every agent already knows is worse
   than a name that is one word too narrow).
2. **Recipes.** A pipeline spans repos, so "add from catalog" for this
   class is a **curated, shipped list** — `image_recipes.rs`, one entry per
   family: display name, the component list (`role → repo, file, size`,
   with the quant alternatives for the diffusion model), the `args` the
   family wants (`cfg_scale`, `steps`, `sampling_method`, `diffusion_fa`,
   size), `modes`, `edit`. "Add from recipe" downloads every component
   through the existing downloader (one `hf_downloads` job each, live
   progress as today), then hands a **prefilled row** to the editor —
   the audio catalog flow, with a static source instead of audio.cpp's
   `model_specs`. Ship with Z-Image-Turbo, FLUX.1-schnell, FLUX.1-dev,
   FLUX.1-Kontext, SDXL, Qwen-Image; adding one is a Rust literal, not a
   schema. *(Shipped 2026-09-21 with eight more, from upstream's own
   per-family docs: Z-Image base, FLUX.2 klein 4B, FLUX.2 dev, Chroma1 HD,
   SD 3.5 Large, Qwen-Image 2.1, Qwen-Image-Edit 2509, and Wan 2.1 T2V 1.3B —
   the first `vid_gen` row. The literal-not-a-schema claim held: no DTO
   changed to add them. FLUX.2 dev is the first row whose `args` carry
   `offload_to_cpu`: §9's footprint arithmetic assumes a pipeline is resident,
   and this one deliberately is not — its 19 GB DiT and 14 GB encoder are
   streamed in per module, so the peak is one of them rather than their sum.)* A recipe is also what `local_model_plan` returns for this class:
   there is no GGUF header to plan from (the diffusion GGUF has no
   `general.architecture` lmgw understands), so the plan for an image model
   is "this file matches recipe X; here is X's parameter set" or "unknown
   family — set `files` by hand".

Shared components (the FLUX `ae.safetensors`, `t5xxl`) are downloaded once
into `<image models_dir>/<owner>/<repo>/<file>` and referenced by several
rows — the path convention already makes that natural. What §7.2 missed is
that a shared component also has to be *presented* as shared: the first
build rolled one recipe's live download up into every neighbour's card, so
adding Z-Image-Turbo made five FLUX and Chroma rows show a progress bar and
hide their Add button. A card claims progress only when nothing of it is
still un-queued (or when that browser queued the rows itself), and each
component names the other recipes that want the same file
(`components[].shared_with`). A `--llm` encoder is
an ordinary llama.cpp GGUF, but it lives in the image models dir, **not**
hard-linked from the chat dir: the classes' directories are separate on
purpose (MCP instructions, `gguf_files`), and 2.5 GB twice is cheaper than a
cross-class path.

## 8. Surfaces

- **ops / MCP — one deliberate departure from the audio precedent.**
  Audio model CRUD lives in the web layer only (`web/api.rs::audio_model_set`,
  validators in `web/audio.rs`); `lmgw__local_model_get|set|test` refuse
  `target=audio` and point at the dashboard, and no `lmgw__audio_model_set`
  exists. The image class gets an **ops-level `image_model_set`** shared by
  `/api/op/image_model_set` and a new `lmgw__image_model_set` (`full`
  tier), the way `aux_model_set` is shared. Reason: the whole point of this
  gateway's MCP plane is that an agent can bring a model up end to end
  (`hf_add` → `plan` → `set` → `test`); audio broke that chain because its
  rows carry voice presets and clip files that only the dashboard could
  validate, and an image row is `files` + `args` — exactly the shape the
  aux tools already handle. `lmgw__local_model_get|check|test|plan`,
  `lmgw__container`, `lmgw__gguf_files`, `lmgw__hf_add|hf_set` gain
  `target=image` / class `image`.
  `local_model_test` for this class generates one small image (256², 4
  steps — the smallest thing the loaded pipeline will do) and reports bytes
  and wall time; an encoder cannot generate, a diffusion model cannot chat,
  so the test is per class as it already is for aux.
- **`/api`**: `image_models` in the models domain payload; `ImageModelDto`
  in `lmgw-api-types`; settings DTO gains the `image` section
  (`deny_unknown_fields` — a contract change with a release note, as
  containers §6 did).
- **UI**: Models page gets the Image class section with the same row
  actions (enable, start/stop/apply/logs, test, edit, delete); an **image
  model editor** (files with a picker per role over the image dir listing,
  the dozen well-known args as widgets, a raw args JSON box that merges
  last, the rendered command line panel like `local_edit`); **Add from
  recipe** beside Add from catalog; Settings → Image class (image,
  models_dir, extra_run_args, prefix). An **Image lab** page (Audio lab's
  sibling): model picker, prompt / negative prompt, size, steps, cfg, seed,
  n, the sampler/scheduler pickers filled from the probed capabilities,
  optional LoRA rows, an edit panel with image + mask upload; results as a
  gallery with download; the request body and target endpoint shown;
  dispatched in-process through the real handlers so every call lands in
  Logs. Overview's runtime table needs nothing — a fourth class string.
- **Tray**: nothing — it already stops "all models" by registry, not by
  class.
- **README**: a §5-style "Image" entry under Configure, the
  `/v1/images/*` routes in the endpoint list and the `lmgw.endpoints`
  block, capability `task` values, the recipe list.

## 9. VRAM, hold, reaper

- **Footprint estimate** (planning only; residency is measured by NVML as
  for every class): `Footprint::image()` in `vram/plan.rs` = the sum of
  the sizes of every file in `files` (the audio class's
  `Footprint::audio()` directory-size rule, applied to a file list). Honest
  caveat in the plan output: `--offload-to-cpu` and `--params-backend`
  make the estimate an upper bound for the *idle* figure, and no file size
  says anything about the compute buffers. **Measured (§12.4): the idle
  residency of the Z-Image-Turbo Q4_K pipeline is 7.1 GiB, and a single
  1024² generation transiently needs 13.7 GiB** — the compute buffers
  (≈ 6.6 GiB at that size, resolution-dependent) are allocated per job and
  freed after it. An admission ledger that only knows the idle figure
  would admit a 15 GiB chat model beside a resident image model and OOM
  the next generation. So the image class carries **two** figures: the
  measured idle residency (as every class) and a learned
  `peak_extra_bytes` — the largest `used − idle` delta sampled from the
  driver while the row had a request in flight, kept per row in
  `image_models` (migration 0035) and charged by the ledger for as long as
  the pipeline is `ready`. Built 2026-09-21, see the WP7 block in §14.
  Until a generation has run it is unknown, and every surface says so
  rather than inventing a multiplier. `--offload-to-cpu` is
  the knob for a shared card: idle 1.0 GiB, peak 7.7 GiB, +0.4 s per image
  (§12.4); the editor shows those numbers beside the switch.
- **GPU hold**: per-model fallback only (`hold_fallback_mode`), like
  aux/audio — the global chat fallback does not apply. A cloud image alias
  is the fallback target; the `x-lmgw-fallback` header tells the client.
- **Reaper / eviction**: in-flight guard is authoritative (§3); a
  generation cannot be interrupted by an eviction because the guard is held
  until the response body is done.

## 10. Testing

- `runtime_argv.rs`: image argv rendering — `--entrypoint /sd-server`
  placement, `/models` rewrite of every `files` value and every dir flag,
  `--listen-ip 0.0.0.0`, switch-vs-value for `args`, the `model` vs
  `diffusion_model` exclusivity, unknown-key rejection against a fixture
  `--help`.
- `runtime_descriptor.rs` / `runtime_registry.rs`: fourth class, id −4
  synthetic upstream, container name and labels.
- `models_endpoint.rs` + `capabilities_local.rs`: an enabled image row is
  exposed under the prefix with the right `task`/`endpoints`/modalities and
  nothing invented; `edit` and `modes` variants; override merge.
- New `image_backend.rs`, mirroring `audio_backend.rs`'s 25 cases
  (wiremock as sd-server): generation and edit passthrough, `model` rewrite, error-string normalization, unbounded body,
  key-policy refusals, one log row with zero tokens, guard held to body end
  (the wiremock delays the body and the test asserts the reaper skipped).
- `hf_download.rs`: image-target file kinds, safetensors download, recipe
  → multi-download → prefilled row.
- `migrations.rs`: the `image_models` table and the settings blob's new
  section.
- Gated on the real container (skips cleanly when absent): one end-to-end
  generation against Z-Image-Turbo — the same gate the llama tests use.

## 10b. Touch map — what exists and what is new

The runtime/lifecycle/hold/tray layer is already generic over `Class`
(`runtime/mod.rs` is the one class enum in the crate): adding
`Class::Image` with its `as_str`/`parse`/`engine` arms ripples through
`container_name`, the podman labels, `hold_sweep`, `ops::container` and
`normalize_group_target`, the tray's "start warm / stop all", and the UI's
`containers.rs`. Nothing in `src-tauri` or `ci/` changes.

| Layer | Existing precedent (audio) | New for image |
|---|---|---|
| store | `migrations/0013_audio.sql` + `0014`, `0022`–`0024`; `store.rs:1264-1409` CRUD; `Snapshot.audio_models` | `migrations/00NN_image.sql` (table incl. `idle_seconds`, `hf_models.target` CHECK widened); `ImageModel` + CRUD; `Snapshot.image_models` |
| config | `AudioSettings` (config.rs:1675), `AudioModel` (:545), `audio_upstream()` (:2125), `resolve_audio_local` (:2250), `audio_public_name` (:2191) | `ImageSettings`, `ImageModel`, `image_upstream()` id −4, `resolve_image_local`, `image_public_name`, `UpstreamKind::SdCpp` |
| runtime | `descriptor.rs::audio_runtime()`, `argv.rs::EngineArgs::Audio` + `render_audio_args`, `runtime/audio.rs` (server.json), `health_path` per class, `lifecycle.rs::acquire_spec` (class → models_dir) | `image_runtime()`, `EngineArgs::Image` + `render_image_args` (argv from `files` + `args`, `/models` rewrite, `--entrypoint /sd-server` placement in `podman_run_argv`), health path, `acquire_spec` arm; **no config dir** |
| capabilities | `capabilities/exposed.rs` `for_audio` (~185-342), `mod.rs:695 for_audio`, `notes.rs:367 notes_for_audio` | `for_image`, `notes_for_image`, new `task` values |
| routes | `server.rs:822-905` handlers; `proxy.rs:2606-3040` (`resolve_audio`, `audio_json_call`, `handle_audio_transcription`, `finish_audio`); `telemetry.rs:215 RequestClass::Audio`; policy | the same four for image; `RequestClass::Image`; policy hook |
| vram | `vram/plan.rs:306 Footprint::audio` | `Footprint::image` |
| ops / api | `web/api.rs:684 audio_model_set` (web-layer only), `ops.rs:3984 audio_model_problems`, `ops.rs:4317 container` | `ops::image_model_set` (shared), `image_model_problems`, `Class::Image` arms in `container`/`local_model_*`/`hf_*` |
| mcp | `selfadmin.rs:1204-1341` target enums; no audio CRUD tool | `lmgw__image_model_set`; `image` in every target enum |
| hf | `hf.rs:18 TARGETS`, `dest_dir`, `ops.rs:2971/3016/3083` GGUF gates, `classify_repo_file` | `image` target; per-target accepted extensions; image file kinds; `image_recipes.rs` |
| api-types | `AudioModelView`/`AudioModel`/`AudioSettings`/catalog DTOs; `ModelsFull.audio` | `ImageModelView`/`ImageModel`/`ImageSettings`/`ImageRecipe*`; `ModelsFull.image` |
| ui | `pages/models.rs` audio section + `AudioRow`; `model_editors.rs:770 AudioEditor`; `pages/audio_catalog.rs`; `pages/audio_lab.rs` (+`audio_spec.rs`, `audio_stream.rs`); `overview.rs:155-180 class_lit/rank/title`; `settings.rs` audio section | image section + `ImageRow`; `ImageEditor`; `pages/image_recipes.rs`; `pages/image_lab.rs`; `"image"` arms in overview (sd-server *has* a web UI at `/`, so the per-row direct link applies); settings section |
| tests | `tests/it/audio_backend.rs` (25), `audio_catalog.rs`, class cases in `runtime_argv/descriptor/lifecycle`, `capabilities_*` | `tests/it/image_backend.rs`, `image_recipes.rs`, class cases added beside audio's |
| docs | README §5 audio, `/v1/models` example, capabilities table | README image entry, capabilities `task` rows, this spec |

## 11. Work packages

| WP | Content | Verifies |
|---|---|---|
| **0 — spike** | §12, by hand with podman, before any code — **done 2026-09-21** | the assumptions in §2 |
| **1 — class plumbing** | `Class::Image`, `Engine::Sdcpp`, table + migration, `ImageSettings`, `ImageModel`, descriptor + argv renderer, lifecycle (health probe, capabilities probe), synthetic upstream −4, footprint | `runtime_*`, `migrations` — **done 2026-09-21** |
| **2 — routes + exposure** | `/v1/images/generations|edits`, capabilities/notes, exposure, policy, logging, error normalization | `image_routes`, `models_endpoint`, `capabilities_local` — **done 2026-09-21** |
| **3 — files** | downloader file kinds, `gguf_files target=image`, recipes, `local_model_plan` for the class | `hf_download` — **done 2026-09-21** |
| **4 — operator surfaces** | ops + `/api` + MCP tools + DTOs + `local_model_test` for the class | `mcp_selfadmin`, `ops_container`, `tool_inventory` — **done 2026-09-21** |
| **5 — UI** | Models section + editor, Add from recipe, Settings section, Image lab | `web_pages`, manual — **done 2026-09-21 (dashboard and Image lab)** |
| **6 — docs** | README, capabilities spec table, release note for the DTO change | — **done 2026-09-21**, and the release note needed a place: `docs/release-notes.md` is new |
| **7 — learned peak** | §9's `peak_extra_bytes`: migration 0035, the `vram::peak` sampler, the ledger charge, the reset on a pipeline change | `migrations`, `vram_admission`, `image_live` — **done 2026-09-21** |

WP1 → WP2 → WP4 is the critical path (a curl-able local image model);
WP3 and WP5 hang off it. Branch `feat/image-generation`, merged back to
`main` when WP2 answers a real request on this box.

## 12. Spike results (WP0, measured 2026-09-21)

Host: the RTX 4090 (24 GiB; ~0.8 GiB held by the desktop throughout),
image `ghcr.io/leejet/stable-diffusion.cpp:master-cuda` at digest
`sha256:8771c2d5…` (commit `c678dfe`), pipeline: Z-Image-Turbo Q4_K
(3.86 GB) + Flux `ae.safetensors` (335 MB, Comfy-Org mirror) + Qwen3-4B
Instruct Q4_K_M (2.5 GB), `--diffusion-fa --cfg-scale 1.0`. Every figure
below is one run, wall clock; VRAM is `nvidia-smi` total used, sampled at
4 Hz. Fixtures captured for the tests: the image's full `--help`
(`tests/fixtures/sdcpp/sd-server-help-c678dfe.txt`, 256 lines, three
option groups) and this pipeline's `capabilities`
(`tests/fixtures/sdcpp/capabilities-z-image-turbo-c678dfe.json`).

1. **Sources.** `black-forest-labs/FLUX.1-schnell` is gated (`401`);
   the identical VAE is un-gated under `Comfy-Org/z_image_turbo`. The
   diffusion GGUF and the text-encoder GGUF downloaded without a token.
2. **Listen address and health.** Without `--listen-ip 0.0.0.0` the
   server logs `listening on: http://127.0.0.1:8080` and the published
   port answers connection-reset from the host — confirmed required.
   With it, the port is reset for the first ~0.6 s, then answers. First
   run of `capabilities` answered **`500`, `Connection: close`,
   `EXCEPTION_WHAT: filesystem error: status: Operation not permitted
   [./proc/1/map_files/…]`** and kept doing so: the LoRA/upscaler scan
   walks the unset dir setting as the cwd. With `--lora-model-dir` and
   `--hires-upscalers-dir` pointing at (empty) directories it answers
   `200`. `/v1/models`, `/sdapi/v1/options` and `/sdapi/v1/samplers`
   were fine throughout. There is no 503 phase.
3. **Cold start and generation.** `podman run` returns in 0.26 s.
   Lazy default: `200` after **0.89 s**, 0.4 GiB in VRAM, weights uploaded
   inside the first request. `--eager-load`: `200` after **2.2 s** with
   6.6 GiB resident (the log: `loading tensors completed, taking 1.29s`).

   | request | first (lazy) | warm | eager first |
   |---|---|---|---|
   | 1024×1024, 8 steps | 6.3 s | **4.7 s** | 4.9 s |
   | 512×512, 4 steps | — | 0.60 s | — |
   | 256×256, 4 steps (the `local_model_test` shape) | — | 0.23 s | — |
   | with `--offload-to-cpu`, 1024², 8 steps | 8.9 s | 5.1 s | — |

   Same seed → byte-identical PNG (1 669 613 bytes) across lazy/eager and
   across processes. `capabilities` for this pipeline: `supported_modes:
   ["img_gen"]`, limits 64–4096 px, `max_batch_count 8`, `max_queue_size
   64`, 21 samplers, 18 schedulers.
4. **VRAM** (above the 0.8 GiB desktop baseline). Weights per the log:
   6 221 MB (text encoders 2 376, DiT 3 685, VAE 160). Idle resident after
   the first generation: **7.1 GiB**. Peak during 1024²: **13.7 GiB**;
   during 512²: 8.7 GiB; 256²: no measurable rise above idle. Compute
   buffers are freed after each job (idle returns to 7.1 GiB). With
   `--offload-to-cpu`: idle 1.0 GiB, peak 7.7 GiB. The file-size estimate
   (6.2 GB) is 12 % under the idle figure and 2.2× under the 1024² peak.
5. **SIGTERM.** Without `--init` (first run, idle): podman reported
   `StopSignal SIGTERM failed to stop container in 10 seconds, resorting
   to SIGKILL`. With `--init`: idle stop **0.30 s**, stop mid-generation
   **0.35 s**, both exit 143, VRAM back to baseline; the interrupted
   client saw a dropped connection with no status.
6. **Request/response shapes.** `{"prompt":""}` → `400
   {"error":"prompt required"}`; malformed JSON → **`500`**
   `{"error":"server_error","message":"[json.exception.parse_error.101]
   …"}`; `"model":"whatever"` ignored, `200`; `output_format: webp` +
   `output_compression` honoured (`output_format` echoed). Two concurrent
   1024² requests: 4.7 s and 9.3 s — strictly serialized, no error, no
   timeout. Native async: `POST img_gen` → `202 {id: "job_<hex>_<seq>",
   poll_url, status: queued}`; job went `generating` → `completed` with
   `result.images[{index, b64_json}]`; unknown id → `404`; cancel of a
   generating job → `409 {"error":"job is currently generating and cannot
   be interrupted yet"}` (cancel is queue-only in this build).
7. **Help and version need the GPU.** `podman run --rm --entrypoint
   /sd-server <image> -h` without `--device nvidia.com/gpu=all` fails with
   `error while loading shared libraries: libcuda.so.1`, exit 127. With
   the device: `-h` exits 0 (256 lines: Svr / Context / Default Generation
   options), `--version` prints `stable-diffusion.cpp version unknown,
   commit c678dfe`.
8. **Edits on a non-edit pipeline segfault.** `POST /v1/images/edits`
   with an image on Z-Image-Turbo: the log shows `Using 'z_image_omni'
   preset for reference images … EDIT mode … ZImageOmniPipeline`, then
   the process dies — container `exited`, **exit 139**, the client's
   connection dropped after ~4 s with no status. Reproduced twice. Native
   `img_gen` with `init_image` (plain img2img, no reference image) on the
   same pipeline completes normally (0.76 s at 512²), so it is the
   reference-image edit path, i.e. exactly what `/v1/images/edits` maps
   to, that a non-edit model cannot take.

What changed in the design because of this: `--init`, `--listen-ip`,
`--eager-load` and the two directory flags became unconditional (§3); the
health poll tolerates a 5xx and reports its `EXCEPTION_WHAT` (§3); the
`edit` column is a hard gate, not a hint (§4, §6); admission learns a
per-row transient peak (§9); recipes carry mirror choices and a `gated`
mark (§2.7, §7); the help probe runs with the class's device flags (§2.5).
What did not change: everything else in §2 held as read from the source.

## 13. Out of scope / follow-ups

- **Native async job routes** (`/v1/images/jobs…` → `/sdcpp/v1/img_gen`):
  wanted for cancel and progress, but a `202` ends the request while the
  job holds the GPU, so it needs a job ledger in lmgw that keeps the
  acquire guard alive until the terminal state and routes polls by job id
  → container. Design it with the job ledger, not before.
- **Video** (`vid_gen`): same server, same row (`modes`), needs the async
  routes above (a video job runs minutes) and a `/v1/videos`-shaped
  contract that no client standard has settled. The row schema is ready
  for it; the routes are not.
- `response_format: url`; `/sdapi/v1/*` compatibility; ControlNet /
  PhotoMaker / IP-Adapter inputs (reachable through `sd_cpp_extra_args`
  today, no lab widgets); LoRA management UI; ESRGAN upscale as a
  standalone route; Gemini/Imagen and Anthropic-dialect image generation
  (the Anthropic API has no image endpoint — `/v1/images/*` is
  OpenAI-dialect only, stated in `lmgw.endpoints`).
- Sharing a `--llm` encoder GGUF with the chat class.

## 14. Decisions taken in this draft — and what is open

Taken (overturn any of them in review):

- Class name `image`, prefix `image/`, engine `sdcpp`, upstream `sdcpp`
  (−4).
- Argv rendering, no config file; `files` and `args` as validated JSON
  maps rather than columns.
- OpenAI routes only in v1; sd.cpp's own `sd_cpp_extra_args` is the
  extension mechanism, lmgw adds none.
- `master-cuda` unpinned as the class default (same posture as the llama
  and audio defaults); pin per model by digest when a build matters.
- A shipped recipe list instead of a fetched catalog.

- `--eager-load`, `--init`, `--listen-ip 0.0.0.0`, `--lora-model-dir`,
  `--hires-upscalers-dir` rendered unconditionally (spike, §12).
- Admission keeps a learned per-row transient peak for this class (§9) —
  the one genuinely new mechanism in the spec.

Found while building WP1 (2026-09-21) — three corrections to the text above:

- **There is no registry "inspect tick"** (§3, "Death mid-request"). The
  registry rebuilds its map from `podman ps` at **boot only**
  (`registry::reconcile`, driven by `lifecycle::boot`); the 15 s tick is the
  idle reaper, and it reads the map rather than podman. What actually
  recovers a container that died mid-request is the **forwarding** path:
  `vram::LocalHold::recover` turns a transport error against the held
  endpoint into a forced `stop` (which drops the entry) plus a fresh
  `admit_local`, transparently, once. So the mechanism the design leans on
  exists — it is just one layer up, and WP2's `/v1/images/*` handler has to
  surface a dropped upstream connection as `GatewayError::Transport` for it
  to fire. An `exited (139)` container whose model is never requested again
  keeps its `ready` entry until the idle reaper's `stop` (a no-op on a dead
  container) drops it, and with `idle_seconds = 0` it keeps it until a
  restart.
- **The migration is two files, not one** (§10b): `0033_image.sql` creates
  `image_models` inside a transaction, `0034_hf_target_image.sql` rebuilds
  `hf_models` for the widened `target` CHECK outside one. sqlx honours
  `-- no-transaction` only as the **first line of a file**
  (`sql.starts_with`), so a combined file would have had to run the table
  creation unprotected too — and a failure halfway would leave the table
  created, the migration unrecorded, and every later start replaying a
  `CREATE TABLE` that now fails. (The two mid-file `-- no-transaction`
  markers inside `0013_audio.sql` are, for the same reason, dead text.)
- **`upstreams.kind` was not widened.** `sd_cpp` exists only on the
  synthetic −4 upstream, which is never persisted (0023), so the column's
  CHECK deliberately does not list it; `ops::parse_upstream_kind` refuses an
  owner-supplied `kind=sd_cpp` with a sentence instead of letting the INSERT
  fail against a constraint.

Found while building WP2 (2026-09-21) — three refinements of §5/§6:

- **A cloud `task` of `image_generation` needs images *instead of* text, not
  images as well** (§5, last bullet). `task` is what picks a model's
  `endpoints`, and an OpenRouter-shaped catalog gives a chat model that can
  also draw `output_modalities: [text, image]` — reading "contains image"
  would take `/v1/chat/completions` away from exactly the models people use
  it through. `catalog::parse_task_openai` therefore says `image_generation`
  only when the outputs contain `image` and no `text`, which is what §5's
  `output_modalities: [image]` already spelled.
- **The cloud route guard refuses only what a catalog positively denies.** A
  provider that publishes no modalities (or whose catalog cannot be reached)
  gets the request forwarded: absent means unknown is the whole contract of
  those lists, and refusing on a silence would lock out every
  OpenAI-compatible image provider that ships no metadata. The local half is
  the opposite and deliberately so — `edit` is a column lmgw owns, so it is a
  hard gate.
- **`/v1/images/edits` is multipart only.** Its audio sibling
  (`/v1/audio/transcriptions`) also accepts a JSON body naming a path inside
  the container; there is no such shape here — OpenAI's edits route is
  multipart, sd-server reads multipart, and a JSON variant would mean
  inventing a way to name image bytes that no upstream would understand. A
  JSON body is refused with the shape it should have had.

Found while building WP4 (2026-09-21) — four notes on §8's operator surfaces:

- **The MCP tool's `files` / `args` / `modes` are text, not nested JSON.**
  `mcp::selfadmin`'s own rule is that every argument is a flat scalar ("a tool
  whose parameters need hand-built JSON is a tool a small local model cannot
  call reliably"), and `capabilities_override` already bends to it by taking a
  JSON *string*. So `lmgw__image_model_set` takes `key = value` lines — the
  syntax an owner reads straight off `sd-server --help`, with a bare key
  meaning a switch — while `/api/op/image_model_set`, which the dashboard and
  WP5's editor post, sends the real JSON object. One `ops::ImageModelPatch`
  accepts both, the way `ArgList` already accepts a llama flag list as text or
  as tokens. A value that parses as a JSON number or boolean keeps its type,
  because `--cfg-scale "1.0"` and `--cfg-scale 1.0` are not the same argv.
- **`local_model_test target=image` dispatches through the real handler.** Not
  `vram::send_local` at the acquired endpoint, the way the chat and aux probes
  do, but `proxy::handle_image_generation` in process — which is what the Audio
  lab already does with its siblings (§8, "dispatched in-process through the
  real handlers so every call lands in Logs"). The test then inherits
  `resolve_image`'s guards, the error normalization and the log row for free,
  and what it measures is the route a client uses rather than the container
  behind it. Measured on this box against the real pipeline: **227 ms, 86 424
  bytes** for the 256²/4-step shape, on top of a 3.2 s cold start.
- **The test refuses a row that does not claim `img_gen`.** The only refusal
  §8's "generate one small image" needs beyond the route's own: a row whose
  `modes` say `vid_gen` and nothing else does not serve a still, and the same
  "operator says, lmgw sends only that" rule that makes `edit` a hard gate
  makes this one. Every other misuse is refused by the route itself, because
  the test goes through it.
- **`parse_class_target` accepts `image`, the downloader schemas still do
  not.** `lmgw__gguf_files`, `lmgw__hf_add` and `lmgw__hf_set` deliberately
  keep `chat|aux|audio` in their enums until WP3 teaches the downloader about
  `.safetensors` — `hf::TARGETS` already refuses the target, and advertising it
  would offer a path that writes into the wrong tree. `ops::gguf_files` would
  answer for the class today (WP1 wrote its `used_by` arm), it is just not
  offered.

Found while building WP3 (2026-09-21) — four notes on §7's downloader and
recipes:

- **§7.1's file-kind heuristics mis-file the whole Qwen-Image family.** The
  rule as written makes `qwen` a `text_encoder` marker and "everything else
  `.gguf`/`.safetensors` in a `-GGUF` repo" a `diffusion` one — and
  `QuantStack/Qwen-Image-GGUF/Qwen_Image-Q4_K_M.gguf` (the DiT) and
  `unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf`
  (Z-Image's `--llm`) satisfy *both* clauses. `ops::classify_image_file` is
  therefore a ladder rather than a list: directory evidence first
  (`split_files/vae/`, `text_encoders/`, `diffusion_models/`, `loras/` — the
  Comfy-Org layout states the role outright), then the unambiguous filename
  markers (`clip_l`, `t5xxl`, `_vae`, `esrgan`), then a **diffusion-family word
  in the name (`image`, `flux`, `diffusion`, `dit`, `unet`) beating the vendor
  word (`qwen`, `mistral`, `llm`)**, then §7's `-GGUF`-repo fallback, then a
  root-level `.safetensors`/`.ckpt` as the all-in-one checkpoint. Still
  heuristics, still labelled as such on every surface.
- **`hf_add` now refuses a file the target cannot load, by name.** The explicit
  `file=` branch only ever checked that the path was *in* the repo, which was
  harmless while every target was GGUF-only and is not once the kinds differ:
  `hf_add repo=Comfy-Org/z_image_turbo file=…/ae.safetensors` with no target
  would have written a `.safetensors` into the chat models dir, to be
  discovered as a missing GGUF at the first start. The refusal names the kinds
  the target accepts and points at `target=image`.
- **No shipped recipe is gated, so the gated path is correct but dormant.**
  §2.7 treats gating as a live constraint; checked against the hub on
  2026-09-21, every component of all six families has an un-gated source —
  `city96/FLUX.1-dev-gguf` and `QuantStack/FLUX.1-Kontext-dev-GGUF` re-quantize
  BFL's gated weights, `Comfy-Org/z_image_turbo` mirrors the FLUX
  `ae.safetensors`, and `stabilityai/stable-diffusion-xl-base-1.0` reports
  `gated: false` outright. The `gated` flag and `image_recipe_add`'s
  name-the-component refusal stay for the family that eventually has no mirror,
  with a test pinning that none needs a token today.
- **A recipe's `args` carry only what departs from sd-server's own defaults**,
  which its `--help` states: `--steps` 20, `--cfg-scale` 7.0, `-W`/`-H` 512,
  `--guidance` 3.5, `--scheduler` "model-specific", and `--sampling-method`
  *"euler for Flux/SD3/Wan, euler_a otherwise"*. So no recipe sets
  `sampling_method` even though §7.2 lists it as an example key — the server
  already picks `euler` for every flow-matching family here, and writing it
  down would be a constant lmgw has to keep in sync with sd.cpp for no gain.
  SDXL therefore carries nothing but its 1024² size, and only the
  guidance-distilled families carry `cfg_scale = 1.0`, where the default is
  actively wrong. The alternative — inventing a step count or a CFG for a
  family nobody measured — would have been a guess wearing a recipe's
  authority.

Found while building WP5's dashboard half (2026-09-21) — seven departures
from §8's one paragraph on the UI:

- **"Add from recipe beside Add from catalog" is a modal on Models, not a
  page.** "Add from catalog" turned out to have no route and no nav entry of
  its own — it is a `<Modal>` the audio section header opens
  (`pages/audio_catalog.rs`). `pages/image_recipes.rs` is its exact twin, so
  the recipes browser opens from the image section header and hands its
  prefilled row over through the page-level `Editors` context rather than
  through a route query. That is also what makes the handover trivial: the
  browser sets `editors.image`, and the one `ImageEditor` at the bottom of
  the page opens on it.
- **Apply is in the editor, start/stop/logs/test are in the row.** §8 lists
  all of them as row actions, but no class puts Apply in a table row: it
  lives in `ContainerStatusRow` inside each editor, which is where an owner
  who just changed a value is standing. Start/Stop/Logs *are* in the image
  row (the audio rows have neither) because an image row **is** its
  container's argv — every file and flag renders into it — so "what does it
  name, is it up, what did the start complain about" is one question.
- **The direct "open the server's own web UI" link stays on Overview.** The
  chat rows on Models have no such link; the Overview runtime table has it,
  and its filter was `class != "audio"`, so image containers were already
  covered. What Overview was missing is the *enabled-but-not-running* image
  rows, which it now lists like the other three classes.
- **`--type`'s value list is the one hardcoded string in the page.** The
  quant vocabulary (`f32 f16 bf16 q8_0 q5_0 q5_1 q4_0 q4_1 q4_k q5_k q6_k`)
  lives in sd-server's `--help`, which only `lmgw_core::sdcpp_caps` parses —
  nothing publishes it over `/api`. The select carries the documented set
  from the committed `c678dfe` help with a comment saying so, and a build
  that adds one stays reachable through the raw args JSON box, which merges
  last and wins over every widget.
- **The extra-args textarea goes over as text, not as one token per line.**
  `ImageModelPatch.extra_run_args` is an `ArgList`, whose text form is split
  with shell quoting (`argv::parse_args_text`) — which is what makes
  `--device nvidia.com/gpu=all` two argv elements instead of one
  26-character flag. The image editor follows the chat editor here; the
  audio editor's line-array form has the splitting bug and is out of this
  package's scope.
- **`RuntimeStatus` in `lmgw-api-types` gained `warnings[]` and
  `image_capabilities`.** The server has serialized both on the runtime
  frame since WP1 (`registry::RuntimeView`), but the client mirror named
  neither, so the warnings an image start reports were unreachable from the
  dashboard. `ImageCapabilities`/`ImageLimits`/`ImageFeatures`/`ImageAsset`
  are mirrored with it, tolerant and all-defaulting, which is also what the
  Image lab will read its sampler and scheduler lists from.
- **Two more class lists that hid `image`, both outside the UI crate.**
  `api_usage::series_identity`'s colour-slot list had four of the five
  request classes, so an image series drew with no stable slot; and
  `lmgw__usage`'s `class` enum refused a value `ops::usage` accepts. Both now
  read `chat|aux|audio|image|tool`, as the ops verb already did.

The **Image lab** route and nav entry are deliberately not stubbed: the
pattern is one `<Route>` line in `app.rs` beside `/audio-lab` and one
`<NavItem>` in `shell.rs`, and a placeholder page would only be something
the follow-up has to delete.

Found while building WP5's Image lab half (2026-09-21) — five notes on §2.3
and §8's one paragraph on the lab:

- **CFG is `sample_params.guidance.txt_cfg`, and `sample_params.cfg_scale` is
  ignored.** §2.3 lists `cfg_scale` among the `sample_params` keys; the native
  body has no such field. Measured on this box against `c678dfe`, same prompt,
  seed 42, 8 steps, 512²: the baseline PNG is 487 003 bytes,
  `guidance: {txt_cfg: 9.0}` yields a **different** image (351 853 bytes), and
  `cfg_scale: 9.0` yields the baseline **byte for byte** — a key the parser
  never read. The authority is the `defaults` object every container publishes
  on `GET /sdcpp/v1/capabilities`: it is the request struct's own
  serialization, so its spelling is the parser's. (`sample_method` was checked
  the same way and does take effect.) The lab writes `guidance.txt_cfg` and the
  CFG field's caption says so.
- **The page posts a form; the body is built on the server — from a builder
  both halves import.** The Audio lab builds its document in the page
  (`pages/audio_spec.rs`) and posts the finished thing. The Image lab posts
  `ImageGenForm` to `/image-lab/api/generate` and `web/image_lab.rs` builds the
  body, because the seam the tests have to pin is "what does the form become".
  But the builder itself lives in `lmgw-api-types::image_lab`, imported by both
  crates: the page's "Request" panel renders with the very function the server
  dispatches with, so the panel cannot drift from the wire. It is the one
  module in that crate with a function in it, and it earns that by being the
  contract rather than a view of one.
- **"Seed: blank = random" became "blank = the server's default".** sd-server's
  seed default is a fixed 42 and `-1` is its own spelling for "random each
  run"; a lab that silently sent a random seed for an empty field would make
  its own results unreproducible in a way the route is not — and would have to
  append an extension block to every request to do it. The field stays empty by
  default, a **Random** button fills a concrete reusable seed, and the tooltip
  names `-1`.
- **The edit tab is disabled, not hidden, and the pipeline's `ref_images` claim
  is printed as a claim.** A model that does not serve `/v1/images/edits` keeps
  a greyed tab with the reason beside it (the row's `edit` column for a local
  row, the catalog's input modalities for a cloud one) — hiding the tab would
  leave "can this model edit?" unanswered. The side panel prints
  `reports ref images: yes — not the gate` for exactly §12.8's trap: a pipeline
  that advertises reference images and then dies on one.
- **Results are `data:` URLs held for the session, with a visible Clear.** No
  object URLs (there is no right moment to revoke one that a download link and
  an `<img>` still share), no persistence (§8: the results live in the page),
  and no cap on how many runs are kept — the gallery is the count, and
  "Clear gallery" is the control. Each shot downloads as
  `<model>-<seed>-<i>.<ext>`, with the seed segment absent when the form did
  not set one, because there is no seed to name.

Found in review (2026-09-21) — what an adversarial read of the seven commits
changed, one line each (the nits are in the commit, not here):

- **`clear` is a list of names, not a haystack.** Every model patch matched
  `clear` with `contains`, so `clear=extra_run_args` emptied `args` as well
  (`"extra_run_args".contains("args")`). It is now split on commas and
  whitespace and compared whole, the image patch refuses a name it cannot
  clear, and `files` is refused by name — §4's "a row with no files names no
  pipeline" is a rule, so it needed a sentence rather than a silent wipe.
- **A key is judged under the spelling the image's own help declares.** The
  claimed-flag guard in `render_image_args` read the stored key while
  `flag_for` resolved aliases, so `args: {l: 127.0.0.1}` rendered a second
  `--listen-ip` and `files: {m: …}` slipped past the `model` /
  `diffusion_model` exclusivity. Everything that judges a key now resolves it
  through `SdcppCaps::resolve_key` first, and a collision with one of the
  three unconditional flags is a problem at **save** time rather than a value
  dropped at start.
- **A path in `args` is refused, naming `files`.** `args` values are passed
  through verbatim, so a path there points at a host path the container has
  not got — the existence check and the `/models` rewrite both live on the
  `files` side. Read off the help (`path to …`, `… directory`), not a list.
- **`<sd_cpp_extra_args>` in a field is refused, never stripped.** The block
  has no escaping (§2.3), so a prompt carrying either delimiter closes lmgw's
  block early or opens one of its own. `ImageGenForm` refuses the prompt, the
  negative prompt, the sampler, the scheduler and a LoRA path by name.
- **`..` in a `files` value cannot escape `models_dir` on the host.** The two
  default directory keys skipped the relative-path check, and `ensure_dirs`
  creates what they name — on the host. `dir_path_from` falls back to the
  class default and `file_target` refuses to join; the advisory sweep now
  checks every key, the save already did.
- **A read verb no longer creates directories.** `command_line_preview` and
  `image_model_get` render through `ModelRuntime::preview_spec`; so does
  adoption, which renders only to compare argv against a container that is
  already up. An unwritable or unset `models_dir` must not replace a rendered
  command line with an io error.
- **The usage plane knows upstream −4.** `UpstreamKind::SdCpp` counts as local
  and the synthetic id has a name, so image traffic is labelled `sdcpp` and
  priced free rather than logged as "upstream -4" and unpriced.
- **A text route refuses an image model before admission.**
  `/v1/chat/completions`, `/v1/completions`, `/v1/responses`, `/v1/embeddings`,
  `/v1/rerank` and `/v1/count_tokens` answer `image/<id>` with the same shape
  `resolve_image` refuses a chat model with. Resolution alone used to start the
  pipeline. The audio class has the same hole; it predates this work.
- **A 401 with a token says the token was rejected**, not that the repo is
  gated — "accept the licence" is advice for a page the owner already
  accepted.
- **Adoption keeps the capabilities it read.** For this class the readiness
  route *is* `GET /sdcpp/v1/capabilities`, so a warm-started container now
  publishes its samplers and limits instead of waiting for a restart.
- **`image_recipe_add` does not re-queue a component that is already
  downloading** (it flipped the row back to `queued` under the running job);
  it reports them as `already_queued`.
- **`/v1/models` notes name the real CFG key.** One sentence for the finding
  in the WP5 block above: CFG inside the extras block is
  `sample_params.guidance.txt_cfg`, and `cfg_scale` there is ignored. The
  example carries it now.
- **Quoted scalars keep their value, not their quotes** (`cfg_scale = "1.0"`
  stored `"1.0"` with quote bytes, which sd-server cannot parse), and a
  `files` value that is not a path is named in the VRAM note instead of
  silently charged 0.
- **Two corrections to the WP5 dashboard block above.** The `--type` value
  list is not `f32 f16 bf16 q8_0 q5_0 q5_1 q4_0 q4_1 q4_k q5_k q6_k`: the help
  states *examples* (`f32, f16, q4_0, q4_1, q5_0, q5_1, q8_0, q2_K, q3_K,
  q4_K`) and the k-quant names are **capitalized**, which sd.cpp matches
  exactly — the select carried values that would have failed a start. And the
  lab's Random seed button drew over an eyeballed `2.147e9`; it is `i32::MAX`
  now, because sd-server's seed is a C `int`.
- **`lmgw__llama_flags` refuses an image model id** instead of taking it for a
  container image and trying to pull it; `lmgw__model_inspect` offers
  `target=image`, which `modelinfo::class_of` has resolved since WP1.

Found while building WP7, the learned peak (2026-09-21) — §9's one genuinely
new mechanism, and five things the design did not know:

- **The baseline has two sides.** §9 says the peak is "the maximum NVML
  reading sampled while the model had a request in flight", and a maximum
  alone is not chargeable: the ledger already sees the idle residency in the
  driver's `used`, so what it is blind to is the *delta* above it. The
  baseline is the last reading taken while the row was `ready` and idle —
  and, for a window with no such reading before it, the first one after it,
  which is the same quantity read from the other side (the buffers are freed
  when the job ends, §12.4). That fallback is not a nicety: `acquire` hands
  its guard out the moment the container answers its readiness probe, so the
  **first** request after every start is such a window, and a pipeline with a
  short `idle_seconds` — started per request, reaped after it — would
  otherwise never learn anything at all.
- **The idle reaper's 15 s tick is the wrong cadence, measured.** The obvious
  design (sample fast while in flight, sleep on `REAP_INTERVAL` otherwise)
  was built and watched the *whole* first generation happen inside one sleep:
  a 3.2 s cold start plus a 1 s render, and the sampler woke to an idle
  pipeline having seen nothing. The three rates are now 50 ms while a
  generation runs, 250 ms while a pipeline is up and idle (the baseline
  half), and 1 s — a registry map read, no driver call — when no image
  container exists. 1 s is shorter than any sd-server start (2.2 s eager,
  §12.3), which is the only property that interval has to have.
- **20 Hz, because 4 Hz misses the short ones.** The spike's own rate is
  enough for the shape that motivated §9 — a 1024² render whose 13.7 GiB
  plateau lasts the whole job — and not for a 512² one: measured here, that
  rise came and went inside a single 250 ms gap on one run and was caught on
  the next. What the sampler produces is a **sampled maximum**, a lower bound
  on the true transient, never an interpolation; a missed spike leaves the
  row with what it had, which is also why the figure is a running maximum
  rather than the last measurement.
- **A window shared with another start teaches nothing.** NVML answers per
  device, not per process (§13 keeps it that way), so a chat model loading
  beside a generation is indistinguishable from a generation allocating
  6 GiB. The window records the registry's shape when it opens and is
  abandoned if that changes — one lost generation instead of a wrong figure
  charged forever. Two image rows in flight at once is the case that cannot
  be split, and there the whole delta is attributed to each: over-charging is
  the safe direction.
- **Measured on this box while building it** (Z-Image-Turbo Q4_K, the §12
  pipeline): 256² teaches **418 MiB** — the size §12.4 called "no measurable
  rise above idle" at 4 Hz — and 512² teaches **1.7 GiB**, which is §12.4's
  8.7 GiB peak against its 7.1 GiB idle. `tests/it/image_live.rs` renders all
  three and prints the figure.
- The charge is on *residency*, not on the row's own admission: a `ready`
  image pipeline reserves its learned peak against everything else, and its
  own start is still sized at the file estimate. Charging its own start would
  refuse, on a card that fits the pipeline and not the pipeline plus its
  buffers, the very first generation — the one that teaches the figure.
  `estimated_resident_bytes` stays a residency too: it is the number a
  surface compares against the driver's `used`, so the peaks are subtracted
  from free in every capacity branch instead of being folded into it.

Not changed, deliberately: the 600 s synthetic-upstream timeout (shared by all
four classes, and it surfaces as `GatewayError::Timeout` rather than a hang),
and the unauthenticated `/image-lab/api/*` (the chat and audio labs are the
same, and the dashboard's own auth is the boundary).

Open:

- ~~The settings DTO's new `image` section is a contract change under
  `deny_unknown_fields`, and §11 puts its release note in WP6 — but this repo
  has **no release-notes file at all**: the per-model-containers §6 note was
  never written either. WP6 has to create the place before it can write the
  note (README's "Releases & updates" section, or a new
  `docs/release-notes.md`).~~ Settled in WP6 (2026-09-21): `docs/release-notes.md`
  is the place, newest first, contract changes only, linked from README's
  "Releases & updates".
- Nothing blocking WP1. The spike weights stay under
  `~/.local/share/lmgw/sdcpp/` (the future image `models_dir` — the
  spike used the `<owner>/<repo>/<file>` layout the downloader will use,
  so the first real row can point at them).
