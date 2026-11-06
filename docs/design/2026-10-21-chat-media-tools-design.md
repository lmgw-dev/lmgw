# Image and audio tools for the Chat — Design

**Date:** 2026-10-21
**Status:** draft; slices 1 to 4 built 2026-10-21 (slice 3 with slice 4's UI
half), and the default models of §3.8 the same day. The
owner's decisions of 2026-10-21 are recorded below as such, with the answers to
§12's questions; everything under §12 "Taken in this draft" is the drafter's call
open to review. Where building changed what the draft said, the
text says what was built (§14 lists those changes).

> Companion to [2026-09-21-image-generation-design.md](2026-09-21-image-generation-design.md)
> ("image §n" below), [2026-10-06-llama-egress-design.md](2026-10-06-llama-egress-design.md)
> (§8, tool-result images), [2026-09-30-chat-complete-design.md](2026-09-30-chat-complete-design.md)
> (§9.4, the `kb__*` toolset this one copies), [2026-09-25-chat-archive-pin-attachments-design.md](2026-09-25-chat-archive-pin-attachments-design.md)
> (§2, attachments) and [2026-10-04-voice-audio-input-design.md](2026-10-04-voice-audio-input-design.md)
> (§3.4, letting the loop's GPU claim go).

The chat model calls lmgw's own image rows (stable-diffusion.cpp `sd-server`) and
audio rows (TTS, ASR) as tools: draw a picture, edit an attached one, upscale it,
speak a text, transcribe a recording. On one card this is a sequential handoff —
the chat model out, the image model in, the chat model back — which only a gateway
that owns the GPU can do. The tools are a built-in toolset on `/mcp`, gated by
`ToolScope` like every tool there, and attachable to Chat threads and folders by
label like `kb` and `docs`.

**Today** (lines at `main` 2fb70b07):
- Three built-in toolsets exist: `lmgw__*` (self-admin, `/mcp/admin`), `docs__*`
  (quickdoc) and `kb__*` (`mcp/kb.rs`: `owns`, a static `list`, `call`). Their
  namespaces are reserved in one list, `RESERVED_NAMESPACES` (`mcp/mod.rs:433`);
  their labels resolve in `mcp/exec.rs` (`builtin_label` :566, `resolve_builtin`
  :617) and are listed in `exec/labels.rs` (`BUILTINS` :181), `exec/served_by.rs`,
  `mcp/inventory.rs`, `agents/mod.rs:219` and `web/chat_tool_write.rs:227`.
- Every built-in executor flattens its result to one text block (`builtin_text`,
  `mcp/exec.rs:1186`). The IR can carry `ToolResultBlock::Image` / `Audio`
  (`ir.rs:125`), and `gate/tool_images.rs` decides per send whether a tool image
  may reach a llama.cpp model (llama egress §8.2).
- The northbound `/mcp` answers every POST with one JSON body and acknowledges
  every notification with `202` and nothing else (`mcp/ingress.rs:684`): there is
  no `notifications/progress` and no `notifications/cancelled` handling.
  `ToolPlane::call_tool` (`ingress.rs:169`) takes a name and arguments, nothing
  else.
- The Chat's tool loop (`web/agentchat.rs::run_send`) holds one GPU claim for the
  run (`agentchat/claim.rs::LoopClaim`). It lets the claim go only while a heard
  voice turn's first tool call waits for its user row (`agentchat/heard.rs`); the
  next model call takes it again (`LetGo::regain`). A turn's calls run in
  parallel when `parallel_tool_calls` allows (`agent.rs::execute_approved`), and
  a turn's stop drops them where they stand (`Cancel::guard`).
- `ToolExecutor::call` (`agent.rs:242`) has no progress channel. The loop knows a
  call's id through a task-local (`agent.rs` `CALL_ID`).
- Image: `/v1/images/generations|edits` are synchronous passthroughs
  (`proxy/image.rs`), parameters beyond the OpenAI body go through
  `<sd_cpp_extra_args>`. The Image lab dispatches in process through those
  handlers and gets a `Response` back (`web/image_lab/dispatch.rs`); there is no
  in-process call that returns bytes. lmgw has no code for sd-server's native job
  API, previews, cancel or upscale.
- Audio: `proxy/synthesize.rs` (`open`, `Synthesis::speak`, `finish`) and
  `proxy/transcribe.rs` (`transcribe_for`) are in-process calls that return bytes
  and a string. CPU rows (`backend: cpu`) are `Placement::Cpu`: no VRAM, never
  evicted, served under the GPU hold.
- VRAM: `vram/queue.rs` keeps a free-text `stage` per waiter ("needs X, Y free —
  evicting", "starting the container") and publishes it only in the global
  `vram` telemetry view; the stage does not name the model being evicted, and
  nothing lets one request follow its own waiter. The Chat's `state` frame
  (`ModelState`, `lmgw-api-types/src/chat_voice.rs:38`) reports only the chat
  model's cold admission (`web/chat_voice/turn_state.rs::admit_reporting`).
- Attachments (`chat_attachments`, migrations 0038 and 0051): kinds `image`,
  `text`, `pdf`, `office`, `audio`; a draft (`message_id IS NULL`) is bound to the
  next **user** message (`store/chat_attachments.rs::send_user_message_by` :363).
  Nothing binds an attachment to an assistant reply (`ChatReply`,
  `store/chat_messages.rs:40`), although the `Message` DTO already carries
  `attachments` for every role. An image goes to the model as a bare
  `ContentPart::Image` (`web/chat_attach_render.rs::image_part` :81), with no id
  or name the model could pass back.
- UI: `tools_from_ir` reduces image and audio result blocks to `[image]` /
  `[audio]` (`lmgw-ui/src/pages/chat.rs:493`); the Chat has no `<audio>` element.
  The dashboard's CSP already allows `media-src 'self' data: blob:`
  (`web/ui.rs:97`).

## Decisions

*Owner, 2026-10-21:*
1. **One tool per action, not per row:** `image__generate`, `image__edit`,
   `image__upscale` (as a tool where it fits; it is also a card action),
   `audio__speak`, `audio__transcribe`, each with a `model` parameter whose enum
   is the eligible rows — and, answered with §12's questions, the cloud image,
   TTS and ASR aliases that serve the route. The description lists every row
   with its family's prompt style (natural language for Flux-like, tags for
   SDXL-like, …). Where the prompt style lives is this spec's to say (§3.4).
2. **No new residency policy.** Before an image call the loop's claim lets go
   (`LocalHold::let_go` / `LetGo::regain`, as `HeardTools` does for ASR); the
   image row loads, stays resident per its `idle_seconds`, and is LRU-evicted
   when the chat model needs the room back. CPU audio rows need no swap.
3. **Slot save and restore is deferred** to its own `docs/ideas.md` entry (§13).
4. **Previews and progress use sd-server's native async job API**
   (`POST /sdcpp/v1/img_gen`, `GET /sdcpp/v1/jobs/{id}`, `preview` +
   `preview_interval`). The tool call holds the GPU claim while it polls, so the
   general job ledger image §13 deferred is **not** needed here; async routes for
   API clients stay a follow-up. The Chat shows the latest preview frame in place
   with "step n / m" (and the pass), replaced by the final image, plus the swap
   stages (unloading X, loading Y, generating). `/mcp` callers get
   `notifications/progress` when they sent a `progressToken`. Preview mode has a
   sane default and is a per-row setting with human labels.
5. **Cancel** on a running generation and on the whole turn: sd-server's cancel
   when the job is queued or the build reports `cancel_generating`; otherwise lmgw
   stops the row's container (0.35 s in the spike, image §12.5) and the next call
   reloads it. The UI says plainly that cancelling restarts the image model.
   `/mcp`'s `notifications/cancelled` maps to the same.
6. **Extras in this build:** an image card under each produced image (model, seed,
   steps, size, time; actions re-roll, vary/edit-this, upscale via
   `POST /sdcpp/v1/upscale`, save — run directly, no model round-trip); flat,
   optional, model-settable `seed`, `steps`, `cfg`, `negative_prompt`, `size` and
   img2img `strength`. Video is not in this build (§13).
7. **Under the GPU hold:** the row's own fallback alias if one is configured
   (fallbacks always apply, no content-based exceptions), the hold error otherwise.
   No new local-only guards.

## 1. Summary

A fourth built-in toolset, `mcp/media.rs`, serves two labels, `image` and `audio`,
whose tools are listed from the snapshot (the enabled rows and the aliases that
can do the action) rather than from a static table. Each call runs **in process** through the same
gate every `/v1` request takes — key policy, hold swap, route guard, admission,
one `request_logs` row — and returns bytes:

- image calls on a **local** row go to sd-server's native job API and poll it,
  which is what gives previews, step progress and a real cancel (§5); a cloud
  route (a hold fallback) goes through the OpenAI route synchronously (§4.1);
- audio calls go through `proxy::synthesize` and `proxy::transcribe`, which
  already return bytes (§4.2).

In a Chat thread the produced files become `chat_attachments` rows bound to the
reply (`source = 'tool'`), the tool result names them `att:N`, and the model can
pass that ref to the next call (`image__edit image=att:57`). The turn record keeps
the ref, not the bytes, and every send turns it back into an image for the model
that answers (§6.4). The loop lets its GPU claim go before an image call that lands
on the GPU (§4.3), and the Chat shows the swap, the preview and a Cancel (§8).

## 2. sd-server's native API — what this builds on

Read from upstream `examples/server/api.md` (master, fetched 2026-10-21) and held
against the build lmgw last captured (`tests/fixtures/sdcpp/*-c678dfe*`):

| Fact | Upstream master | c678dfe fixture |
|---|---|---|
| `POST /sdcpp/v1/img_gen` → `202 {id, kind, status, created, poll_url}`; `429` when the queue is full | yes | yes (spike, image §12.6) |
| `GET /sdcpp/v1/jobs/{id}` → `{status: queued\|generating\|completed\|failed\|cancelled, queue_position, started, completed (ms), result.images[{index,b64_json}], error{code,message}, preview}`; `404` unknown, `410` gone | yes | yes, no `preview` |
| `preview` request field `none\|proj\|tae\|vae`, `preview_interval` (steps, default 1, ≤ 0 clamped to 1) | yes | absent from the help and the capabilities |
| job `preview.{pass, step, total_steps, b64_json}`, latest frame only; `pass` restarts `step` per batch item, high/low-noise stage or hires pass | yes | no |
| capabilities `preview_modes`, `features_by_mode.img_gen.preview` | yes | absent |
| `features_by_mode.img_gen.cancel_queued` / `cancel_generating` | yes | `true` / `false` (cancel of a generating job answered `409`, image §12.6) |
| `POST /sdcpp/v1/upscale {image, upscaler?, repeats 1–4, tile_size?, output_format?}` → `{images[{b64_json}], upscaler, scale, repeats, width, height}`, synchronous, holds the context lock; output ≤ `limits.max_upscale_width/height` (8192) | yes | absent (no `upscale` in capabilities) |
| `upscalers[].image_upscale`, top-level `upscale` boolean; model-backed upscalers scanned from the top level of `--hires-upscalers-dir` only | yes | `upscalers` without `image_upscale` |

Three upstream facts shape choices below:

- **A loaded TAE decodes the final image too.** "A selected TAE is used for
  encoding and decoding, including when a full VAE is also selected" (server
  README). The `tae` preview mode is therefore not free: it needs a TAE file in
  the row's `files`, and that file then lowers the final image's quality (§5.2).
- **Upscale is an ESRGAN model in the upscalers directory**, scanned at the top
  level only. lmgw renders `--hires-upscalers-dir /models/upscalers` on every
  image row (image §3), so one file there serves every row; the downloader's
  `<owner>/<repo>/<file>` layout does not put a file there (§9, slice 4).
- **sd-server gained model management** (`/sdcpp/v1/models/load|unload`,
  recipes). Command-line preloading, which is what lmgw renders, stays supported
  ("starting with the existing model arguments preserves command-line
  preloading"), so one pipeline per container stands; nothing here uses the new
  routes.

The class default image is `ghcr.io/leejet/stable-diffusion.cpp:master-cuda`,
unpinned (image §14), so the running build is whatever was last pulled. **Slice 1
starts by confirming it**: `--version`'s commit, a fresh capabilities fixture for
the spike pipeline (`capabilities-z-image-turbo-<commit>.json`) and help text
fixture, and whether `preview_modes`, `features_by_mode.img_gen.preview`,
`cancel_generating` and `upscale` are there. Every path below reads those fields
from the probed capabilities and falls back when they are absent; none assumes
them.

**Confirmed 2026-10-21** on a dev instance with the locally built
`localhost/lmgw-sd-server:sdcpp-master` (`stable-diffusion.cpp version b982,
commit 0518b34`), the spike pipeline at 512², three generations
(`tests/fixtures/sdcpp/{capabilities,jobs}-z-image-turbo-0518b34.json`,
`sd-server-help-0518b34.txt`):

- `preview_modes` is `[none, proj, tae, vae]` and `features_by_mode.img_gen.preview`
  is `true`, with no TAE file in the row (the runtime recipe reports
  `preview_tae: true` too). `proj` frames are the latent's size, 64×64 PNG for a
  512² picture (about 6.6 KB each); `vae` frames are full size, 512×512.
- `cancel_queued` `true`, `cancel_generating` still `false`: a cancel of a
  generating job answers `409 {"error": "job is currently generating and cannot
  be interrupted yet"}` and the job completes. An unknown id answers `404 {"error":
  "job not found"}` for both the poll and the cancel.
- `upscale` is `false` with an empty `upscalers/`; `upscalers[]` now carries
  `image_upscale` (`false` for every built-in); `limits` names
  `max_upscale_width/height` 8192 and `max_queue_size` 64.
- The job is `generating` from the first poll (`started` equals `created` on an
  idle server, `queue_position` 0), carries `preview {pass, step, total_steps,
  b64_json}` while it runs, and drops `preview` once `completed`.
- Timings on the 24 GiB card: the container up in 2.6 s (weights in the page
  cache, `runtime.load_elapsed_seconds` 2.1); a 512², 8-step job 1.05 s from
  `started` to `completed` with `proj` previews, 1.04 s without, 1.79 s with `vae`
  previews.

**Slice 2's live check** (2026-10-21, the same build, `scripts/media-tools-live.sh`
on a dev instance, four generations): `proj` frames are 64×64 for a 512² picture
and 128×128 for a 1024² one (an eighth of the side); `vae` frames are full size
(512×512). Polled at 250 ms, a 512² job showed steps 1, 4, 7 of 8 with `proj`
and 2, 3, 5, 6, 7 with `vae` (1.1 s and 1.8 s drawing). The capabilities read
the same as the committed fixture (`cancel_generating` still `false`). A 1024²
job cancelled at step 2 took the restart path: the turn's result arrived 0.04 s
after the cancel route answered `restart`, and the card's memory was back at its
idle baseline 0.27 s after it.

## 3. The toolset

### 3.1 Names, labels, namespaces

| Tool | Label | What it does |
|---|---|---|
| `image__generate` | `image` | a new picture from a prompt |
| `image__edit` | `image` | a picture changed as the prompt says, by a model that edits (§3.2, §3.3) |
| `image__upscale` | `image` | an ESRGAN upscale of a picture (slice 4) |
| `audio__speak` | `audio` | speech from text |
| `audio__transcribe` | `audio` | text from a recording |

- `RESERVED_NAMESPACES` grows to five: `image__` and `audio__` join `lmgw__`,
  `docs__`, `kb__`, with the same two guards (`ops` refuses the prefix at
  configuration time; `build_aggregate` moves a southbound tool out of it under
  its server's name, or skips it with a `SkippedTool` naming why). A registered
  server whose prefix is `image` or `audio` today is handled by that backstop:
  its tools move under its server name, visibly, and its next save asks for
  another prefix.
- `builtin_label` and `exec/labels.rs::BUILTINS` gain `image` ("Draw, edit and
  upscale pictures with this gateway's image models") and `audio` ("Speak text
  and transcribe recordings with this gateway's audio models").
  `exec/served_by.rs` names them `lmgw (image)` and `lmgw (audio)` on log rows.
  `mcp/inventory.rs` lists them as `builtin` sources, so the tool picker
  (`lmgw-ui/src/widgets/tool_picker.rs`) and the Tools page show them with no UI
  change. `agents/mod.rs` and `web/chat_tool_write.rs` accept the labels like
  `docs` and `kb`.
- **On `/mcp`**: `AggregatePlane::list_tools` adds `media::list(&snap)` beside
  `docs::list()` and `kb::list()`; `call_tool` routes `media::owns(name)` after
  the scope and the owner's per-tool switch, exactly as `kb__*`. A client key's
  tool scope takes `image__*` / `audio__*` like any prefix.
- **Attached by label** to a Chat thread or folder (the thread's `mcp_tools`
  entries, resolved by `resolve_attached`), to a `/v1/responses` request, to an
  agent manifest and to a realtime session — every `SplitExecutor` user
  (`responses.rs:863`, `agents/batch.rs:638`, `realtime/responder/tools.rs:352`,
  `web/agentchat.rs:912`). Only the Chat has a thread to store files in; the
  others get the thread-less executor of §3.6.

### 3.2 Eligible rows

The `model` enum is computed on every `tools/list` and every turn's resolve from
the snapshot; values are public names (`image/<id>`, `audio/<id>`, an alias), so
the gate resolves them like any alias. Rows come first, then aliases.

| Tool | Rows | Aliases |
|---|---|---|
| `image__generate` | enabled image rows whose modes (empty = `IMAGE_MODE_DEFAULT`, `img_gen`) contain `img_gen` | enabled aliases publishing `/v1/images/generations` |
| `image__edit` | of those rows only the ones whose `edit` column is set: an **instruction edit** (§3.3, §14 item 76) | enabled aliases publishing `/v1/images/edits` |
| `image__upscale` | offered when the image models dir's `upscalers/` holds at least one file; its `upscaler` enum is their stems (§9) | none: upscaling needs a local row |
| `audio__speak` | enabled audio rows with `task = tts` | enabled aliases publishing `/v1/audio/speech` |
| `audio__transcribe` | enabled audio rows with `task = asr` | enabled aliases publishing `/v1/audio/transcriptions` |

**The enums and descriptions depend on configuration only**, so the tool block a
chat model is sent stays the same from turn to turn until the owner changes
something; a block that changed would change the prompt prefix and throw away a
local chat model's prompt cache. A cloud alias's endpoints are the ones its
configuration publishes — its model's name (`task::by_name`) under its
`capabilities_override` (`capabilities::exposed::configured_endpoints`), what
`GET /v1/models` publishes for it whenever its catalog states no task — never
whether some other look happened to read its catalog; a tool list waits on no
provider and starts no catalog read. An alias onto one of lmgw's own upstream
kinds publishes its backing row's. A cloud image model a catalog states publishes
no route (decision-models design §6.2), so a cloud image alias joins the enum
through an owner's `capabilities_override` naming its task. An image row's
defaults and size range are its container's probed capabilities, remembered in
memory after the container stops (`Registry::image_capabilities`): a row reads
the same from its first start on, and before any start its line has no probed
facts yet. An alias's line says
it is a cloud model, via which upstream, and which parameters it does not take
(§12, decision 16).

**Only models that edit are `image__edit`'s** (`mcp/media/edit_rows.rs`, §14
item 76). A row without `edit` takes a picture only as an img2img start
(`init_image` + `strength`), which keeps the picture's composition by
construction: asked to add something, it hands back nearly the same picture.
So such a row is not in the enum, `image__edit` is not listed when no row and
no alias edits (and `image__generate`'s description then says that none of its
models can edit, instead of pointing at `image__edit`), and a call naming such a
row anyway is refused before anything runs: "model 'image/z-image-turbo' cannot
edit pictures: it only draws new ones from a prompt, and given a picture it
would hand back nearly the same one. The models that can edit: …" (or "No model
on this gateway can edit pictures: add an edit model under Models → Image …").
`proxy::image_call` refuses an edit on such a row too (`not_an_edit_model`),
whatever reached it. A variation is the card's **Vary** (§8.2); the tools have no
variation tool yet.

A tool with no eligible row is not listed. A label whose tools are all unlisted
resolves to the existing "offers nothing" failure, with a sentence per label
("no enabled image row serves generations — add one under Models → Image") instead
of the self-admin wording `resolve_builtin` uses today. The list changes when a
row does: `ops::image_model_set` and the audio row writes call
`McpManager::notify_tools_changed()`, which sends `/mcp` sessions
`notifications/tools/list_changed`.

### 3.3 Schemas

Flat scalars only (`mcp::selfadmin`'s rule: a small local model cannot build
nested JSON reliably). Every parameter but `model`, `prompt`/`text` and the input
ref is optional; an omitted one is the row's own default, never an lmgw constant.

```json
{"name": "image__generate", "inputSchema": {"type": "object", "properties": {
  "model":           {"type": "string", "enum": ["image/z-image-turbo", "image/sdxl-base"]},
  "prompt":          {"type": "string", "description": "What to draw, in the style the chosen model reads (see above)."},
  "negative_prompt": {"type": "string", "description": "What to keep out of the picture."},
  "size":            {"type": "string", "description": "WIDTHxHEIGHT in pixels, e.g. 1024x768. Omit for the model's default."},
  "seed":            {"type": "integer", "description": "The same seed with the same settings gives the same picture. Omit for a new one."},
  "steps":           {"type": "integer", "description": "Sampling steps. Omit for the model's default."},
  "cfg":             {"type": "number", "description": "How strictly the picture follows the prompt. Omit for the model's default."}
}, "required": ["model", "prompt"], "additionalProperties": false}}
```

`image__edit` is `image__generate` plus `"image": {"type": "string",
"description": "The picture to change: att:N from this conversation, or a data:
URL."}` (required); its `prompt` says what to change. On an edit row the picture
goes as `ref_images[0]`; a cloud alias gets it on its edits route. It has no
`strength` (an earlier pass had one, for the variation rows it no longer offers,
§14 item 76). `ref_images` is never sent to a row without `edit`: sd-server
segfaults on it (image §12.8), and the body builder (§4.1) is where that is
enforced, with a test. A variation (the card's Vary) sends the picture as
`init_image`, with `strength` when one is set, and its `prompt` describes the
whole result.

`image__upscale`: `image` (required, as above), `upscaler` (enum of the stems,
optional: sd-server takes the first compatible one), `repeats` (`integer`,
`minimum` 1, `maximum` 4 — sd-server's own bound, stated in the schema; the
description adds that the result may not exceed the build's
`limits.max_upscale_width/height` when the capabilities name them). It has no
`model`: which row runs it is §9's rule, among the rows the caller's key may
use (`proxy::image_call::upscale`).

`audio__speak`: `model`, `text` (required), `voice` (string; the description lists
each row's voices and its default), `instructions` (string; only rows that take
them, said per row), `language` (string; passed where the row reads one,
`SpeechLanguage::for_row`, and named as ignored otherwise).
`audio__transcribe`: `model`, `audio` (required: `att:N` or a `data:` URL),
`language` (optional).

The `model` parameter is required even when the enum has one entry: the schema
stays the same shape whatever the row count, and one choice costs a small model
nothing. The one exception is a default model (§3.8): while one resolves for
the call, `model` leaves `required` and the description names the default.

### 3.4 Descriptions and prompt style

Each tool's description is one paragraph on what the tool does and what it costs,
then one line per eligible row, built from facts lmgw has:

```
Draw a new picture with one of this gateway's image models. The user sees it at
once; the result names it as att:N, which image__edit and image__upscale take.
A picture loads its image model onto the GPU and can unload the chat model
meanwhile, so make the pictures you were asked for, not extra rounds.
- image/z-image-turbo (Z-Image Turbo): write the prompt as plain sentences.
  Defaults: 1024x1024, 8 steps, cfg 1 (negative_prompt has no effect at cfg 1).
  Sizes 64–4096 px a side.
- image/sdxl-base (SDXL): write short comma-separated phrases. Defaults:
  1024x1024, 20 steps, cfg 7.
```

The defaults come from the row's `args`, else the probed capabilities' `defaults`
(the request struct's own serialization, image §14), else are left out; the size
range only once a capabilities probe exists. The cfg-1 note is derived: a row
whose effective cfg is 1.0 ignores the negative prompt.

**Prompt style** has no home today. It gets two:

- `ImageRecipe.prompt_style: PromptStyle` in `image_recipes.rs`, one value per
  shipped family: `natural` for Z-Image, Z-Image Turbo, FLUX.1 schnell/dev/Kontext,
  FLUX.2 klein/dev, Chroma1 HD, SD 3.5 Large, Qwen-Image, Qwen-Image 2.1,
  Qwen-Image-Edit 2509 and Wan 2.1; `keywords` for SDXL base. `tags` (booru-style
  tags) is for the anime fine-tunes a hand-made row brings; no shipped recipe has
  it yet.
- `image_models.prompt_style` (nullable, migration 0085). `NULL` means derived: the
  recipe `image_recipes::match_path` finds for the row's `model` or
  `diffusion_model` file. A row with neither a column value nor a recipe match has
  no prompt style, and its line says "prompt style not set: describe the picture in
  plain sentences"; the image editor shows the field empty with "Not set — the
  chat model writes plain sentences until you pick one".

`PromptStyle` lives in `lmgw-api-types/src/chat_media.rs` (new) with the labels
both crates use: `natural` "Plain sentences", `keywords` "Short keyword phrases",
`tags` "Booru-style tags". `lmgw__image_model_set` gains `prompt_style` (one of the
three, or `clear`).

Audio rows list their voices (`audio::voices::row_speech`: the presets, the
voices the package ships, the voice library; no request) and the default one,
whether they take instructions and a language, and for ASR the languages the
row's spec names where it names any. Every voice is listed: the list is the
row's own, not cut to a count.

### 3.5 Results

| Tool | In a Chat thread | On `/mcp` and thread-less callers |
|---|---|---|
| `image__*` | text + `Resource{uri: "lmgw:attachment/57"}` per image (rehydrated to an image block per send, §6.4) | text + MCP `image` content (`image/png`, base64) |
| `audio__speak` | text only; the WAV is stored for the player | text + MCP `audio` content (`audio/wav`) |
| `audio__transcribe` | the transcript | the transcript |

The text says what happened in words a model can relay:
`Made image att:57 (1024x1024) with image/z-image-turbo: seed 1730094420, 8 steps,
cfg 1, 4.7 s (1.1 s of it drawing). The user sees it.` — the job's own time is the
drawing, the rest admission and loading. Without a thread: `Made an image
(1024x1024) with …. It is in this result.` Under a fallback: `… with
image-cloud.example, the fallback for image/z-image-turbo while the GPU hold is
on: 4.7 s; seed and steps were not applied.` — naming only the parameters the
call set. A refusal is an `isError` result with the gateway's own message (the
hold, a key's policy, sd-server's error).

The chat model gets no audio back from `audio__speak`: a chat model that hears
audio in a tool result is rare, and the bytes would cost its context for nothing.
`/v1/responses` attaches the toolsets like any label; its `mcp_call` output is a
string, so a Responses client gets the text with the image left out — the bytes
are on `/mcp` and `/v1/images/*`.

### 3.6 Input refs

A model cannot pass bytes, so a Chat thread's files get a ref, **`att:N`**, `N`
being the `chat_attachments` id:

- **Shown to the model.** `chat_attach_render::render` puts one text part before
  each image and audio attachment: `[attachment att:42: image "cat.png"]`. Always,
  not only when a media toolset is attached, so a thread's replay bytes do not
  change when a label is toggled (they change once, when this lands). Produced
  files are named by their tool result.
- **Resolved by the call**, never trusted: the ref must name an attachment of the
  turn's own thread (`ChatRepo::attachment_reachable` and the thread id), of kind
  `image` for `image__*` and `audio` for `audio__transcribe`. Anything else is an
  `isError` result naming the refs the thread has. A bare number is read as a ref.
- `data:` URLs are accepted everywhere; `http(s)` URLs are refused ("lmgw does not
  fetch URLs for tools"), as `{"type":"mcp"}` labels refuse server URLs.
- **Thread-less callers** (`/mcp`, `/v1/responses`, agents, realtime) have no
  thread: `att:N` is refused with "refs name a Chat thread's files; pass a data:
  URL here".

### 3.7 Policy, charging, logs

Each call is two kinds of row, as a `kb__search` is: the tool row
(`record_tool_call`, proto `chat` or `mcp`, server `lmgw (image)`) and the model
row the in-process call writes (class `image` or `audio`, `images_out` /
`chars_in` / audio seconds as billable-units counts them). The model row carries
the run's model label: `chat` in a Chat thread and `admin` in an Admin Chat
thread, as the thread's model turns, voice and card actions do (§14 items 10 and
62), while the tool row is `chat-tool` in either, as the thread's other tools'
are. The model row is
charged to the caller: a device's key in a device's turn, the `/mcp` request's
key. That key's policy (model scope, budget, rate) is checked for the row's alias
before admission, as `/v1/images/generations` checks it (`policy_or_refuse`); a
refusal is the call's `isError` result. The key's model scope also narrows each
tool's `model` enum (`Offer::within`, `policy::check_scope`) on `/mcp`, in a
label's resolve and in a device's turn, so a scoped key is never offered a model
its call would be refused; the gateway's own runs and the owner see every row.

### 3.8 Default models

*Owner's request, 2026-10-21; the decisions below are the owner's unless marked
as the builder's call in §14.* A chat model that must pick a `model` every call
picks badly or inconsistently, so each tool has a **default model** the owner
chooses, which a call that leaves out `model` uses.

**Three levels**, the narrowest that says something winning — **thread >
folder > Settings**:

| Level | Where it is set | Stored as | Empty means |
|---|---|---|---|
| Settings → Chat → Picture and sound tools | the settings page; `lmgw__settings_set chat_media_defaults` | `chat_media_defaults` in the settings JSON | no default: the model chooses |
| a Chat folder | the folder's settings form | `media_defaults` in the folder's `defaults` JSON | absent: inherit; `""`: no default |
| a Chat thread | the thread's settings drawer | `chat_threads.media_defaults` (JSON, migration 0087) | absent: inherit; `""`: no default |

Each level is one `MediaDefaults` (`lmgw-api-types/src/chat_media/defaults.rs`):
`generate` (`image__generate`), `edit` (`image__edit`), `speak`
(`audio__speak`), `transcribe` (`audio__transcribe`) and `voice` (the voice that
goes with the default speech model). Input is strict: a write naming another
field (`image__generate`) is refused with the five it takes, never stored as
nothing; a stored level is read tolerantly (`MediaDefaults::from_stored`), a
key a newer build added dropped on its own. Drawing and editing have a default each,
not one shared: `image__edit` offers only the models that edit (§14 item 76),
and the row that draws best is usually not one of them, so a drawing default
never stands in for an edit default. `image__upscale` has no `model` (§9) and so no
default.

- **A folder's defaults are read at each turn**, not copied into the thread
  as the folder's other defaults are (chat-complete §5): every thread in the
  folder follows a change, a thread created before the change included. The
  turn reads the folder with its approval floor (`web::chat_turn`) — for a
  device's thread in a folder the device does not see too; the route below
  resolves the same way and only leaves the folder's own field out of its
  answer. A folder's `defaults_patch` changes `media_defaults` field by field,
  as it does `voice` (a field's `null` inherits again, `media_defaults: null`
  unsets the level).
- **Scope:** Settings' defaults apply everywhere a media tool is listed —
  `/mcp`, `/v1/responses`, agent runs, a realtime session and a Chat thread.
  The folder's and the thread's apply in Chat threads only (the turn's tool
  list, `mcp::exec::resolve_attached_in_thread`); the other callers have no
  thread.
- **A run's calls use the defaults its tool block named** (§14 item 83): the
  list resolves them once (`Resolved::media_defaults`) and the run's executor
  holds them (`MediaExecutor::with_defaults`) — a Chat turn, a `/v1/responses`
  request, an agent run. A settings change while the run goes on does not move
  a call that leaves out `model` to another model; a named default gone since
  is the call's error naming it and the choices. `/mcp` resolves per request:
  its `tools/list` and each `tools/call` read the defaults as they are then. A
  realtime session reads them at each call yet.
- **A default resolves only while it is one of its tool's choices for the
  caller** (`Offer::default_for`): the enum of §3.2, narrowed by the caller's
  key scope (§3.7) — a speech row's voices with it, so a scoped key is shown
  no row's voices it may not use. A row switched off or deleted, an alias that
  lost the route, a model outside the caller's alias scope — each is no
  default, and the tool
  requires `model` again (the behaviour before defaults). Nothing falls back to
  another row; a thread's or a folder's default that does not resolve does not
  fall back to the level above either.
- **An explicit `model` always wins.**
- **The voice** applies when the call's model is the default speech model it
  belongs to (`Offer::voice_for`): a row that lists that voice, or a cloud alias
  (whose voices are the provider's own, so lmgw cannot tell). A call that names
  another model, or a voice of its own, does not take it.
- **The tool block:** with a default, `required` leaves out `model`, the
  description's first paragraph ends "Omit `model` to use <name>.", and the
  `model` property's description says the same; with a default voice, the
  `voice` property's description names it. These change only when the resolved
  default does (§14 item 12's rule). A change of Settings' defaults sends `/mcp`
  sessions `notifications/tools/list_changed`.
- **Writes are checked**: a name a level does not hold yet must be one of its
  tool's choices for the writer (a device's key scope included), else a 400
  naming the reason and the choices — a name of the wrong kind says which tool
  cannot use it. A voice must be one of the level's speech model's when the
  level sets or inherits one (a row's listed voices, named in the refusal; any
  name with a cloud alias); with none, one some speech row the writer may use
  lists (any name passes while a cloud speech alias is among the choices). A
  name the level already holds passes, so a model switched off since never
  blocks a save of something else.
- **Stale defaults are shown, never replaced:** `GET /chat/api/media/defaults`
  (`?folder=`, `?thread=`) answers each level's stored names, what the
  narrowest level asked inherits, what resolves now, the pickers' choices (the
  enums, with labels and each speech row's voices) and every stored name that
  does not resolve with its reason. The pickers keep such a name picked and
  marked "— unavailable", with the reason under the box.

**UI.** Settings → Chat → *Picture and sound tools*: Drawing pictures, Editing
pictures, Speaking text, Voice, Transcribing recordings; the empty choice is
"None — the model chooses". The thread's drawer and the folder's form have the
same five boxes in a section of the same name, whose empty choice is "Inherit —
<what it inherits>" and whose "None — the model chooses" is a choice of its
own. The voice box lists the voices of the speech model it goes with (the box
above, or what it inherits); with a cloud alias it offers the same Inherit and
"The model's default voice" choices and "Type a voice…", which opens a box for
the provider's voice name, so a stored `""` reads as the model's default voice,
not as Inherit.

## 4. In-process calls

### 4.1 `proxy::image_call`

One function serves the tools and the card actions (§8.3):

```rust
pub(crate) struct ImageCall<'a> {
    pub alias: &'a str,
    pub action: Action,            // Generate | Edit { image, mode } | Upscale { image, upscaler, repeats }
    pub params: Params,            // prompt, negative_prompt, size, seed, steps, cfg, strength
    pub preview: Option<PreviewMode>,
}
pub(crate) async fn run(
    state: &SharedState, ctx: &RequestCtx, proto: ClientProto,
    call: ImageCall<'_>, hooks: &dyn Hooks, stop: &StopSignal,
) -> Result<ImageOutput, GatewayError>
```

`Hooks` carries `before_admit(&Route)` (the claim let-go, §4.3) and the observer
of §5 (no-op in slice 1). In order:

1. the key's policy for `alias`;
2. `gate::resolve(alias, RouteCheck::Image(endpoint))` — the hold swap and the
   route guard, so an edit on a row without `edit` is refused before anything
   starts;
3. `hooks.before_admit(route)`, then admission (`Routed::admit`);
4. **a local sd-server route → the native job** (§5): build the body, `POST
   /sdcpp/v1/img_gen` through `vram::send_local` (so a dead container is
   recovered once, image §14), poll `GET /sdcpp/v1/jobs/{id}` until a terminal
   status, decode `result.images`. Used for every local call, with or without a
   preview: it is the only path with a cancel and a job time;
5. **any other route → the OpenAI route**, synchronous: generations as JSON with
   `prompt`, `size`; edits as multipart with the picture; both with
   `response_format: "b64_json"`, except for an OpenAI `gpt-image` model, which
   always answers base64 and does not take the parameter (lmgw fetches no
   URLs). What the route cannot
   say (seed, steps, cfg, negative prompt, strength) is listed in the result as
   "not applied by <alias>"; nothing is smuggled into a cloud prompt. Upscale on
   such a route is refused ("upscaling needs a local image model");
6. one `request_logs` row of class `image` (`finish_image_job` beside
   `finish_image`: route `/v1/images/generations` or `/v1/images/edits` as the
   matching HTTP call would log, `/sdcpp/v1/upscale` for an upscale; zero tokens,
   `images_out` from the result, latency the wall time).

`ImageOutput` is the images (bytes, mime), the effective parameters, the job time
(`completed − started`), the wall time, `answered_by` and the fallback reason.

**Native body** (`proxy/image_call/body.rs`, pure, unit-tested):

| Tool parameter | Native field |
|---|---|
| `prompt`, `negative_prompt` | `prompt`, `negative_prompt` |
| `size` `WxH` | `width`, `height` (parsed; malformed → `isError` naming the shape) |
| `seed` | `seed`; absent → lmgw draws one in `0..=i32::MAX` (§12) |
| `steps` | `sample_params.sample_steps` |
| `cfg` | `sample_params.guidance.txt_cfg` (the key the capabilities' `defaults` spell, image §14) |
| `strength` | `strength` (variation only) |
| edit input | `ref_images: [data URL]` on an `edit` row, `init_image` + `strength` otherwise |
| — | `preview`, `preview_interval: 1` when a mode applies (§5.2); `output_format: "png"`, `batch_count: 1` |

Unset fields are omitted, so the server's own defaults (the row's `args`) apply.
The effective values for the card are the request's, else the probed
`defaults_by_mode.img_gen`; a value neither has is shown as unknown, not guessed.

**Bounds**: the whole job — submit to terminal status — is bounded by the
route's own ceiling, `upstream.request_timeout()` (`image.request_timeout_seconds`
on the synthetic upstream, `0` = none), and surfaces as `GatewayError::Timeout`
like the sync route's. A `429` (queue full) is an `isError` result naming the
build's `limits.max_queue_size`. A transport error while polling means the
container died: `LocalHold::recover` restarts it and the job is submitted again,
once, as `send_local` retries a sync request once. A 404 or 410 for the job
means the container restarted under it (sd-server keeps its jobs in memory, and
only lmgw submitted this one): the job is submitted again on it, without a
recovery, spending the same single resubmit. A second loss of either kind is the
error.

### 4.2 Audio calls

- `audio__speak`: the key's policy, then `synthesize::open` on the alias (gate,
  hold swap, admission), the text cut into clauses by the splitter a spoken reply
  uses (`realtime::clauses::ClauseAggregator`), one `Synthesis::speak` per clause,
  the WAVs joined into one file at their common rate (clauses of one route at two
  rates are the engine's error, said as such), `Synthesis::finish` writing the
  one TTS row. The read-aloud's sentence batches are not used yet: they belong to
  its streaming speaker.
- `audio__transcribe`: `transcribe_for` with the ref's bytes, name and mime, the
  row's alias and the optional language; one ASR row.
- Both race the call's stop as they do today; audio calls have no container
  stop yet.

### 4.3 The loop's claim

`before_admit` lets the chat loop's claim go when the route the gate settled on is
a local row placed on the GPU — an image row, a GPU audio row — and does nothing
for a CPU audio row or a cloud route. Every loop that holds a claim for its run
does so — the Chat's, a `/v1/responses` run's, an agent run's: their media
executor's `Hooks` (`agent/claim/media.rs`, holding the loop's `Arc<LoopClaim>`,
`agent/claim.rs`) calls `LoopClaim::let_go`. A realtime session holds no claim
across its tools (it drops its hold per stream). the decision needs the route, which only the call's
resolve knows, so it is a hook inside the call rather than a wrapper around the
executor. An audio call resolves once for the hook before `synthesize::open`
resolves again (a resolve starts nothing).
That is safe for the same reason as the heard wait: the loop's model call is over
when its tools run, and the next model call takes the claim again
(`LoopClaim::for_call` → `LetGo::regain`). Parallel calls in one turn are fine:
`let_go` is idempotent, and only the next model call regains. A claim shared by
concurrent model calls (an agent's classify stage) lets go once they have ended.

A regain refused goes where any request goes (`LoopClaim::call_on`,
`agent::claim::reroute`): under a GPU block switched on meanwhile, to the block's
configured fallback; for a candidate alias whose model the picture evicted and
that the alias may not start again (`CandidateLost`, a `No` or `Background`
restart rule), to the alias's next pick through the gate's walk. That call holds
its own claim; the loop's stays let go, so its next call tries the regain again.

What follows is the existing scheduler and nothing else (decision 2): the image
row's admission evicts LRU residents with no request in flight — the chat model
now among them — the row stays up for its `idle_seconds`, and the regain evicts
it again if the chat model does not fit beside it.

### 4.4 Under the GPU hold

The gate's hold swap does it (decision 7): a row with a fallback alias is answered
by that alias through the OpenAI route (§4.1 step 5), the result and the card say
so, and the request row carries the fallback reason; a row without one is the
hold's `GpuHold` error as the call's `isError` result, which the model relays. A
CPU audio row serves under the hold. The Chat's model itself may be on its own
fallback at the same time; nothing here couples the two.

## 5. Native jobs: progress, previews, cancel

### 5.1 The poll

The job is polled every `agent::CANCEL_POLL` (250 ms), the existing named rate at
which "a person waiting reads a stopped clock as instant"; it samples, it bounds
nothing. Each poll may yield an observer event:

```rust
enum ImageProgress {
    Stage { phase: Phase, model: String },          // §5.3
    Queued { position: u32 },                        // job queue_position > 0
    Generating { pass: u32, step: u32, total_steps: u32, preview: Option<Bytes> },
}
```

A preview is passed on only when its `(pass, step)` changed since the last one; a
job with no `preview` object (mode `none`, or a build without previews) yields
`Generating` with no step, and the Chat shows elapsed time alone. The frame is
passed on as sd-server made it; its pixel size depends on the mode: on 0518b34
`proj` is 64×64 at 512² and 128×128 at 1024², `vae` the picture's own size (§2).
As built the events are `MediaProgress` (`proxy/image_call/observe.rs`), each
carrying the model it is about and, once a job exists, what a cancel would do
now; a call's hooks drop a report that says nothing new (`mcp/media/hooks.rs`).

### 5.2 Preview mode

`image_models.preview_mode` (nullable, migration 0085). `NULL` is the default:
`proj` when the row's probed `preview_modes` contain it, else `none`. A mode the
probed build does not list is not sent; the editor shows it unavailable with the
reason. Labels (no raw keys on screen):

| Value | Label | Note shown beside it |
|---|---|---|
| `none` | Off | "Progress shows elapsed time only." |
| `proj` | Quick sketch | "A rough colour sketch from the sampler's state. Costs nothing." |
| `tae` | Tiny decoder | "Needs a TAE file in this row's files. sd-server then also decodes the final image with it, which lowers its quality." Unavailable while `files` has no `taesd`/`tae`. |
| `vae` | Full decoder | "Decodes every step with the row's VAE: slower, and the extra memory raises the row's learned peak." |

The learned peak (image §9) needs nothing new: a `vae` preview's extra buffers are
sampled like any other transient.

**Which mode a job asks for** depends on how closely its caller follows it
(`Hooks::follows`, as built in slice 2): a caller that shows no previews (`/mcp`
progress, an agent run, a `/v1/responses` run, an `/mcp` call without a progress
token) asks for `proj` where the build lists it, because sd-server reports steps
only beside a preview, the steps are what `/mcp` progress and the job's pace
(§5.4) are read from, and `proj` costs nothing measurable (1.05 s against
1.04 s); it drops the frames. The Chat asks for the row's mode as above. A row
whose mode is `none` asks for none for any caller, and its jobs then report no
steps.

### 5.3 Swap stages

The queue's waiter gets a typed phase beside its free-text `stage`:

```rust
pub enum WaitPhase { Gate, Measuring, Evicting { victim: String }, WaitingForRoom { needs: u64, free: u64 }, Starting }
```

`evict_one` takes the waiter id and sets `Evicting { victim: "<class>/<model>" }`
before it stops a victim, so the stage names what goes. As built, the waiter's
words keep its figures beside the victim ("needs 7.1 GB, 2.3 GB free — evicting
chat/qwen3-30b"; `evict_one` takes the wait's needs too), so the Traffic page,
which reads the words, keeps them while the victim goes; words no phase types
(the look for lost containers, the moment before a victim is chosen, a climb's)
clear the phase, and the follower's watch remembers what it last sent, so a
phase set again across such words is not told twice. As built, a caller
follows its **own** waiter rather than a target's: it runs its admission inside
`vram::stage_watch::follow`, a task-local watch the waiter enqueued inside it
carries, and every phase set on that waiter is sent to it as well (two requests
may wait for the same model, and the telemetry view cannot tell them apart; the
queue also broadcasts at enqueue and dequeue only). `WaitPhase` lives in
`lmgw-api-types` (`status.rs`, tagged by `kind`) and the `vram` view's queue
carries it as `phase`. The image call says `loading` itself before admission
when the row has no ready container (admission switched off or unmeasured makes
no waiter), maps `gate`, `measuring` and `starting` to `loading`, `evicting` to
`unloading` naming the victim, and `waiting_for_room` to `waiting_for_room` with
its `needs_bytes` and `free_bytes`. The Chat renders them as "Unloading
chat/qwen3-30b", "Loading image/z-image-turbo", "Waiting for room (needs 7.1 GB,
2.3 GB free)", then "Generating". A local route that is already resident skips
straight to "Generating".

The chat model coming back is the other half of the swap. `LoopClaim` gains a
`RegainReport` (the Chat's is `web::chat_voice::RegainFrames`, on the turn's
`Events`); when a regain has to start the model (no ready port), it sends the
existing `state` frames (`ModelState::loading("chat", alias)`, then `ready` with
`ms`, or the refusal's `held`/`failed`), as `admit_reporting` does for the turn's
first admission.

### 5.4 Cancel

The per-call cancel (§6.5, §7.3), the turn's Stop (the call's own stop raised,
§6.5) and a dropped call future (an `/mcp` disconnect) take one path, owned by a guard in the job loop
(`proxy/image_call/cancel.rs::JobGuard`, which runs it from `Drop` on a spawned
task):

| Job | Build | Action | Container |
|---|---|---|---|
| `queued` | `cancel_queued` | `POST /sdcpp/v1/jobs/{id}/cancel` | stays |
| `generating` | `cancel_generating` | the same; wait for `cancelled` | stays |
| `generating` | no `cancel_generating`, this call the row's only request in flight | drop the hold, `Registry::stop_generation(Image, row, generation, force)` | stopped; the next call reloads it |
| `generating` | no `cancel_generating`, other requests in flight on the row | stop waiting; a finisher task keeps polling to the terminal status **holding the claim**, then drops it | stays; the job's result is discarded |

`stop_generation` stops only the container the hold was admitted to, so a row
restarted meanwhile is not hit. Holding the claim in the abandon case keeps
admission honest: the GPU is busy until the job ends. The synchronous upscale
route has no job: cancelling it is the restart or abandon rows by the same rule,
an abandoned upscale holding its claim until the image server answers it.

The tool result of a cancelled call is `isError` with "Cancelled by the user; the
picture was not made. Do not try again unless asked." The loop goes on, so the
model can answer. Each progress frame carries which of `native`, `restart` or
`abandon` a cancel would take now, so the button can say it before it is pressed
(§8.1); the result says which one happened, between the two sentences ("… was not
made (image/z could not stop mid-picture, so lmgw stopped its container; the next
picture loads it again). …").

As built (`proxy/image_call/cancel.rs`): the guard owns the call's claim, the job
loop tells it each job it submits and each status it polls, and the call tells it
when it is done with the job; only a call that lets its job go running — a stop,
a dropped `/mcp` future, **the route's own timeout**, a stall or an unanswered
status read (below) — cancels: the timeout, the stall and the silence are told
to the guard (`JobGuard::let_go`), which keeps the job, so dropping the guard
cancels it as a stop's drop does. A job submitted but not polled yet counts as
queued. A native cancel the server refuses with `409` (the
job began generating between the poll and the cancel) falls back to the restart
or abandon rule, which the result's sentence, chosen at the drop, does not see. A
job whose submit is still on the wire has no id to cancel: the claim goes. The
restart stops the generation unforced after the claim let go, so a request that
joined the row meanwhile keeps it and the job runs on under that request's claim.
The followed job (native or abandon) is followed until it ends, fails, or its
image server no longer answers for it (a dropped connection, a non-success
status), or until it **stalls by its own pace** (`proxy/image_call/pace.rs`):
the longest gap between two of its advances while generating (another status,
queue place or `(pass, step)`), from the first `generating` read on; a job that
goes `STALL_PACES` (20) of those without advancing, a poll still unanswered then
included, has stalled, and the warning names the quiet time, the slowest gap and
the factor. It is judged once the job has taken `JUDGED_AFTER_STEPS` (2) steps,
which the warning names too. Never by a clock that started with the call: a call
that timed out holds its claim while its job still draws. A job that reports no
steps (a row whose preview mode is off, a build without previews) has no pace
and is followed while its server answers for it; a log line says so when a
cancel leaves it to be followed. On a route with no ceiling
(`request_timeout_seconds` 0) the call's own follow judges the same pace: a job
that stalls there is given up, and its guard restarts the row when the call is
its only request (no native cancel is asked of a server that stopped working on
it), else lets the claim go. A route with a ceiling keeps it as the call's
bound, so a stage the pace did not foresee (a large picture's decode after fast
steps) is not cut short below it.

**A server that stops answering.** Every status read — a job's submit, each
poll (the call's and a follow's), a native cancel — must be answered within
`STATUS_ANSWER` (`proxy/image_call/answer.rs`), which is `vram::CONTROL_TIMEOUT`
(5 s), lmgw's existing bound for a control read of a container it runs (the busy
probe's, the hold sweep's); `state.http` itself keeps no timeout, so the draw
and the upscale are never cut by it. A read unanswered within it is a server
that stopped working, as a stall is, pace or none: the call lets its job go
(`LetGo::Unanswered`), and a follow stops following; the row is restarted where
the claim is its only request, else the claim goes, and the warning and the
call's error name the bound. A follow's stall takes the same rule now (the
claim only went before).

**The upscale** runs its request on a task of its own that shares the call's
claim, and the guard keeps the task: a restart aborts it before the container is
stopped, and an abandoned upscale — stopped, or past the route's ceiling, with
another request on the row — is followed until the request ends, holding the
claim. While it runs, the call (and an abandoned upscale's follow) reads the
image server's readiness route (`GET /sdcpp/v1/capabilities`) every
`CANCEL_POLL`, each read bounded by `STATUS_ANSWER`; one unanswered gives the
upscale up by the rule above. A refused connection counts as an answer there:
a dead container is the request's own send to find and recover. Its progress
frames say `upscaling` with the `cancel` kind the guard would choose (`restart`
or `abandon`), sent again when that changes. *Not measured live yet:* that
sd-server answers its capabilities route while an upscale runs (the job API
answers polls during a draw; the upscale runs in its request's handler). The
timeout's, the stall's and the silence's errors say what became of the job or
the upscale (`cancel::let_go_error`).

### 5.5 No job ledger

Image §13 deferred the native routes for API clients because a `202` ends the
HTTP request while the job holds the GPU, which needs a ledger keeping the guard
alive and routing polls to the right container. A tool call is different: the
call itself stays open until the job ends and holds the guard all the while, so
the ledger is not needed (decision 4). Async `/v1` image routes for API clients
remain that follow-up.

## 6. Chat wiring

### 6.1 Executor

`MediaExecutor` (`mcp/exec/media_exec.rs`, logic in `mcp/media.rs`) joins
`SplitExecutor` the way `KbExecutor` did: `SplitExecutor::new` builds a
thread-less one (§3.6) for every user, and `run_send` replaces it with
`.with_media(MediaExecutor::new(..).for_thread(files).with_hooks(hooks))` —
`files` the thread's `web::chat_media::ThreadFiles` (refs and storage), `hooks`
the claim's `agent::claim::media::MediaClaim` (§4.3; `/v1/responses` and agent
runs pass theirs through `SplitExecutor::with_media_hooks`). `ScopedExecutor` and `HeardTools` wrap it
unchanged. The executor knows which call it runs from the loop's task-local call
id.

### 6.2 Frames

`ToolFrame` (`lmgw-api-types/src/chat_frames.rs`) gains `Progress`
(`event: "progress"`):

```json
{"event": "progress", "call_id": "call_3", "phase": "generating",
 "model": "image/z-image-turbo", "pass": 1, "step": 3, "total_steps": 8,
 "queue_position": null, "elapsed_ms": 2140, "cancel": "restart",
 "preview": "iVBORw0KGgo…"}
```

`phase`: `unloading | loading | waiting_for_room | queued | generating | speaking |
transcribing | upscaling`; `model` the row the phase is about (the victim for
`unloading`); `preview` base64 PNG or absent; `cancel`: `native | restart |
abandon`, absent for audio **and before a job exists** (a cancel then only stops
the wait); `waiting_for_room` adds `needs_bytes` and `free_bytes`. The frame names
its call by `call_id` only, not by `index`: the executor knows the id, and the
`ready` frame pairs the two. As built the type is `ToolProgressFrame` in
`lmgw-api-types/src/chat_media/progress.rs`; audio calls send one `speaking` or
`transcribing` frame when they start. Progress frames are lossy: the executor sends them with
`try_send`, so a slow reader drops previews rather than stall a generation, and the
next frame supersedes the last anyway. They are not written to the change feed.
Clients built against an older `lmgw-api-types` read the event as
`TurnEvent::Unknown` and skip it. (The dashboard's own reader had used `progress`
as its example of an unknown event in two tests; they name `checkpoint` now.)

`ToolResultFrame` gains `attachments: Vec<AttachmentMeta>` (built in slice 2):
what the call stored, so the page renders them without a reload. Its `content`
carries the `lmgw:attachment/N` refs, not the bytes. `ThreadFiles` keeps each
call's files by call id until its result frame takes them.

### 6.3 Stored files

Migration 0085 gives `chat_attachments` a `source` (`upload | tool | action`) and a
`call_id`. A tool's file is inserted **when the call ends** (kind `image` or
`audio`, `source = 'tool'`, its call id, `message_id NULL`, name
`<model-stem>-<seed>.png` by the Image lab's `model_stem` rule, or
`<model-stem>-<voice>.wav`), so its ref exists for the next call of the same turn.
`meta.media` holds the facts (`MediaFacts`, §9). At persist, `ChatRepo::save_reply`
binds the turn's tool files to the reply in the same transaction as the reply row
— by the attachment ids the turn's `ThreadFiles` stored, carried on
`ChatReply::tool_files` (`UPDATE … SET message_id = ? WHERE thread_id = ? AND
message_id IS NULL AND source <> 'upload' AND id IN (…)`), which a call id that a
model reuses across turns cannot confuse; a continue or a resume binds to the row
it appends to. A reply that is not saved (the ticket refused it) deletes its
turn's tool files in the same path (`Turn::persist`). Every draft query — listing, sending, deleting,
export — reads `source = 'upload'` only, so a tool file is never mistaken for a
draft. A file whose turn never reached persist (the process died) belongs to its
thread and goes with it; nothing sweeps.

Binding to the reply means the reply's lifecycle covers them: deleting or
regenerating a reply removes its pictures as it removes its text
(`ON DELETE CASCADE`). Bound files enter the chat search through the existing FTS
trigger on `message_id`.

Temporary threads keep their files in the in-memory repo like their other rows
(`ChatRepo::Temp`), and keep-as-thread copies them with the rest. Keep gives
each file a new row id, so it rewrites the records it writes with them: every
`lmgw:attachment/N` resource and every `att:N` in a tool result's text names the
kept file (`store/chat_keep/refs.rs`), and the kept thread's next turn gets its
pictures back. Each picture's `meta.media.parent` is rewritten the same way, and a
file no message took yet (a stopped turn's or card action's) is not kept (§14
items 43, 44).

### 6.4 The record

The turn record (`ir_messages`) keeps `Resource{uri: "lmgw:attachment/57",
mime: "image/png"}` where the image was, never the base64: a thread's detail JSON
carries every record, and a 2 MB string per picture would ride along on every open.
`web::chat_media::Hydrator` turns each such resource back into
`ToolResultBlock::Image` with the stored bytes, reading each ref once per turn.
It runs twice: when a turn's request is built from the history (`chat_turn`, for
the plain path and the tool loop alike, and the PDF text form beside it), so a
thread whose toolset was switched off still replays its pictures; and in
`PerRoute::request` in the tool loop, before `blind.on_route`, for the refs this
turn's own calls left in the loop's request, so every model call of a turn sees
the picture its earlier call made. What happens after is the existing rule: the
llama.cpp egress sends it where `tool_images` allows and a placeholder with a WARN
elsewhere, Anthropic natively, OpenAI chat-completions as a placeholder. A ref
whose attachment is gone becomes the text `[picture att:57 was deleted]`. An audio
ref is never rehydrated for the model (§3.5).

### 6.5 Cancel route

`POST /chat/api/threads/{id}/media/{call_id}/cancel` (`Cap::Chat`, in the
`server.rs` route table and the OpenAPI chat plane): raises the stop of that call,
or of the card action with that id (§8.3). Answers `{ok, cancel: "native" |
"restart" | "abandon"}` (`MediaCancelled`; `cancel` absent when no job runs yet,
or for an audio call), `404 not_running` for a call that is not running and
`404 not_found` for a thread the caller does not reach. As built the running
calls are `AppState::media_calls` (`web/chat_media/cancel_route.rs`), registered
by `(thread id, call id)` while a call runs through the executor's `LiveCalls`
(`web/chat_media/progress.rs::ThreadLive`); each call gets a stop of its own, and
the turn's stop is passed on to it, so both take the same path. The whole turn's
Stop reaches a media call as the call's own stop: the loop hands each tool call a
stop it may take (`agent::take_call_stop`), raises it when the turn's cancel
lands and waits for a call that took it, where any other call is dropped. The
media executor takes it, so a stopped call ends at its next await and still
writes its model row and its tool row, both `canceled` — a cloud picture billed
before the stop is in Usage. Since slice 2 the job is cancelled
by §5.4's guard; slice 1 stopped only the wait.

### 6.6 Device clients

Assistant messages now carry `attachments`, and `ToolResultFrame` an
`attachments` list. Kai and kai-mobile render neither yet; their rendering is a
planned follow-up at their next pin, not a reason to shape this differently.

## 7. `/mcp`

### 7.1 The call's context

`ToolPlane::call_tool(name, args, cx: CallCx)`, `CallCx { progress:
Option<Box<dyn media::Progress>>, stop: Option<StopSignal> }` (as built: both
optional, `Default` for none). The admin plane and the test fakes take it and
ignore it. `ToolPlane::reports_progress(name)` (default `false`; the aggregate
says it for `image__*` and `audio__*`) decides whether a call may be answered as a
stream. `dispatch` is generic over a `Clone + 'static` plane, so the stream can
own the call.

### 7.2 Progress

A `tools/call` whose `params._meta.progressToken` is set, from a client whose
`Accept` includes `text/event-stream`, for a tool that reports progress, is
answered as an SSE stream (Streamable HTTP allows either response form per POST):
`notifications/progress` frames while it runs, then the JSON-RPC response, then the
stream ends. Without a token, without the `Accept`, or for any other tool, the
answer stays one JSON body and progress is dropped. The call runs as the stream is
read (`mcp/ingress/progress.rs`); notifications still queued when it answers go
out before the answer.

```json
{"jsonrpc": "2.0", "method": "notifications/progress", "params": {
  "progressToken": "t-1", "progress": 3, "total": 8,
  "message": "image/z-image-turbo: step 3 / 8"}}
```

`progress` counts steps over all passes, so it only rises as MCP requires;
`total` is the current pass's `total_steps` while the job has had one pass, and
absent once a second pass began. Stages go as `message` ("unloading
chat/qwen3-30b", "loading image/z-image-turbo"); as built they do not repeat the
last `progress`, which MCP forbids ("MUST increase with each notification"):
each message before the first step moves `progress` half-way to 1 (0.5, 0.75, …),
so it rises and stays below the first step, and a report that would not raise it
is not sent. Previews do not travel: MCP progress has no image field, though the
job is asked for `proj` so that it reports its steps (§5.2). The stream sends a
keep-alive comment every `SSE_KEEPALIVE` (15 s), as `GET /mcp` does: a phase
that reports nothing new (a wait for room whose figures stand still, a job
reporting no steps) would leave it silent, and a proxy that drops an idle
connection would cancel the job (§7.4).

### 7.3 Cancelled

`dispatch` handles one notification instead of acknowledging it blind:
`notifications/cancelled {requestId, reason}` on a known session raises the stop
of that session's in-flight call with that id (`ingress/progress.rs`: a table of
`(session id, request id) → StopHandle`, an entry living as long as its call; the
request id keyed by its JSON, so `2` and `"2"` stay two ids). Every `tools/call`
with a session is registered, streamed or not. The `202` follows as today. The
cancelled request's eventual response is still sent; MCP lets the client ignore
it. As built, the stop is read by the image and audio tools only: a southbound
server's call is not cancelled yet (nothing on that path reads the stop; the
call runs to its answer or its server's `timeout_ms`). A device row's call has a
cancel path of its own (`mcp/host/calls.rs`, on a dropped future), which a
later pass can reach the same way.

### 7.4 Disconnects

MCP says a disconnect is not a cancel, but lmgw has no stream resumption: a
result nobody can receive is abandoned, which is today's behaviour for any `/mcp`
call (the handler future is dropped). For an image job that drop runs the §5.4
guard, so the GPU is never left working for nobody without the claim saying so.

## 8. Chat UI

### 8.1 While it runs

The tool card of a media call grows a live pane (`pages/chat_media/live.rs`):

- the stage line from `progress` frames ("Unloading chat/qwen3-30b · Loading
  image/z-image-turbo · Generating"), with the elapsed time;
- the latest preview, `<img>` from the frame's base64, sized to the target's aspect
  ratio, with "Step 3 / 8" (and "pass 2" once a later pass begins); without
  previews, a spinner and the elapsed time;
- **Cancel**. For `native` and `abandon` it acts at once ("Stop waiting: another
  request is using image/z-image-turbo, so its picture finishes in the background
  and is thrown away" is the abandon tooltip). For `restart` it is the inline
  confirm the task strip uses: "Cancel restarts image/z-image-turbo: it cannot
  stop mid-picture, so lmgw stops its container, and the next picture loads it
  again." → **Restart and cancel**.

The final image replaces the preview in place. The `state` frames of the chat
model's reload show in the turn's existing state line.

As built (slice 3): the pane sits under the call's tool card (`CallMedia`),
shown while the card runs and its tool is an `image__*` or `audio__*` one; its
frame is the picture's frame (the asked `size`, else square), so the picture
lands where the preview was. Before the first `progress` frame it says
"Waiting for the picture" with a spinner and the page's own clock; the stage
line lists each stage once in a row (a queue place or a wait that changes
rewrites its own entry). Cancel stays disabled until the call's `ready` frame
has named its id; with no `cancel` kind on the frame its tooltip says it only
stops the wait. The restart confirm is the pane's own (the sentence above, then
**Restart and cancel** and **Keep drawing**), not the task strip's
`ConfirmButton`, whose armed button carries the question as its label; it is
an `alertdialog` named "Restart the image model to cancel?" and described by
the sentence, takes the focus on **Keep drawing**, and Escape keeps drawing,
the focus back on Cancel. The
chat model's regain `loading` after a picture reads "reloading chat/x after
the picture…" on the turn's line; its `ready` reads as any load's. A `result`
frame drops the card's preview and stages; a turn whose stream ends with a
call still running (Stop mid-picture, a failure) settles that card as
stopped ("The reply was stopped before this call answered."), its pane gone.
A user's cancel reads "cancelled" in the card's summary, in the neutral
colour of a declined call, never as a failure.

### 8.2 The image card

Under each produced image (`pages/chat_media/card.rs`), from `AttachmentMeta`
with `source` `tool` or `action`:

- facts: model (and "answered by image-cloud.example, fallback" when one did),
  seed, steps, cfg, size, time ("4.7 s, plus 3.2 s loading");
- **Re-roll**: same prompt and settings, a new seed;
- **Vary** on a variation row, **Edit** on an `edit` row: an inline prompt field
  prefilled with the original prompt (and a strength field for a variation row),
  run as an image call with this image and recorded as `image__edit` (a
  variation is the call's `Action::Vary`, never an edit: §14 item 76);
- **Upscale** when the upscale tool is offered (slice 4);
- **Save**: the picture as `<model-stem>-<seed>.png`, through the same save path
  as the Image lab's download;
- **Attach**: adds the picture to the composer as a draft (a copy with
  `source = 'upload'`), so the next message carries it to the model.

Action results join the card as a strip of thumbnails under the original, newest
selected, each with its own facts and actions. They are not shown to the model
unless attached.

As built (slices 3 and 4): a call's pictures are the reply's attachments whose
`call_id` is the card's (`ToolResultFrame.attachments` while it streams, the
stored reply's afterwards); a card gathers the `action` pictures whose
`meta.media.parent` chain leads to its picture, and an `action` picture whose
chain leaves the message (a re-roll of an edit names the edit's input) gets a
card of its own under the reply. The facts line also says what the answering
route did not apply ("not applied: seed") and the prompt (two lines,
whole on hover). **Vary** or **Edit** is picked by the catalog: a local row
whose task is `image_edit` edits, any other local row varies, a cloud alias
edits (§14 item 23); a `wrong_action` refusal flips it and reopens the form
with the prompt. **Upscale** is offered on every picture but an upscale's
(*call*: the page cannot see the offer); it opens a note that it runs on a
local image model only, and the gateway's refusal (no upscaler, the hold, a
size over the limit) is said under the card. **Save** is the attachment's own
download; **Attach** fetches the picture and uploads it as a composer draft
(§14 item 49's copy), so the Chat's own upload path checks it.

After the UI review (§14 items 68–75): the picture's tooltip is "Open full
size" and its alt text the prompt (the file name when it has none); a picture
is never drawn wider than its own width; the actions (not Save or Attach) are
held, "Once the reply is finished", until the reply is a stored message; a
running action's pane sits beside the strip, outside its `listbox`; a running
action is the page's, not the card's, so a thread left and opened again while
it runs shows its pane and Cancel and holds the buttons; Attach shows its
chip at once and is held until the copy is in.

### 8.3 Card actions

`POST /chat/api/attachments/{id}/media` (`Cap::Chat`), body
`{action: "reroll" | "vary" | "edit" | "upscale", prompt?, strength?, upscaler?,
repeats?}` (`MediaActionRequest`). It resolves the source attachment's facts (the
model and parameters it was made with), runs `proxy::image_call` (or its
`upscale`) as the thread's caller, its model row labelled as the thread's speech
calls are (`chat`, `admin`; the thread's media tool calls carry the same label,
§14 item 62), and answers SSE (as built, §14 item 20):
`action {call_id, action, model?, source}` with the action id `act-<boot>-<n>`
(§14 item 48), which the cancel route of §6.5 takes; `progress` frames carrying it
as their `call_id`; then
`done {call_id, attachment, text}` or `error`. The result is a `chat_attachments`
row with `source = 'action'`, the action id as its `call_id`, bound to the
source's message, `meta.media.parent` an edit's input, else the source. No chat
model is called and no claim is let go: no loop is running, and a turn running in
the thread at the same time is just another request to admission. A thread
deleted, kept or purged while an action runs stops the action (§14 item 44), and a
revoked key stops its own (item 50).

### 8.4 Audio

`audio__speak` results render as `<audio controls>` from
`/chat/api/attachments/{id}` with model, voice and duration, plus Save and Attach.
Uploaded audio attachments get the same player in their chip. `audio__transcribe`
shows its transcript in the tool card as today. As built (slice 3): the
spoken result's player, facts (model, the fallback that answered, voice,
length), Save and Attach are under its tool card; an uploaded audio chip has
no player yet. *Call:* the player loads nothing until it is played
(`preload="none"`): a thread with many recordings fetches none on open, and the
facts line says the length. (Under Broadway, where WebKit has no audio output,
`preload="metadata"` once left the page answering no script for over a minute.)

### 8.5 Elsewhere

- `tools_from_ir` names stored refs (`image att:57`) instead of `[resource]`.
- The image editor (`pages/model_editors.rs`) gains **Prompt style** (the three
  labels and "Derived from the recipe: Plain sentences") and **Live preview**
  (§5.2's labels, unavailable modes greyed with their reason).
- The tool picker needs nothing: the inventory carries the two labels.

As built (slices 3 and 4): the image editor's two fields are an "In the Chat"
section (`model_editors/image_chat.rs`); the Live preview list marks a mode
"(unavailable)" and says why under it (no TAE file; not in the running
container's `preview_modes`), and before a container has run it says which
modes are offered is known once the model has started. The image recipes list
ends with **Upscalers** (`image_recipes/upscalers.rs`): each shipped entry with
its size, scale, licence, where it lands and Add (`image_upscaler_add`), the
files already in `upscalers/`, and that a running image model finds a new file
at its next Upscale (§14 item 46).

## 9. Wire shapes and schema

**Migration `0085_chat_media_tools.sql`** (one file, all of the feature's schema,
so later slices add no migration):

```sql
ALTER TABLE chat_attachments ADD COLUMN source TEXT NOT NULL DEFAULT 'upload'
    CHECK (source IN ('upload', 'tool', 'action'));
ALTER TABLE chat_attachments ADD COLUMN call_id TEXT;
CREATE INDEX idx_chat_attachments_call
    ON chat_attachments (thread_id, call_id) WHERE call_id IS NOT NULL;
ALTER TABLE image_models ADD COLUMN prompt_style TEXT
    CHECK (prompt_style IN ('natural', 'keywords', 'tags'));
ALTER TABLE image_models ADD COLUMN preview_mode TEXT
    CHECK (preview_mode IN ('none', 'proj', 'tae', 'vae'));
```

**`lmgw-api-types/src/chat_media.rs`** (new): `PromptStyle`, `PreviewMode` (with
labels and notes), `MediaFacts` — `tool`, `model`, `answered_by`, `prompt`,
`negative_prompt`, `seed`, `steps`, `cfg`, `width`, `height`, `strength`,
`job_ms`, `wall_ms`, `parent`, `upscaler`, `scale`, `voice`, `duration_ms`,
`language`, and `not_applied` (the parameters the answering route did not take);
every field optional, tolerant. `AttachmentFacts` reads it from `meta.media`.
`AttachmentMeta` gains `source` and `call_id`; `ImageModel` gains `prompt_style`
and `preview_mode`, `ImageModelView` gains `derived_prompt_style` (what the
matching recipe gives the row, for the editor's "Derived from the recipe" line).

**`status.rs`**: `ImageCapabilities` gains `preview_modes`, `upscale`,
`defaults_by_mode` (as JSON); `ImageLimits` gains `max_upscale_width/height`;
`ImageFeatures` gains `preview`; `ImageAsset` gains `image_upscale`. All
defaulting, so an old build's capabilities still read. Built in slice 1 (the
descriptions' and the job's defaults read `defaults_by_mode`), in the API type and
the probe's own type (`runtime::image`).

**Frames**: `ToolFrame::Progress(ToolProgressFrame)` and
`ToolResultFrame.attachments` (§6.2).

**Upscalers** (slice 4): the `upscaler` enum is the stems of the files at the top
level of `<image models_dir>/upscalers` (what sd-server scans,
`image_upscalers::on_disk`). The recipes page gets an **Upscaler** entry: one
shipped ESRGAN model, `Comfy-Org/Real-ESRGAN_repackaged`
`RealESRGAN_x4plus.safetensors` (66,857,836 bytes, un-gated, BSD-3-Clause, read
with `scripts/hf-recipe-check.py` on 2026-10-21), downloaded into
`upscalers/<file>` instead of the `<owner>/<repo>/<file>` layout by
`image_upscaler_add {key}` (`lmgw__image_upscaler_add`) and listed under
`image_recipes`' `upscalers {shipped, on_disk, dir}`; the page itself is the UI
half, not built yet. Which row runs an upscale: a resident image row
whose capabilities report `upscale`, preferring the row that made the picture;
with none resident, the row that made it; for an uploaded picture, the enabled row
with the smallest planning footprint.

## 10. Build slices

Each slice lands green on its own (`ci/check.sh`), on `main`. Files are owned per slice so slices that may overlap in time do
not share files. Paths are under `crates/`; `core` = `lmgw-core/src`.

**Slice 1 — the toolset, the calls, the stored files** (core and api-types, no UI).
First the build check of §2 (fixtures committed).
- new: `lmgw-core/migrations/0085_chat_media_tools.sql`; `core/mcp/media.rs` +
  `core/mcp/media/{list.rs, args.rs, refs.rs, image.rs, audio.rs, result.rs}`;
  `core/proxy/image_call.rs` + `core/proxy/image_call/{body.rs, job.rs,
  openai.rs, log.rs}` (job: submit and poll to the end, observer hook defined,
  no-op); `core/web/chat_media.rs` + `core/web/chat_media/{store.rs,
  rehydrate.rs}`; `core/web/agentchat/media_claim.rs` (since the review
  `core/agent/claim/media.rs`);
  `lmgw-api-types/src/chat_media.rs`.
- edits: `core/mcp/{mod.rs, exec.rs, inventory.rs, ingress.rs}` (namespaces,
  labels, `MediaExecutor`, `SplitExecutor`, the aggregate's list and call branch
  only), `core/mcp/exec/{labels.rs, served_by.rs}`, `core/agents/mod.rs`,
  `core/web/chat_tool_write.rs`, `core/web/agentchat.rs`, `core/web/chat_turn.rs`,
  `core/web/chat_repo.rs`, `core/web/chat_temp.rs`,
  `core/web/chat_attach_render.rs`, `core/store/chat_attachments.rs` (+ the draft
  queries' callers), the image-model store and `core/config/models.rs`,
  `core/image_recipes.rs` (`prompt_style`), the `ops` image-model patch and the
  audio row writes (`notify_tools_changed`), `core/mcp/selfadmin`
  (`lmgw__image_model_set` params), `lmgw-api-types/src/{chat_threads.rs,
  models.rs}` DTOs, `lmgw-api-types/src/lib.rs`.
- tests: `lmgw-core/tests/it/media_tools.rs` + `tests/it/media_tools/{list.rs,
  image_call.rs, audio_call.rs, chat_turn.rs, refs.rs, hold.rs}`,
  `tests/it/migrations.rs`.
- **As built (2026-10-21):** the files above, and besides them
  `core/mcp/exec/media_exec.rs` (the executor, out of the already large
  `exec.rs`), `core/store/chat_tool_files.rs` (bind, drop, read by id),
  `core/catalog.rs` (`peek_only`), `core/mcp/discovery.rs` (the labels' offer),
  `core/ops/routing.rs` (alias writes nudge `/mcp` too), `core/runtime/image.rs`
  and `lmgw-api-types/src/status.rs` (the capabilities' new fields),
  `core/web/image_lab` (`model_stem` shared), `core/store/{chat_messages.rs,
  chat_approvals.rs, chat_keep.rs, chat_attachment_extra.rs}` (the reply's write
  binds; Keep and uploads carry `source`), and `lmgw-ui/src/pages/models.rs` (the
  blank row's two new fields; the editor has no fields for them yet).
  `ToolResultFrame.attachments` stays with slice 2's frames.

**Slice 2 — progress, previews, cancel** (core and api-types).
- new: `core/proxy/image_call/{observe.rs, cancel.rs}`, `core/vram/stage_watch.rs`,
  `core/web/chat_media/{progress.rs, cancel_route.rs}`, `core/mcp/ingress/progress.rs`.
- edits: `core/proxy/image_call/job.rs`, `core/vram/{queue.rs, scheduler.rs,
  view.rs}` (typed phase, the victim), `core/agent/claim.rs` (regain
  frames), `core/mcp/ingress.rs` (`CallCx`, SSE answer, `notifications/cancelled`),
  `core/server.rs` + `core/web/mod.rs` (the cancel route),
  `core/openapi/planes/chat_threads.rs`, `lmgw-api-types/src/{chat_frames.rs,
  status.rs}`.
- tests: `tests/it/media_tools/{progress.rs, cancel.rs, mcp_progress.rs}`,
  `tests/it/mcp_ingress.rs` (the trait change), a phase case in the vram tests.
- live: `scripts/media-tools-live.sh` (§11).
- **As built (2026-10-21):** the files above, and besides them
  `lmgw-api-types/src/chat_media/progress.rs` (`MediaPhase`, `CancelKind`,
  `ToolProgressFrame`, `MediaCancelled`, `MEDIA_CANCEL_PATH`) and
  `lmgw-api-types/src/mcp_apps.rs` (`ToolResultFrame.attachments`),
  `core/mcp/media/hooks.rs` (a call's claim hook and progress, repeats dropped),
  `core/mcp/exec/media_exec.rs` (`LiveCalls`: a followed call's progress and own
  stop), `core/web/chat_media/store.rs` (each call's files for its result frame),
  `core/web/chat_voice/turn_state.rs` (`RegainFrames`), `core/web/agentchat.rs`
  (the wiring), `core/state.rs` (`media_calls`), `core/vram/{climb.rs,
  background.rs, local_hold/let_go.rs}` (`evict_one`'s new argument, `LetGo`'s
  alias and readiness), `core/mcp/media/audio.rs` (its two stage frames), and
  one-line matches for the new frame in `lmgw-ui` (`chat_stream/read.rs`,
  `chat_turn.rs`). The `/mcp` dispatch tests are the unit tests in
  `core/mcp/ingress.rs` (there is no `tests/it/mcp_ingress.rs`); the vram phase
  case is in `tests/it/vram_admission/admission.rs`; `tests/it/device_chat/walk.rs`
  walks the new route.

**Slice 3 — the Chat UI** (needs slice 2's frames).
- new: `lmgw-ui/src/pages/chat_media.rs` + `pages/chat_media/{live.rs, card.rs,
  player.rs}`; `scripts/drive/chat-media.json`, `scripts/drive/chat-media.js`,
  `scripts/chat-media-drive.sh`.
- edits: `lmgw-ui/src/pages/{chat.rs, chat_stream.rs, chat_attach.rs,
  model_editors.rs}`, the stylesheet, `scripts/mock-openai.py`
  (`/v1/images/generations` answering a small PNG).
- **As built (2026-10-21), with slice 4's UI half:** the new files above, and
  `pages/chat_media/actions.rs`, `pages/model_editors/image_chat.rs`,
  `pages/image_recipes/upscalers.rs`, `scripts/drive/chat-media-live.json`;
  edits in `pages/{chat.rs (ToolCard's call id and live state, IrTool's id, the
  resource names, the page's MediaCtx), chat_turn.rs (progress by call id, a
  result's files, the regain line), chat_stream.rs (post_sse, the stream reader
  any SSE route uses), chat_stream/read.rs, chat_approvals.rs, chat_rows.rs,
  chat_voice/{status.rs, realtime/bubbles.rs}, image_recipes.rs, models.rs (the
  derived prompt style for the editor), model_editors.rs, mod.rs,
  audio_lab/kept.rs}` and `lmgw-ui/src/api.rs` (`fetch_file`, shared with the
  Audio lab); `chat_attach.rs` needed nothing. One core line: the Chat turn
  stream's keep-alive (`core/web/chat_turn/out.rs`, §14 item 54).

**Slice 4 — card actions and upscale.** Its core half can run beside slice 3 (no
shared files); its UI half follows slice 3.
- new: `core/web/chat_media/actions.rs`, `core/proxy/image_call/upscale.rs`,
  `core/mcp/media/upscale.rs`, `core/image_upscalers.rs` (the listing and the
  shipped entry), `lmgw-ui/src/pages/chat_media/actions.rs`.
- edits: `core/server.rs` + `core/web/mod.rs` + `core/openapi/planes/
  chat_attachments.rs` (the actions route), the recipes page's Upscaler entry
  (`lmgw-ui/src/pages/image_recipes.rs`), `lmgw-api-types/src/chat_media.rs`
  (the action body); `scripts/drive/chat-media.json` steps.
- **Core half as built (2026-10-21):** the new files above, as
  `core/web/chat_media/actions/{plan.rs, running.rs}`,
  `core/proxy/image_call/upscale/choose.rs` and `core/image_upscalers/add.rs`
  beside them; edits in `core/mcp/media/{list.rs, refs.rs, image.rs}` (the
  offer, a ref's maker, an edit's parent), `core/proxy/image_call/log.rs`,
  `core/store/chat_tool_files.rs` + `core/web/{chat_repo.rs, chat_temp.rs}`
  (binding an action file), `core/ops/recipes.rs` and the op's registrations
  (`openapi/ops/table/downloads.rs`, `web/{op_names.rs, api.rs}`,
  `mcp/selfadmin.rs` + its catalog), `lmgw-api-types/src/image_recipes.rs`;
  tests `tests/it/media_tools/{actions.rs, upscale.rs}`. The UI half was built
  with slice 3 (above).

## 11. Test plan

Tests are modules of the one `tests/it` binary per crate. The image backend's
fixtures are the template (`tests/it/image_backend.rs`: `FakePodman`, wiremock
standing in for sd-server on the allocated port).

Slice 1:
- `list`: the enum holds exactly the eligible rows (disabled, `vid_gen`-only, wrong
  task excluded); the description carries each row's prompt style (recipe-derived,
  column override, "not set"), its defaults and the cfg-1 note; an empty toolset
  fails its label with the per-label sentence; `image__`/`audio__` names from a
  southbound server are moved by the backstop; a key scoped to `kb__*` sees none.
- `image_call`: wiremock sd-server answers capabilities, `202` and a poll sequence
  (`queued` → `generating` → `completed`); the body maps every parameter (cfg as
  `guidance.txt_cfg`), omits unset ones, draws a seed when none was given; an edit
  on an `edit` row sends `ref_images`, on any other row `init_image` and never
  `ref_images`; `429`, `failed` and a job past the route's timeout come back as
  their errors; a transport error mid-poll resubmits once; one image row per call
  with `images_out`.
- `hold`: under the hold a row with a fallback is answered by a wiremock cloud
  `/v1/images/generations`, and the result names the alias and the parameters not
  applied; without a fallback the result is the hold error; a CPU audio row speaks.
- `chat_turn`: a scripted chat model (wiremock) calls `image__generate`, then
  answers; the file is stored with `source = 'tool'` and bound to the reply; the
  record holds the `lmgw:attachment/N` resource; the second model call of the turn
  and the next turn's first call carry the image (asserted on the chat upstream's
  request body); `image__edit image=att:N` on another thread's attachment is
  refused; the chat row's in-flight count is zero while the image call runs (the
  claim was let go) and it is admitted again for the next call; a refused persist
  leaves no tool file; drafts never list tool files.
- `audio_call`: speak and transcribe through the audio fixtures, one row each;
  `audio__speak` gives the chat model text only and `/mcp` an audio block.
- `migrations`: 0085 applies over a database with attachments and image rows.

Slice 2:
- `progress`: preview frames at `(pass, step)` changes only, `pass` 2 restarting
  the step, `Generating` without a preview when the mode is off; the waiter phase
  names the evicted model; regain sends `state` loading/ready when the chat model
  had been evicted.
- `cancel`: queued → cancel `POST` seen; generating with `cancel_generating` →
  cancel `POST` and the `cancelled` status awaited; without it, the only request
  in flight → `podman stop` of that generation seen by `FakePodman`; with another
  request in flight → no stop, the claim held until the job ends; dropping the
  call future (the turn's Stop) takes the same path; the cancel route answers the
  action it took.
- `mcp_progress`: a `tools/call` with a `progressToken` and an SSE `Accept` gets
  rising `notifications/progress` and the response; without either a JSON body;
  `notifications/cancelled` for the request cancels the job.
- Live, `scripts/media-tools-live.sh` (skips when no image row exists): one
  generation with `proj` previews against the real row, recording preview sizes;
  a cancel mid-generation with its timing and the VRAM back at baseline; the
  capabilities fixture of the running build. As built it runs four checks, each
  one generation (`STEPS`): a Chat turn at 512² with the default previews, an
  `/mcp` call with a progress token, a 512² turn with `vae` previews, and a 1024²
  turn cancelled at step 2; it writes the running build's capabilities as the
  gateway read them to `target/media-tools-live/capabilities.json` (the committed
  0518b34 fixture still matches, so none was added). Results in §2.

Slices 3 and 4 — `scripts/drive/chat-media.json` on a dev instance
(`scripts/dev-instance.sh`), run in Chrome and in WebKit: the chat model is
`mock-openai.py` with a `TOOL_SCRIPT` that calls `image__generate`; the GPU hold
is on and the image row's fallback is a cloud alias on the same mock, so the
whole path runs without a container. Checks: the card shows the facts and the
fallback, Save, Attach puts a draft in the composer, Re-roll adds a thumbnail,
the audio player appears for a scripted `audio__speak` on a mock TTS alias, and
the card fits at 720, 900 and 1280 CSS px at 125 % (`ui-matrix.py`). The live
pane's stages, previews and the restart confirm are checked against
`media-tools-live.sh`'s real row, where previews exist.

As built: `scripts/chat-media-drive.sh` starts the dev instance itself (the
headless binary, a scratch data dir, the `lmgw-media3` container prefix), sets
up the mock, the held row `image/mock-turbo` (an empty weights file named like
Z-Image Turbo's, so its prompt style is the recipe's), `cloud-image` and
`tts-mock`, and gives each engine a thread of its own. The drive also checks
the live pane before the picture, a Vary form prefilled with the prompt, a
refused Upscale said under the card, a cancel before any job (the call's result
says so, no picture), the player's source a WAV, all of it again after a
reload (the strip, the stored `image att:N`, no live pane), the editor's "In
the Chat" section (the derived style, the default in words, no config key) and
the recipes list's Upscalers. Results 2026-10-21: Chrome and WebKit (Broadway,
`--zoom 1.25`) 0 FAIL each, `ui-matrix.py --tauri --dsf 1.25` on a drive's
thread at 720, 900 and 1280 CSS px 0 FAIL. `--live --webkit`
(`scripts/drive/chat-media-live.json`, the spike's Z-Image-Turbo row on
`localhost/lmgw-sd-server:sdcpp-master`): one container start and two 512×512
pictures; the stages read "Waiting for the picture", "Loading
image/z-image-turbo", "Loading image/z-image-turbo · Generating", steps 2, 4,
7, 10, 13, 16, 18, 20 of 20 with quick-sketch previews, the card "seed 42 · 20
steps · cfg 1 · 512×512 · 2.1 s, plus 2.9 s loading"; the second picture's
Cancel said it restarts the model, the confirm said why, and the result said
the container was stopped; 0 FAIL. Screenshots at 125 % in
`target/chat-media-shots/` (not committed).

After the UI review (2026-10-21): the drive also opens the picture full size,
checks its tooltip, alt text and width (256 CSS px for a 256-pixel picture),
double-clicks Attach (one copy), leaves the thread for a second one while a
re-roll runs and comes back (the pane, Cancel and the held Re-roll are there,
and the picture lands in the strip), reads a cancelled call as cancelled, runs
a turn that draws twice with calls the mock sends without ids
(`TOOL_SCRIPT` `{"rounds": …}` and `"id": false`: one picture per card),
stops a turn mid-picture (the pane goes, the card says so), and checks every
stream's frames came whole within 0.6 s of their first byte. `--kwin` runs the
WebKit drive in a nested virtual KWin instead of Broadway. Results: Chrome,
WebKit under Broadway and WebKit in the nested KWin 0 FAIL each,
`ui-matrix.py --tauri --dsf 1.25` at 720, 900 and 1280 CSS px 0 FAIL.

## 12. Decisions taken in this draft — and what is open

Taken by the drafter (overturn any in review):

1. **Refs are `att:N`**, and every image and audio attachment is labelled with its
   ref in the model's view, whether or not a media toolset is attached (§3.6).
2. **Produced files are `chat_attachments` rows** (`source`, `call_id`) bound to
   the reply, and the record keeps `lmgw:attachment/N` resources, rehydrated per
   route (§6.3, §6.4) — rather than base64 in `ir_messages`.
3. **Every local image call uses the native job API**, previews or not; the
   OpenAI route is only for cloud routes (§4.1).
4. **lmgw draws the seed** when the call sets none, so every card shows a seed
   that reproduces it. The Image lab's ruling ("blank = the server's default 42")
   stays for the lab.
5. **Default preview `proj`** when offered; `tae` only with a TAE file and its
   quality note (§5.2).
6. **Poll at `CANCEL_POLL`** (250 ms).
7. **Restart only when the call is the row's only request**, abandon otherwise,
   with the claim held to the job's end; restart behind an inline confirm (§5.4,
   §8.1).
8. **A cancelled generation is a tool result and the loop goes on**, telling the
   model not to retry.
9. **`audio__speak` gives the chat model text only** (§3.5).
10. **`model` is required** even with one eligible row.
11. **Upscale's row choice** as in §9; the tool is offered only when the
    upscalers dir holds a file.
12. **Card action results are not shown to the model** unless attached (§8.2).
13. **A dropped `/mcp` connection abandons the call** (§7.4).
14. **Progress frames are lossy and not in the feed** (§6.2).
15. **Three prompt styles**, column over recipe, "not set" said as such (§3.4).
16. **Cloud routes get no smuggled parameters**: what the OpenAI image body cannot
    say is reported as not applied.
17. **A job whose container died mid-poll is resubmitted once.**

Answered by the owner 2026-10-21:

- The `model` enums **also list cloud image, TTS and ASR aliases**, each with its
  description line, not only rows (§3.2); a cloud route gets no smuggled
  parameters (decision 16).
- Card action results **do not** reach the model without Attach (decision 12, as
  drafted).
- The upscale row choice stands as drafted in §9 (decision 11).

## 13. Later

- **Video** (`vid_gen`, Wan 2.1 T2V 1.3B is the shipped recipe): same server, same
  job API, minutes per job and an encoded container as the result; it needs its own
  tool and card. Not in this build (decision 6).
- **Slot save and restore across evictions**: its own `docs/ideas.md` entry
  (decision 3); with it, the chat model's return after a picture would not
  re-prefill the thread.
- Async `/v1` image routes for API clients, with the job ledger image §13 describes.
- Masks and inpainting, LoRA, batch count, hires and ControlNet as tool parameters;
  the card has no mask painter yet.
- A per-row prompt note (trigger words) beside the prompt style.
- MCP Tasks for long northbound calls, and stream resumption for `/mcp`.
- An upscaler-only sd-server (no pipeline loaded, through sd-server's model
  management), so upscaling does not need a pipeline resident.
- Kai and kai-mobile: rendering assistant attachments and `progress` frames.

## 14. What building slice 1 changed

Recorded where the text above now says it; listed here so a reviewer sees what
differs from the draft of 2026-10-21.

1. **The enums list aliases too** (the owner's answer, §12): §3.2 names how an
   alias qualifies — the endpoints its configuration publishes (its model's name
   and its `capabilities_override`; the review round below replaced the cached
   catalog read, see 12). A cloud image alias needs a `capabilities_override`
   task, because a catalog's image model publishes no route.
2. **The claim is a hook, not a wrapper** (§4.3, §6.1): `media_claim` implements
   `image_call::Hooks`; the route the decision needs exists only inside the call.
3. **Files bind by the ids the turn stored** (§6.3), not by `call_id IN (…)`: a
   model may reuse a call id across turns.
4. **History refs are hydrated when the request is built** (§6.4), for both paths;
   the loop's `PerRoute::request` hydrates the turn's own.
5. **`audio__speak` uses the clause splitter, not the read-aloud's sentence
   batches** (§4.2): one request per clause, the WAVs joined.
6. **The result's time names the drawing** (§3.5): "4.7 s (1.1 s of it drawing)",
   the job's own time; a fallback's result names only the parameters the call set
   as not applied, and a cloud answer by URL is refused (lmgw fetches no URLs).
7. **The seed is drawn by the job body** (§4.1): a cloud route gets none, so its
   picture's facts carry no seed rather than one it did not use.
8. **Slice 1 sends no preview** (§5.2); the job loop reports progress to a hook
   nothing renders yet, and a stop ends the wait (its rows written, §6.5) without
   cancelling the job, which runs to its end on the image server — §5.4's guard is
   slice 2. (Superseded by items 34, 36, 41 and 51: slice 2 asks for previews,
   and a stop, a timeout or a stall cancels the job through the guard.)
9. **The inventory lists the `image` and `audio` sources also with no tools**, as
   it lists `docs` and `kb`; a label resolved with nothing behind it fails with
   §3.2's sentence.
10. **The model rows' label** follows the run's: `chat` for a Chat thread,
    `admin` for an Admin Chat thread (since item 62; it was `chat` before), the
    tool rows `chat-tool` in both, `realtime` for a session, `openai` for
    `/mcp`, `/v1/responses` and agents.
11. **Covered by tests since the review round:** the resubmit after a container
    death mid-poll (a wiremock on a listener of its own, dropped) and a second
    death's error, the chat model let go and admitted again around a picture on a
    local chat row (the Chat and `/v1/responses`, `gpu_world`'s image rows), a
    `kb__*`-scoped key reaching no media tool on `/mcp`, and a superseded turn
    leaving no unbound tool file. **Not covered yet:** an agent run's picture
    around its claim (the same `LoopClaim` as the two tested runners), and the
    second death's `mark_failed` (no observable in the registry's view).

The review of slice 1 (2026-10-21) changed:

12. **The tool block depends on configuration only** (§3.2): a cloud alias's
    endpoints from its model's name and its override
    (`capabilities::exposed::configured_endpoints`), not from whether a catalog
    was cached; an image row's probed facts remembered in memory after its
    container stops (`Registry::image_capabilities`), not only while it runs.
    Before a row's first start its line has no probed facts, so the block changes
    once then; the memory is per process, not persisted yet. Two later inputs
    change it too: a file arriving in or leaving the image models dir's
    `upscalers/` (item 27: `image__upscale`'s offer and its `upscaler` enum are
    the disk's), and a row's first probe, which brings its upscale size limit
    into `image__upscale`'s description (item 47).
13. **Every loop with a claim lets it go for a picture** (§4.3): the claim moved
    to `agent::claim`, `/v1/responses` and agent runs hold a `LoopClaim` and pass
    the `MediaClaim` hooks to their media executor; they held their claim through
    the call before, a circular wait when the image row did not fit beside the
    chat model.
14. **A refused regain is re-routed for a lost candidate too** (§4.3), not only
    under a GPU block: `CandidateLost` picks again through the gate.
15. **A Stop is the call's own** (§6.5): the loop hands each tool call a stop it
    may take (`agent::call_stop`), and the media executor takes it, so a stopped
    call writes its `canceled` model row and tool row instead of none.
16. **Keep rewrites the records' refs** (§6.3) to the kept files' new ids.
17. **A job the server no longer knows (404/410) is submitted again** (§4.1),
    spending the one resubmit a dead container gets.
18. **The OpenAI image route asks for base64** (§4.1 step 5): `response_format:
    "b64_json"` except for OpenAI's `gpt-image` models.
19. **The key's model scope narrows the enums** (§3.7), on `/mcp`, in a label's
    resolve and in a device's turn.

Building slice 4's core half (2026-10-21) changed:

20. **The action stream's frames** (§8.3): `action {call_id, action, model?,
    source}` first (`model` absent for an upscale, whose row is picked when it
    runs; `progress` and `done` name it), then `progress`
    (`MediaActionProgress`: `phase` `queued | generating | upscaling`, `model`,
    `pass`/`step`/`total_steps` when the build reports steps, `queue_position`,
    `elapsed_ms`; lossy, `try_send`), then `done {call_id, attachment, text}` or
    `error {message, code}`. `MediaActionProgress` carries the fields of §6.2's
    tool progress frame that an action has today, with no `preview` and no
    `cancel` yet; slice 2's frame type replaces it once it lands, field names
    unchanged. `upscaling` is sent once the row is admitted, its upscalers
    rescanned where it reported none (item 46) and the size limit checked (item
    47) — as built, right before the request goes out. (The frame type is
    superseded by item 41; the `upscaling` frame carries a `cancel` kind since
    item 63.)
21. **Refused before any frame**: `404 not_found`, `422 not_an_image`, `409
    not_sent` for a picture that belongs to no message yet (a draft, or a reply
    still being written: "bound to the source's message" needs one), and `422`
    `no_model`, `model_unavailable` (the maker is disabled, removed or outside
    the key's scope, read from the same offer the tools list), `not_rerollable`
    (no recorded prompt, or an upscale), `source_missing`, `wrong_action` (vary
    on an `edit` row, edit on a variation row; a cloud alias edits by
    instruction), `prompt_required`, `no_upscaler`, `no_image_model`,
    `unknown_upscaler`, `bad_request`.
22. **An action runs in its own task**: closing the stream does not stop it, and
    its picture is stored all the same. Its stop is in a process-wide table by
    `act-<n>` (`web::chat_media::actions::stop(thread, call_id)`), which slice
    2's cancel route calls; until it lands nothing raises it, and a raised stop
    ends the wait without cancelling what the image server started. (Superseded
    by items 41 and 44: the table and `actions::stop` are gone; an action's stop
    is in the media-call registry, which the cancel route and a thread's going
    raise.)
23. **What an action sends**: a re-roll sends the recorded prompt, negative
    prompt, size, steps, cfg and strength, leaves out what the answering route
    did not apply (`not_applied`) and the seed (drawn anew); a re-roll of an
    edit edits the recorded input again (`source_missing` when the input was a
    `data:` URL). Vary and edit send the recorded settings with the body's
    prompt and strength and no size: sd-server infers it from the input.
24. **`meta.media.parent`** is what the picture was made from: an edit's input
    (the tool's `image__edit` now records it when its input was an `att:N`), the
    re-rolled picture for a re-roll of a generation, the source for an upscale.
    An upscale's `job_ms` is the upscale request's own time.
25. **Upscale is local only and never falls back** (§4.1 step 5): the hold's
    swap at resolve is refused (`upscale_needs_local`, "an upscale has no
    fallback"), and admission is pinned to the row (`admit_pinned`). A row whose
    container reports `upscale: false` is refused before the request with the
    reason — no file, or a file that arrived after the container started.
    (The last sentence is superseded by item 46: such a row is asked again
    once held, and refused only if it still reports none.)
26. **sd-server reads the upscalers when it starts** (measured 2026-10-21, the
    `0518b34` build): with the file in `upscalers/` before the start,
    capabilities report `upscale: true` and `RealESRGAN_x4plus` with
    `image_upscale: true`, `max_upscale_width/height` 8192. Whether a running
    container sees a file that arrives later (sd-server's
    `POST /sdcpp/v1/models/refresh` rescans files) was not tried; lmgw says
    "from its next start" until it is. (Superseded by item 46: the server's
    source rescans on every capabilities read.)
27. **The upscaler listing reads the default directory**: a row whose `files`
    set its own `hires_upscalers_dir` reads another one, and the enum does not
    list that one yet. The tool block changes once when a file appears in or
    leaves `upscalers/` — the enum is the disk's.
28. **The shipped entry** is a new op, `image_upscaler_add {key}`
    (`lmgw__image_upscaler_add`; 89 ops), and `image_recipes` answers
    `upscalers {shipped, on_disk, dir}`. It checks the file in the hub's listing
    and queues it through the download queue with `upscalers/<file>` as its
    destination, so the Downloads page, retry and update see it.
29. **Live, 2026-10-21** (dev instance, the spike's Z-Image-Turbo row,
    `localhost/lmgw-sd-server:sdcpp-master` `0518b34`): one 512² generation 4.4 s
    with the container start (1.1 s drawing); the card's Upscale of it with
    `RealESRGAN_x4plus` made a 2048x2048 PNG of 6,525,028 bytes in 2.41 s of
    upscale request (2.45 s call, 2.54 s on the wire), one image row labelled
    `chat` with `images_out` 1, the file bound to the source's message as
    `source = 'action'`.
30. **Not covered by tests yet**: an action on a temporary thread, a raised
    action stop (no route raises it yet), an `edit` row's Edit action, and a
    device key's action (the route takes the device's context like every Chat
    route; the policy check is `image_call`'s own). (Items 43 and 44 cover the
    first two now.)

Building slice 2 (2026-10-21) changed — items marked *call* are the builder's choices, open to review (like §12's):

31. **A caller follows its own waiter** (§5.3): `stage_watch::follow` puts a
    task-local watch on the waiter its admission enqueues, instead of reading the
    `vram` telemetry broadcasts, which go out at enqueue and dequeue only and
    cannot tell two waiters for one model apart. `WaitPhase` is an API type and
    the view's queue carries it.
32. *Call:* **`gate`, `measuring` and `starting` are all `loading`** on the
    frame, and the image call says `loading` itself before an admission whose
    row has no ready container, so a gateway without GPU telemetry still shows
    it.
33. **`/mcp` progress never repeats a value** (§7.2): MCP requires it to rise,
    so each message before the first step moves it half-way to 1; a report that
    would not raise it is dropped. *Call:* the halving.
34. **Steps for `/mcp` need a preview** (§5.2): seen live, a job asked for no
    preview reports no steps, so a call followed for its steps asks for `proj`
    and drops the frames. *Call:* `proj` rather than the row's mode there, and a
    row set to `none` asks for none for anyone. The `/mcp` step counting after
    this change is covered by tests, not by a second live run (the run's four
    generations were spent).
35. **SSE only for tools that report progress** (§7.2): `ToolPlane::
    reports_progress`, so a southbound tool's answer keeps its JSON body even
    when its client sent a token and takes SSE.
36. **The route's timeout cancels the job too** (§5.4): as built, the call
    tells its guard (`LetGo::Timeout`) and drops it with the job still in it,
    which cancels as a stop's drop does, and the error says what became of the
    job. Since item 63 an upscale's timeout does the same, its claim held to the
    real end where the upscale is abandoned.
37. *Call:* **a cancel before the job exists answers no `cancel`** (§6.2, §6.5):
    the stop then only ends the wait, and the frame carries `cancel` from the
    first job poll on. A job submitted but not yet polled counts as queued.
38. **The progress frame names its call by `call_id` only** (§6.2): the
    executor knows the id, not the index.
39. **Regain reports are a trait on `LoopClaim`** (`RegainReport`), not the
    turn's `Events` itself, so `agent::claim` stays free of the web layer.
40. **Covered by tests:** frames at `(pass, step)` changes with previews and a
    new pass, no step without a preview object, the eviction naming its victim
    and the regain's `state` frames (`gpu_world`), each cancel row of §5.4's
    table through the route (native queued, native generating followed to
    `cancelled`, restart seen as `podman stop` of the row's container, abandon
    holding the claim until the job ends), the turn's Stop taking the restart
    path, rising `/mcp` notifications before the answer, a JSON body without the
    token or the `Accept`, `notifications/cancelled` stopping a job, a card
    action cancelled through the route, and an upscale's restart. **Not covered
    yet:** a regain refused after a picture (its `held`/`failed` frame); the
    `409` fallback and the follow after a timeout are covered since item 51.
41. **Card actions share the registry and the frame** (§6.5, §8.3; slice 4's
    items 20 and 22 planned it): `MediaActionProgress` is gone, an action's
    `progress` frames are `ToolProgressFrame` with previews and `cancel`, and an
    action registers its stop in `media_calls` by its `act-<boot>-<n>` id, so the one
    cancel route stops both; the actions' process-wide table and
    `actions::stop` are gone. An action asks for the row's preview mode, as a
    Chat tool call does.
42. **An upscale is cancelled by §5.4's rule for a request with no job**: the
    row's container stopped when the upscale is its only request; otherwise
    (*call*) the upscale finishes on the image server and its claim goes at
    once, since a dropped request cannot be followed to its end — the requests
    sharing the row keep it resident meanwhile. Its frames are the admission's
    stages and then `upscaling`, sent by the call itself (not the action's
    hooks) so the `image__upscale` tool has them too; they carry no `cancel`
    kind yet, so the cancel route answers an upscale's cancel without one.
    (Superseded by item 63: the abandoned upscale holds its claim to its end,
    and its frames carry the `cancel` kind.)

The review of slice 4's core half (2026-10-21) changed:

43. **Keep carries each picture's parent** (§6.3): `meta.media.parent` gets the
    kept parent's new id (`store/chat_keep/refs.rs::meta`), after every file is
    in, whatever their order; a parent that was not kept is taken out rather than
    left naming a temporary id. A re-roll of a kept edit (the tool's or an
    action's) edits its kept input again; it answered `422 source_missing` before.
44. **A thread that goes stops its media calls**: where a deleted, kept,
    folder-deleted or purged thread's live turn is cancelled
    (`LiveTurns::discarded`'s callers: `ChatRepo::keep`, `delete_thread_held`,
    the folder delete, the chat sweep), `MediaCalls::thread_gone` raises the stop
    of every image and audio call registered for the thread (item 41's
    registry), card actions included; a running job is then cancelled by its
    guard as a cancel's is. The action's `error` frame says `thread_gone`
    ("deleted, or kept as a stored chat, while the action ran … run it again on
    the kept chat"), and so does an action that finds its thread gone when it
    stores or binds its picture, instead of "the message was deleted" or a store
    error.
    Keep also leaves out every temporary file no message took
    (`TempAttachment::kept` is `None` for an unbound `tool` or `action` file):
    a stored thread never holds a `message_id NULL, source <> 'upload'` row from
    a Keep. A stopped action's picture is not kept, as a cancelled turn's reply
    is not.
45. **A tracked download is its file at its place**: migration 0086 rebuilds
    `hf_models` with `UNIQUE (repo, file, target, dest_path)`, and
    `upsert_hf_model_at` re-queues a row only at the same place. The shipped
    upscaler's `upscalers/<file>` and an `hf_add` of the same repo file at
    `<repo>/<file>` are two rows in either order, each updated, retried and
    deleted on its own; before, the second queue moved the first row's
    `dest_path` and left the first copy untracked. The recipe lookups in
    `image_recipes` / `image_recipe_add` match the place too.
46. **A server that last said `upscale: false` is asked again** (§9): upstream
    sd-server rescans `--hires-upscalers-dir` on every `GET
    /sdcpp/v1/capabilities` and every `POST /sdcpp/v1/upscale`
    (`refresh_upscaler_cache` in `examples/server/routes_sdcpp.cpp`, read
    2026-10-21 at master `0518b34`, the build item 29 ran; `models/refresh`
    rescans the recipe catalog and is not needed). So once the chosen row is
    held, a cached `upscale: false` is re-read from the container, the registry
    keeps the new document (`Registry::refresh_image_capabilities`), and only a
    server that still reports none is refused — with a reason that no longer
    says "from its next start". Not measured live yet against a running
    container; the source and a fixture test say it.
47. **The size limit is checked before admission** (§3.3): the input's size
    (PNG, JPEG, WebP or GIF header) times the upscaler's scale, once per repeat,
    against the chosen row's probed `limits.max_upscale_width/height` (the
    server's own per-run check) → `422 too_large` naming the input, the result
    and the limit. The scale is the shipped entry's figure or the name's `x4` /
    `4x` word; with no `upscaler`, the smallest scale on disk when every file
    says its scale. A row not probed yet is checked once it is held; a picture
    or upscaler whose size or scale lmgw cannot read leaves the check to the
    server, whose `400` reaches the caller. `image__upscale`'s description now
    states each eligible row's limit (`Offer::upscale_limits`, narrowed by the
    key's scope like the rows): one sentence when every row names the same,
    each row's ("not known yet" for one never probed) when they differ — it took
    the first row's before.
48. **Action ids across restarts**: `act-<boot>-<n>`, `<boot>` eight hex
    digits (32 random bits) drawn once per process, `<n>` a counter; a later
    process repeats an id a stored action file carries as its `call_id` only if
    it draws the same 32 bits, about one chance in four billion per restart.
49. **A card action's picture is not the model's by ref** (the owner's
    decision, §8.2): `att:N` never resolves to a `source = 'action'` file and a
    refused ref's list never names one (`ThreadFiles`,
    `store::chat_media_attachment_ids`). "Attached" means what §8.2's Attach
    does: a copy in the composer as an upload, with an id of its own, which
    resolves like every upload. The card's Attach makes that copy (item 57).
50. **A revoked key stops its own action**: the action's task watches the
    caller's key (`devices::watch`, from the request's revocation mark) and
    raises the action's stop on a Disable, Rotate, Delete or expiry; the model
    row says `canceled`, what was used until then is charged to that key, and
    nothing is stored; the job is cancelled by its guard (§5.4) as a cancel's
    is.

The review of slice 2 (2026-10-21) changed:

51. **A followed job is bounded by itself, not by the call's start** (§5.4,
    review of slice 2): the guard's follow used the route's ceiling from the
    call's start as its deadline, so a timed-out call let its claim go at once
    while the job still drew, and with no ceiling a hung image server was
    polled forever holding the claim. The follow now ends with the job, with
    its server's answers, or when the job stalls by its own pace
    (`image_call/pace.rs`, `STALL_PACES` 20, *call*: the factor, named in each
    warning with the figures it was applied to). *Call:* the call's own follow
    judges the pace only on a route with no ceiling. *Call:* a caller that
    shows no previews asks for `proj` too (§5.2; `Follows::Nothing` is gone),
    so most jobs have a pace; a job with none is followed while its server
    answers. The timeout's error says what became of the job instead of
    "upstream timeout". Covered by tests: the pace (judged after two steps, the
    queue left out, a poll that does not answer), a timed-out job followed
    holding its claim while it draws and let go when it ends, a timed-out
    job's row restarted, a stalled job on a route with no timeout given up and
    its row restarted, a `409` falling back to the restart, and the restart
    race (a request that joined between the decision and the stop keeps the
    row: the stop is refused and only the cancelled claim goes).
52. **Eviction stage words keep the figures** (§5.3): see §5.3's as-built note;
    `set_stage` clears the phase, and `StageWatch` sends a phase only when it
    differs from the last one sent.
53. **`/mcp` hardening** (§7.2, §7.3): the progress stream's keep-alive; the
    cancelled notification handled in `ingress/progress.rs` with the stream and
    the call's answer (moved out of `ingress.rs`); southbound calls not
    cancelled yet (*call*: worded as current state rather than forwarded in
    this pass). Covered by tests: a cancel from another session (or naming `"2"`
    for `2`) leaves the call running, at dispatch level too; a tool that
    reports no progress answers JSON despite a token and a stream `Accept`; a
    device that cannot reach the thread gets `404 not_found` from the cancel
    route and the call runs on.

Building slice 3 and slice 4's UI half (2026-10-21) changed — items marked
*call* are the builder's choices, open to review (like §12's):

54. **Every Chat SSE stream sends one comment a quarter second after each
    burst of frames** (`web/chat_caller/tail.rs`, `Caller::sse_response`: the
    turn, the turn with read-aloud, the read-aloud alone, a warm and a card
    action; `TAIL_FLUSH` is `agent::CANCEL_POLL`), then nothing more from
    there; axum's 15 s keep-alive keeps a quiet stream open. WebKitGTK 2.52
    hands a page a frame's tail only once more bytes arrive: a tool call's
    285-byte `ready` frame reached the page 2.57 s late, with the call's
    result, while a cloud picture was drawn, so the live pane's Cancel had no
    call id until then. Measured with the drive's own reader (each frame's
    first byte and its end, `__cm.wire()`) on the mock path: 2574 ms on a real
    Wayland frame clock (a nested virtual KWin, GPU compositing,
    `chat-media-drive.sh --kwin`) and 2584 ms under Broadway, 0 ms in Chrome;
    with the flush 246 ms and 294 ms, still 0 ms in Chrome. So it is WebKit's
    reading, not Broadway's. An earlier pass of this slice sent a comment
    after every quiet quarter second on the turn stream only, to every
    caller. The golden transcripts (`tests/it/chat_golden.rs`) leave the
    comments out, as where they fall is the clock's.
55. *Call:* **the live pane and the picture share one frame**: the asked size's
    aspect ratio, as wide as the column allows up to 448 CSS px, never taller
    than half the window, so the card's facts and buttons stay beside the
    picture; the stage line, step and Cancel under it. "Waiting for the
    picture" until the first frame (a cloud route sends none).
56. *Call:* **Vary or Edit comes from the catalog** (§8.2's as-built note):
    the page does not read image rows; a `wrong_action` refusal flips the
    choice for the card.
57. **Attach is the page's copy**: the picture fetched as a blob (never
    copied into the page's wasm memory) and uploaded as a draft through the
    composer's own upload (`POST /chat/api/threads/{id}/attachments`), so the
    upload's checks and the composer's chips apply as to any file; no core
    route was added. Its chip shows at once, the copy goes to the thread open
    at the click, and the button is held until the copy is in.
58. *Call:* **Upscale is offered on every picture but an upscale's**: the
    offer (an upscaler on disk, a local row the caller may use) is not on the
    page; the confirm says it runs locally only, and the gateway's refusal is
    said under the card.
59. **Stored results name their files** (`tools_from_ir`): a
    `lmgw:attachment/N` resource reads `image att:N` / `audio att:N`.
60. **The recipes' upscaler note follows item 46**: a running image model
    finds a new file at its next Upscale, not "from its next start" as the
    slice's plan still said.
61. **Not built yet:** a player in an uploaded audio chip (§8.4); the
    upscaler and repeats choice on the card (it takes the server's first
    compatible upscaler, once); a cancel of an action before its `action`
    frame; a mock drive of the restart confirm (only a local row offers a
    restart: `chat-media-live.json` reaches it). **Covered by tests:** the UI's own (`chat_media` and `chat_turn`
    unit tests: grouping, the loose cards, stage words, the live state, a
    progress frame by call id, a result's files, the regain line, preview-mode
    availability) and the drives of §11.

The review of all four slices (2026-10-21) changed — items marked *call* are the builder's choices, open to review (like §12's):

62. **An Admin Chat thread's media rows are `admin`** (§3.7, §8.3, item 10):
    the media tool calls' model rows were labelled `chat` while the thread's
    model turns, voice, attachment reads and card actions were `admin`
    (`speech_proto`); they now carry the thread's label
    (`MediaExecutor::with_model_proto`), so an Admin Chat thread's spend is one
    identity, `internal:admin-chat`. The tool rows stay `chat-tool`, as every
    tool of the thread. Covered by a test (`media_tools/labels.rs`).
63. **An upscale stopped, timed out or on a silent server** (§5.4, items 36 and
    42): the route's timeout returned from `send_local` and the guard was told
    the call had ended, so nothing was cancelled and the claim went while
    sd-server still upscaled. The request now runs on a task the guard keeps:
    the timeout takes the stop's rule (restart where it is the only request,
    else followed to its end holding the claim), as does a stop, which let the
    claim go at once before; the frames carry the `cancel` kind the guard would
    choose, so the Chat's confirm for `restart` shows for an upscale too.
    *Call:* the liveness read is the readiness route, every `CANCEL_POLL`.
64. **A server that accepts and then says nothing is given up** (§5.4): each
    status read is bounded by `STATUS_ANSWER`, `vram::CONTROL_TIMEOUT` (5 s),
    named in the warning and the error; before, a row with no pace (preview mode
    `none`) on a route with no ceiling, or a synchronous upscale, was polled or
    waited on forever holding the claim. *Call:* the existing control-read
    bound rather than a figure of its own, and one unanswered read is enough. A
    follow's stall now restarts the row where its claim is the only request,
    as the call's own stall does, rather than only letting the claim go.
65. **Keep rewrites the call arguments' refs too** (§6.3, item 16): an
    `image__edit {"image": "att:-3"}` in a kept record names the kept file.
66. **The pace's warm-up is named**: `JUDGED_AFTER_STEPS` (2) beside
    `STALL_PACES`, both in the stall warning.
67. **Covered by tests:** an upscale's timeout restarting its only user's row
    and, beside another request, holding its claim until the upscale ends; an
    upscale on a server that stops answering; a pace-less job whose polls go
    unanswered (all `media_tools/upscale_stop.rs`, the silent ones about 5 s
    each); an upscale cancel answering `restart` from its frames; Keep's
    rewrite of a call's arguments (unit and `media_tools/keep.rs`).

The UI review of slices 3 and 4 (2026-10-21) changed — items marked *call*
are the builder's choices, open to review:

68. **A call the upstream sent without an id gets a minted one**
    (`egress/call_ids.rs`, Gemini's minter shared with the OpenAI decoder,
    streamed and whole): `call_<12 hex digits><n>`, unique across the tool
    loop's model calls. Each call's `call_{index}` gave a reply two
    `call_0`s, and both cards showed both pictures. The page copes with a
    record that still has a repeated id: a `progress` frame goes to the newest
    card of that id without a result, a card shows its call's files that its
    result names (`att:N`), else one per card in order, and a stored record
    pairs each result with the first call of its id that has none yet.
69. **A turn's end settles its running cards** (§8.1's as-built note).
70. **The picture card** (§8.2's after-review note): tooltip, alt text,
    natural width, the action's pane beside the strip.
71. **A running card action is the page's** (`chat_media::actions::Run`, in
    `MediaCtx`), found again by the family's pictures; its picture goes to the
    message and the pick of the card that shows it when it lands, and the
    page closes the action streams when it goes (the actions run on, as a
    dropped stream leaves them).
72. *Call:* **the card actions wait for the stored reply**: a click while the
    reply was written was refused ("belongs to no message yet"), which the
    rig ran into right after a picture landed.
73. *Call:* **a cancelled call is "cancelled"**, read from its result's
    first words (`CANCELLED_BY_USER`, shared by the gateway and the page),
    and the turn's own stop "stopped"; both in the colour of a declined call.
74. **The Upscalers' "Lands in" line** is the size of the lines beside it
    (`.cat-pkg-meta` at the small size, which the image recipes' component
    notes share).
75. **Drives** (§11's after-review note).

Testing the tools in a live thread (2026-10-21) changed:

76. **`image__edit` offers only models that edit** (§3.2, §3.3;
    `mcp/media/edit_rows.rs`). Asked to add objects to a Z-Image-Turbo
    picture, a chat model called `image__edit` on that row, which then went as
    an img2img variation, and the user got the same picture back. The
    evidence (the request body and the stored result): the body
    carried `init_image` and the model's `strength: 0.3`, sd-server returned
    a new PNG (different bytes; mean pixel difference 6 of 255, 12 % of the
    pixels moved by more than 16), and the composition was untouched. That
    is img2img doing what it does: sd.cpp (`src/pipeline/image.cpp`) runs
    `floor(sample_steps × strength)` steps from the matching point of the
    schedule — 2 of Z-Image-Turbo's 8 at 0.3, from a nearly clean latent — so
    it redraws texture and cannot add or remove things. The plumbing was
    right; the offer was wrong. Now: the enum holds the `edit` rows and the
    aliases with the edits route only; no such model, no `image__edit`, and
    `image__generate` says none of its models edits; a call naming a row that
    cannot edit is refused with the names of the ones that can (or that
    there are none); the tool has no `strength`. `proxy::image_call` splits
    the action: `Action::Edit` refuses a row without `edit`
    (`not_an_edit_model`) instead of falling back to img2img, and
    `Action::Vary` is the variation, which only the card's Vary sends (and a
    re-roll of a picture recorded as `image__edit` on a row without `edit`,
    which was a variation). A variation that lands on a cloud route (a
    fallback) still goes as an edit there, its strength said as not applied.
    **A result that is its input** byte for byte (`image_call/unchanged.rs`)
    is the call's error, `unchanged_image`: no tool result says "made", no
    card stores it, and its log row says 502. Byte equality is the whole
    check; a pixel-equal re-encode is not caught yet.

The default models (§3.8), built 2026-10-21 — items marked *call* are the builder's choices, open to review:

77. *Call:* **a folder's media defaults are live, not copied** (§3.8): the
    request's "thread > folder > global" read as a resolution at each turn;
    copying them into new threads (the rule for the folder's other defaults)
    would leave every existing thread on the old value and make "Inherit" in a
    thread mean Settings, not its folder.
78. *Call:* **drawing and editing have a default each**, not one image default
    that edit takes only when the row can edit: `image__edit` offers only the
    models that edit (item 76), and the row that draws best is usually not one
    of them. The edit default resolves only to a model of the edit enum; one
    that cannot edit is refused on save, and a stored one whose row lost its
    Edit switch is shown unavailable ("cannot edit pictures"). Without an
    edit default, `image__edit` has none, whatever the drawing default is.
79. *Call:* **an empty override is "no default"** in a folder or a thread
    (`""`, the picker's "None — the model chooses"), distinct from "Inherit"
    (absent): a thread can make its chat model choose again whatever Settings
    say.
80. *Call:* **the default voice goes with the default speech model only**
    (§3.8): it applies when the call's model is that model and the model has
    the voice; a cloud alias is trusted, since lmgw cannot list a provider's
    voices. A voice inherited from another level than the speech model it is
    used with is said unavailable when that model is a row without it; with a
    cloud alias lmgw cannot tell yet.
81. **Not built yet:** a realtime session bound to a Chat thread uses
    Settings' defaults, not the thread's and its folder's; an agent manifest
    has no defaults of its own.
82. **Covered by tests** (`tests/it/media_tools/defaults.rs`): a Settings
    default on `/mcp` (schema, description, a call without `model`, an explicit
    one winning), a refused name with the choices, a stale default (required
    again, the reason, kept as stored), thread > folder > Settings through the
    route and the turn's tool list, a Chat turn whose call names no model, a
    key's alias scope leaving the default out, and the default voice with its
    model and not with another. Drives: scripts/drive/chat-media.json's last
    steps (Settings, a folder, the thread, a cloud speech model's voice, a
    turn), §11.

The review of the default models, 2026-10-21:

83. **A run's calls use the defaults its tool block named** (§3.8): the
    turn's list was resolved once, but each call read the levels again, so a
    settings change mid-turn could run a model-less call on another model
    than the description promised, or fail it as "model is required". Now
    `Resolved::media_defaults` goes to the executor for the Chat, `/v1/responses`
    and agent runs; `/mcp` resolves per request.
84. **Fixed:** a key's scope narrows the speech rows' voices too (the view
    showed a device every row's voices, and a save took a voice only an
    out-of-scope row had); `MediaDefaults` refuses an unknown field on input;
    a name of the wrong kind says "<name> is not a model <tool> can use" (the
    image reason ran for any field, the audio one said "a asr model now, not
    asr"); the route resolves a device's thread with its hidden folder as the
    turn does; a voice the level's speech row cannot use is refused naming
    its voices; a folder's `defaults_patch` merges `media_defaults` per field;
    the cloud voice box keeps "The model's default voice" apart from Inherit.
    Tests: `tests/it/media_tools/defaults.rs` (scope and level voices, unknown
    field on all three inputs, wrong-kind reasons, the hidden folder, a
    settings change mid-turn, the folder patch); drive steps for the cloud
    voice box.
