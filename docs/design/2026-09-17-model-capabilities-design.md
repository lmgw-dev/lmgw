# Model capabilities on `/v1/models` + reasoning control — design (2026-09-17)

Revised after adversarial review (same day); changed decisions are marked
*(rev)*.

Request: `/v1/models` must tell a client what each model can do — modalities,
whether it reasons, whether effort is selectable and which levels exist, the
configured output cap, the tool-call format — plus agent-readable explanations
(how to send audio, how reasoning/effort are controlled), so an application
built against lmgw needs no human and no MCP access to learn any of it. The
consuming application's own minimum spec is
`~/workspace/agents/super-specific-solo-loop/docs/notes/lmgw-capability-fields.md`
(field names `max_output_tokens`, `capabilities.reasoning{kind,enabled,levels,default}`,
`capabilities.tool_calls{kind,parallel,format}`; "unknown is better than guessed").

## 1. Summary

1. Every model object on `GET /v1/models` (and a new `GET /v1/models/{id}`)
   gains `max_output_tokens`, a `capabilities` object and a `notes` array.
   Both dialects (OpenAI-shaped, Anthropic-shaped via `anthropic-version`).
2. The list response gains a top-level `lmgw` object: gateway-wide notes —
   endpoints, the control headers, the fallback / hold semantics — so the
   per-model notes can stay short.
3. Local chat models derive their capabilities **from the GGUF**: the chat
   template (`tokenizer.chat_template`) for reasoning / effort / tools, the
   configured mmproj's header (`clip.has_vision_encoder`,
   `clip.has_audio_encoder`) for modalities, joined with the row's
   llama-server params (`--reasoning`, `--reasoning-effort`,
   `--reasoning-budget`, `chat_template_kwargs`, `--mmproj`, `n_predict`).
   Cloud models take what their catalog publishes (OpenAI-protocol catalogs in
   the Kilo/OpenRouter shape, Gemini `models.list`, Anthropic `models.list`),
   nothing more. An alias onto a local model derives from the backing row
   *(rev)*.
4. Reasoning becomes controllable **per request on every route** through
   three headers (`x-lmgw-reasoning`, `x-lmgw-reasoning-effort`,
   `x-lmgw-reasoning-budget`) and through each dialect's native body fields,
   translated per egress. Today only an OpenAI-shaped `reasoning_effort`
   accidentally reaches an OpenAI-shaped upstream; Anthropic `thinking` and
   Responses `reasoning.effort` are dropped. A control the route cannot
   express is named in a response header, never dropped silently.
5. Audio becomes a chat input modality: an `input_audio` content part is
   accepted and forwarded (llama-server, Gemini). Without this, advertising
   `audio` for gemma4-e4b-mm would be a lie — lmgw rejects the part today.
6. Local models get an `n_predict` param (`--n-predict`) so "configured
   maximum output tokens" is a real, owner-set number.
7. *(rev)* The Anthropic egress' hidden `max_tokens = 4096` default goes: the
   default becomes the catalog's `max_tokens` for that model, and when that is
   unknown the substitution is surfaced (response header + log).

## 2. Wire shape

### 2.1 OpenAI shape (`GET /v1/models`, `GET /v1/models/{id}`)

```json
{
  "object": "list",
  "data": [
    {
      "id": "qwen3.8-27b-reason-maxctx",
      "object": "model",
      "created": 1789598772,
      "owned_by": "llama-server",
      "context_length": 130000,
      "max_output_tokens": 32768,
      "pricing": {"prompt": "0", "completion": "0"},
      "capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions", "/v1/messages", "/v1/responses", "/v1/completions"],
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "vision": true,
        "reasoning": {
          "kind": "levels",
          "enabled": true,
          "levels": ["low", "medium", "high", "xhigh"],
          "default": "low",
          "can_disable": true,
          "preserve_history": true,
          "control": ["x-lmgw-reasoning", "x-lmgw-reasoning-effort", "reasoning_effort", "chat_template_kwargs.enable_thinking"]
        },
        "tool_calls": {"kind": "native", "parallel": true, "format": "qwen-xml"},
        "structured_output": {"json_schema": true, "json_object": true},
        "source": "gguf+config"
      },
      "notes": [
        "Reasoning is ON by default at effort 'low' (--reasoning on, --reasoning-effort low). Per request: header x-lmgw-reasoning-effort: medium|high|xhigh, or body reasoning_effort; x-lmgw-reasoning: off (or reasoning_effort: \"none\") turns it off.",
        "The reasoning trace comes back as message.reasoning_content (delta.reasoning_content when streaming) on /v1/chat/completions, as a thinking block on /v1/messages, as a reasoning item on /v1/responses.",
        "Images: OpenAI image_url part (https URL or data: URI; jpeg/png/gif/bmp) or Anthropic image block. Projector: unsloth/Qwen3.8-27B-GGUF/mmproj-BF16.gguf.",
        "Tools: send OpenAI tools/tool_choice or Anthropic tools; llama-server parses the model's own <tool_call> XML into structured tool_calls. Never hand-format calls.",
        "max_output_tokens is the configured --n-predict: llama-server stops a response there even when the request asks for more."
      ]
    }
  ],
  "lmgw": {
    "version": "0.1.48",
    "endpoints": {
      "openai": ["/v1/chat/completions", "/v1/completions", "/v1/responses", "/v1/embeddings", "/v1/audio/speech", "/v1/audio/transcriptions", "/v1/models", "/v1/models/{id}"],
      "anthropic": ["/v1/messages", "/v1/messages/count_tokens", "/v1/models", "/v1/models/{id}"],
      "other": ["/v1/rerank", "/v1/tasks/run", "/v1/tasks/stream", "/tokenize", "/v1/count_tokens"]
    },
    "headers": {
      "x-lmgw-reasoning": "on|off — per-request thinking switch; wins over body fields.",
      "x-lmgw-reasoning-effort": "an effort level (see capabilities.reasoning.levels of the model); 'none' = off.",
      "x-lmgw-reasoning-budget": "integer thinking-token budget; 0 = off. Expressible on llama.cpp, Gemini and budget-style Anthropic models only.",
      "x-lmgw-fallback": "RESPONSE: the alias that actually answered while the GPU hold re-routed the request.",
      "x-lmgw-reasoning-ignored": "RESPONSE: comma-separated controls (enabled, effort, budget) the route could not express; absent when everything was applied.",
      "x-lmgw-max-tokens-defaulted": "RESPONSE: the max_tokens lmgw had to invent because the request set none, the route requires one, and the catalog publishes no maximum."
    },
    "notes": [
      "Model ids without a '/' are local llama.cpp models; 'embed/…' local embedders and rerankers; 'audio/…' local audio.cpp models; other prefixes are cloud upstreams passed through.",
      "A 503 whose error code is gpu_hold means local models are deliberately paused by the owner; cloud aliases still work. Do not retry in a loop.",
      "Local models start on first request; the first response after idle can take tens of seconds (weights load).",
      "capabilities.source says where the facts came from: gguf+config (read from the model files' chat template and projector header — a text heuristic over the template, cross-checked against llama-server /props by lmgw__local_model_test — plus the owner's settings), catalog (what the provider publishes), config (owner settings only), owner (set by hand). A missing capability field means unknown — do not guess."
    ]
  }
}
```

Field rules:

- `max_output_tokens` — present only when a real number exists: the local
  row's `n_predict`; an OpenAI-protocol catalog's
  `top_provider.max_completion_tokens`; Gemini `outputTokenLimit`; Anthropic
  `max_tokens`. Never derived from the context size — the consuming spec
  reads it as "one response may generate this many", and "the whole remaining
  context" is not that number. A local model without `n_predict` has no
  `max_output_tokens` and a note saying so. Emitted under this name in
  **both** dialects *(rev)*.
- `created` — stable per process (the gateway's start time) for rows that
  have no timestamp of their own; the catalog's `created` for cloud entries
  that publish one *(rev — today it is `now()` on every call)*.
- `capabilities.task` — `chat` | `embedding` | `rerank` | `tts` | `asr` |
  the audio.cpp task name for the other audio tasks (`gen`, `clon`, `vc`,
  `svc`, `s2s`, `sep`, `vad`, `diar`, `align`, `vdes`, `spk`) |
  `image_generation` | `image_edit` | `video_generation`
  *(rev 2026-09-21: the stable-diffusion.cpp class)*. Drives `endpoints`.
  The three image values are picked by the row's own columns, not by a probe,
  because exposure cannot wait for a container: `edit` ⇒ `image_edit`, a
  `modes` list that is `vid_gen` and nothing else ⇒ `video_generation`,
  otherwise `image_generation`. Cloud: `image_generation` when the catalog's
  `output_modalities` contain `image` and **no** `text` — "contains image"
  would take the chat routes away from every chat model that can also draw.
- `capabilities.endpoints` — the lmgw routes that accept this model id.
  `/v1/completions` only on OpenAI-protocol routes (`handle_legacy_completions`
  refuses the others); `/v1/responses`, `/v1/chat/completions`,
  `/v1/messages` on every chat route; `/v1/images/generations` on every image
  model and `/v1/images/edits` on the ones that take reference images
  *(rev 2026-09-21)* — the second is a gate rather than a hint, because
  sd-server segfaults on a reference-image request to a pipeline that cannot
  take one, so lmgw refuses the route for a row that does not list it. Both are
  OpenAI-dialect only: the Anthropic API has no image endpoint, so they appear
  under `openai` in the list-level `lmgw.endpoints` and in no Anthropic list.
- `capabilities.input_modalities` / `output_modalities` — subsets of
  `text`, `image`, `audio`, `video`, `file`, `embedding`. Omitted when
  unknown (a cloud catalog that says nothing).
- `capabilities.vision` — `true`/`false` = `input_modalities` contains
  `image`; omitted when `input_modalities` is unknown. Redundant with the
  list on purpose: the consuming spec (revised 2026-09-17) reads this boolean
  to decide whether a session gets a screenshot tool.
- `capabilities.reasoning` — per the consuming spec, extended:
  - `kind`: `fixed` (nothing to change per request), `toggle` (on/off per
    request), `levels` (effort selectable per request).
  - `enabled`: the default state as this alias is configured. **Omitted when
    no source states it** *(rev)* — a catalog that says "supports reasoning"
    does not say whether it is on by default.
  - `levels`: the values the model accepts, least → most in the canonical
    order `minimal < low < medium < high < xhigh < max`. Only for `levels`.
    Aliases the template folds together (Qwen3.8's `high` → `xhigh`) are
    listed as accepted values; they are.
  - `default`: the effort in force when the request sets none (the row's
    `--reasoning-effort` / `chat_template_kwargs.reasoning_effort`, else the
    template's `|default('…')`). Omitted when unknown (every cloud catalog).
  - `can_disable`: whether the request can switch thinking off entirely.
    Present only when a source states it *(rev)*: local `levels`/`toggle`
    ⇒ `enable_thinking_var` (llama-server maps effort `none` and
    `x-lmgw-reasoning: off` to `enable_thinking = false`, which only a
    template reading that variable honours); OpenAI-protocol catalog ⇒
    `opencode.variants ∋ none`; Gemini, Anthropic ⇒ absent.
  - `budget_tokens`: the configured default budget (`--reasoning-budget`)
    when set and non-zero *(rev)*; a zero budget is reported as
    `enabled: false` (§3.2), not as a budget.
  - `preserve_history` *(rev)*: whether replayed reasoning of earlier turns
    reaches the model again — `params.reasoning_preserve` when set, else
    `chat_template_kwargs.preserve_thinking` when that is a bool *(rev 3: it
    reaches the same template variable, so dropping it published the template's
    default over the owner's setting)*, else true when the template reads
    `preserve_thinking`, else absent.
  - `control`: the exact header and body fields that work on this route —
    only the ones that reach *this* model *(rev 3)*. For a local row that is
    per group: `x-lmgw-reasoning` + `chat_template_kwargs.enable_thinking`
    only when the template reads `enable_thinking`, and
    `x-lmgw-reasoning-effort` + `reasoning_effort` only when it reads
    `reasoning_effort` / `resolved_reasoning_effort` (the names llama-server
    fills from a request's `reasoning_effort`). A template whose only effort
    variable is its own `reasoning_strength` gets neither, plus a note saying
    lmgw does not set that variable — and, having nothing a request can
    change, it is `kind: fixed`, not a `toggle` whose switch is wired to
    nothing.
  - Absent as a whole ⇒ unknown (a cloud model with no catalog signal).
- `capabilities.tool_calls` — `kind`: `native` (lmgw returns structured
  `tool_calls`; every local row whose template renders `tools` in a syntax the
  marker table below names, every cloud model whose catalog says `tools`),
  `none` (template renders no tools;
  aux/audio rows), `text` when the template renders tools in a syntax that
  matches none of the known markers (`format: unknown`; llama-server may
  return such calls as prose) *(rev 3)* — or through an owner override. `--jinja` is on by default in the shipped build and lmgw
  never emits `--no-jinja`, so `params.jinja` does not enter this *(rev)*.
  `parallel`: true when the template loops over `tool_calls` (cloud: absent).
  `format`: the native syntax family the template renders, informational:
  `hermes-json` (`<tool_call>{json}</tool_call>`), `qwen-xml`
  (`<tool_call><function=…><parameter=…>`), `gemma` (`<|tool_call>call:name{…}`),
  `mistral` (`[TOOL_CALLS]`), `llama3` (`<|python_tag|>`), `lfm2`
  (`<|tool_call_start|>`), `gpt-oss` (`<|channel|>commentary to=`),
  `deepseek` (`<｜tool▁calls▁begin｜>`), `granite` (`<|tool_call|>`),
  `provider` for cloud, `unknown` when the template renders tools with none
  of these markers.
- `capabilities.structured_output` — `json_schema` / `json_object` booleans.
  Local llama-server: both true (grammar-backed `response_format`). Cloud
  OpenAI-protocol: from `supported_parameters` when the catalog lists them,
  else absent. Anthropic / Gemini upstreams: absent — lmgw drops
  `response_format` on those egress paths today, unchanged here.
- `capabilities.source` — `gguf+config`, `catalog`, `config` (aux, audio and
  image rows, where there is no GGUF signal to read — an image row's
  `files`/`args`/`modes`/`edit` are the whole story *(rev 2026-09-21)*),
  `owner` (a per-model override, §7).
- `notes` — plain-language, per model, generated from the same facts. Every
  sentence must be true for *this* alias; no generic boilerplate (that lives
  in `lmgw.notes`). The reasoning-trace sentence branches on
  `reasoning_format` *(rev)*: `none` ⇒ "thoughts stay inside
  `message.content`"; `deepseek-legacy` ⇒ "`<think>` tags stay in content and
  the text is also copied to `reasoning_content`"; `auto`/`deepseek`/unset ⇒
  the `reasoning_content` sentence.

### 2.2 Anthropic shape

Same objects with the Anthropic top level (`type: "model"`, `display_name`,
`created_at`) plus the Anthropic SDK's typed names as **aliases**
*(rev)*: `max_input_tokens` (= `context_length`) and `max_tokens`
(= `max_output_tokens`); `max_output_tokens`, `context_window`,
`capabilities`, `notes`, `pricing` and the list-level `lmgw` object are
identical to the OpenAI shape — one capability schema, not the Anthropic
`{feature: {supported}}` tree, because the point is that an agent reads one
contract whichever SDK it holds. Extra keys are safe: both SDKs' model types
accept unknown fields (the existing `pricing`/`context_length` already rely
on that).

`GET /v1/models/{id}` — 404 in the caller's error dialect when the id is not
exposed. The id may contain `/` (`kilo/anthropic/claude-sonnet-5`): axum
`/{*id}` capture.

## 3. Deriving local-model capabilities

### 3.1 Template signals (new, `gguf::TemplateSignals`, pure text heuristics)

Computed from `tokenizer.chat_template`, or from the row's
`chat_template_file` when set — that file wins, as it does for llama-server.

| Signal | Rule |
|---|---|
| `thinking_markers` | template contains any of `<think>`, `<\|channel>thought`, `<\|thinking\|>`, `[THINK]`, `<reasoning>`, `<\|inner_monologue_start\|>`, or renders `reasoning_content` |
| `enable_thinking_var` | references `enable_thinking` |
| `enable_thinking_default` | `enable_thinking \| default(false)` / `default(true)` → that; an `enable_thinking is undefined or … is true`-style guard → true; else unknown |
| `reasoning_effort_var` | references `reasoning_effort` or `reasoning_strength` |
| `effort_levels` | every quoted literal (single or double quotes) compared against `reasoning_effort` / `resolved_reasoning_effort` (`==`, `!=`, `in (…)`, `not in (…)`, `default('…')`), filtered to the canonical vocabulary, sorted canonically. `none` is never a level. |
| `effort_default` | the literal in `reasoning_effort \| default('…')` |
| `preserve_thinking_var` | references `preserve_thinking` |
| `tools_var` | references `tools` as a variable (`if tools`, `for tool in tools`, `tools is`, `tools \|`) |
| `parallel_tool_calls` | loops over `tool_calls` (`for … in ….tool_calls`) |
| `tool_call_format` | first marker match from the table in §2.1 |

Live-verified on 2026-09-17 through lmgw: gemma4-e4b (`--reasoning off`) thinks
when the request carries `chat_template_kwargs: {"enable_thinking": true}`;
qwen3.5-4b (`--reasoning on`) stops thinking with `{"enable_thinking": false}`.
So a configured `off`/`on` is a default, not a cap — `toggle`/`levels` is the
right kind for both rows of a `-reason` pair.

Verified against the 25 distinct templates on this box on 2026-09-17 (ten of
them are checked in under `crates/lmgw-core/tests/fixtures/chat_templates/`):
Qwen3.8 yields `levels = [low, medium, high, xhigh]`, `default = xhigh`,
toggle var, `<think>`, `qwen-xml`, parallel; Qwen3.5/3.6 the same minus
effort; gemma4 `enable_thinking` default false, `<|channel>thought`, `gemma`;
DeepSeek V4 Flash toggle + `<think>`; Cohere North `reasoning_effort` only as
`== "none"` → `levels = []` ⇒ treated as `toggle`; gemma3 / medgemma /
qwen2vl nothing ⇒ `fixed`, `enabled: false`, `tool_calls.kind: none` unless
`tools_var`.

These are heuristics over Jinja text. Runtime truth stays available: when the
model's container is up, llama-server `/props` reports `chat_template_caps`
(`supports_tools`, `supports_parallel_tool_calls`,
`supports_reasoning_effort`, `supports_preserve_reasoning`) and
`modalities {vision, audio, video}`. `/v1/models` does not read it (the
answer must not depend on a container being up), but `lmgw__local_model_test`
reports both and names any disagreement *(rev — pulled forward from
follow-ups; the dashboard model page is still a follow-up)*.

### 3.2 Reasoning kind for a local row *(rev: folds in budget and kwargs; rev 3: only a control a request can reach counts)*

```
markers      = thinking_markers || enable_thinking_var || reasoning_effort_var
kwargs_on    = params.chat_template_kwargs["enable_thinking"] as bool   // Option
configured_on =
    if params.reasoning_budget == Some(0)        → Some(false)   // budget 0 = no thinking
    else match params.reasoning { Some("on") => Some(true), Some("off") => Some(false), _ => None }
        .or(kwargs_on)
effective_on = configured_on.or(enable_thinking_default).unwrap_or(markers)
default_effort = params.reasoning_effort            // fold_reasoning_effort() already
                 .or(effort_default)                 // merged kwargs.reasoning_effort in

// rev 3: llama-server fills `reasoning_effort` / `resolved_reasoning_effort`
// from a request and nothing else, so a template reading only its own
// `reasoning_strength` has no request-reachable effort at all.
settable_effort = effort_var_names ∩ {reasoning_effort, resolved_reasoning_effort} ≠ ∅

kind =
    if !markers                                   → fixed, enabled=false
    else if settable_effort && !effort_levels.is_empty()
                                                  → levels, enabled=effective_on,
                                                    default=default_effort,
                                                    can_disable=enable_thinking_var
    else if enable_thinking_var || settable_effort
                                                  → toggle, enabled=effective_on,
                                                    can_disable=enable_thinking_var
    else                                          → fixed, enabled=(reasoning_budget != 0)
                                                    (nothing a request can turn; the note
                                                    says budget 0 is the only brake)
budget_tokens    = params.reasoning_budget.filter(|b| *b > 0)
preserve_history = params.reasoning_preserve
                   .or(params.chat_template_kwargs["preserve_thinking"] as bool)
                   .or(preserve_thinking_var.then_some(true))
```

`params.reasoning == "off"` on a `levels` template is still `levels` with
`enabled: false` — the request can turn it on (`x-lmgw-reasoning: on`), which
is exactly what the consuming spec's `kind` is meant to convey.

### 3.3 Modalities for a local row *(rev: explicitly configured projector only)*

- `text` always in, `text` always out.
- llama-server started with `-m <path>` does **not** scan the directory for a
  projector (`--mmproj-auto` is the `-hf` manifest path). The projector in
  use is therefore exactly the configured one: `params.mmproj_path`, else a
  `--mmproj <path>` in the row's freeform `args` (argv.rs leaves the flag to
  `args` when the field is unset). No projector ⇒ `["text"]`, plus — when a
  sibling `*mmproj*.gguf` sits next to the weights — a note that a projector
  exists but is not configured.
- Read the projector's header (cached, §3.5): `clip.has_vision_encoder` →
  `image`, `clip.has_audio_encoder` → `audio`. `ModelSummary` gains
  `has_vision_encoder` / `has_audio_encoder` (`Option<bool>` — absent key ≠
  false; a projector with neither key but a `clip.vision.*` block counts as
  vision, as llama.cpp's loader does). An unreadable projector ⇒ modalities
  omitted + note.

### 3.4 Tools, structured output

`tool_calls.kind = native` iff `tools_var`; `parallel` from the template.
`structured_output = {json_schema: true, json_object: true}` for every local
chat row (llama-server grammar).

### 3.5 GGUF summary cache

`/v1/models` must stay cheap: 23 local rows × a header read that walks the
tokenizer arrays is tens of MB of I/O per call. `AppState` gets
`gguf_cache: GgufSummaryCache` keyed by absolute path, validated by
`(len, mtime)` on every hit; `summarize_cached(path)` runs the read on
`spawn_blocking` (one task per row; the blocking pool absorbs a cold start).
Errors are **not** cached and yield absent capabilities plus a note naming
the file — never a 500. Projectors go through the same cache.
`modelinfo::summarize` switches to it; `vram::plan` may (out of scope).

### 3.6 Aliases onto local rows *(rev)*

`Snapshot::exposed_models` lets an alias shadow a local row of the same name
and tags it `alias`. When an alias resolves to an `UpstreamKind::LlamaServer`
upstream, its capabilities are derived from the backing local row (looked up
by `upstream_model_id`) exactly as in §3.2–3.4, with `param_overrides`
(including `max_tokens` and, after §5, `reasoning`) applied over the row's
defaults. `AudioCpp`-kind aliases derive from the audio row.

## 4. Cloud catalogs (`catalog::ModelInfo` grows; the Anthropic arm is split off the OpenAI one) *(rev)*

| Field | OpenAI-protocol catalog (fields exist in the Kilo/OpenRouter shape; a stock `api.openai.com` publishes none of them) | Gemini `models.list` | Anthropic `models.list` |
|---|---|---|---|
| `max_output_tokens` | `top_provider.max_completion_tokens` | `outputTokenLimit` | `max_tokens` |
| `context_length` | as today | `inputTokenLimit` | `max_input_tokens` (new; `context_window` kept) |
| `input_modalities` | `architecture.input_modalities` | absent | `[text, image]` when `capabilities.image_input.supported`; `[text]` when the key is present and false; absent otherwise |
| `output_modalities` | `architecture.output_modalities` | absent | `[text]` when `capabilities` is present |
| `task` | `embedding` when `output_modalities == [embedding]`, else `chat` | `embedContent` in `supportedGenerationMethods` ⇒ `embedding`; `generateContent` ⇒ `chat`; neither ⇒ `chat` with a note that lmgw has no route to it (`bidiGenerateContent` is a live-session API) | `chat` |
| `reasoning` | `supported_parameters ∋ reasoning \| reasoning_effort` ⇒ supported. `opencode.variants` keys (minus `none`) ⇒ kind `levels`, `can_disable = variants ∋ none`; supported without variants ⇒ `toggle`. No `enabled`, no `default`. Not supported ⇒ absent (not `fixed/false` — the catalog does not say the model cannot reason, only that no parameter controls it) | `thinking: true` ⇒ `toggle`, no `enabled`, no `can_disable` (Gemini 2.5 Pro refuses budget 0; the catalog does not say which do); `thinking` absent/false ⇒ `fixed`, `enabled: false` | `capabilities.thinking.supported` ⇒ `levels` from `capabilities.effort.<level>.supported`; no `enabled`, no `can_disable` (the tree has no `types.disabled`); `thinking.supported == false` ⇒ `fixed`, `enabled: false` |
| `tool_calls` | `supported_parameters ∋ tools` ⇒ `native`, `format: provider` | `native` for `generateContent` models | `native` |
| `structured_output` | `response_format` / `structured_outputs` ∈ `supported_parameters` | absent | absent |
| `source` | `catalog` | `catalog` | `catalog` |

A field the catalog does not publish is **absent**, never defaulted. Gemini's
catalog says nothing about modalities; the model-name hints (`-tts`,
`-image`, `native-audio`) are not promoted to facts — the note says "the
provider catalog does not state input modalities".

The 5-minute catalog cache already exists; the new fields ride in it.

## 5. Reasoning control plane

### 5.1 IR

```rust
pub struct ReasoningControl {
    pub enabled: Option<bool>,     // None = leave the route's default
    pub effort: Option<String>,    // verbatim level
    pub budget_tokens: Option<i64>,
}
// Params.reasoning: Option<ReasoningControl>
```

`Params::with_defaults` merges `reasoning` **field-wise** *(rev)*: a client
that sets only `enabled` keeps the alias's `effort` default.

Normalisation to a total triple, applied once after all sources are merged
*(rev)*: `effort == "none"` ⇒ `enabled = Some(false), effort = None`;
`budget_tokens == Some(0)` ⇒ `enabled = Some(false), budget_tokens = None`;
`enabled == Some(false)` clears `effort` and `budget_tokens`. Every egress
maps the normalised triple, so no cell can contradict another.

### 5.2 Sources, precedence (highest first)

1. Headers: `x-lmgw-reasoning: on|off`, `x-lmgw-reasoning-effort: <level>`,
   `x-lmgw-reasoning-budget: <int>`. Parsed in `server.rs` into
   `RequestCtx.reasoning`, which the handlers merge field-wise over the parsed
   body. A malformed value (not on/off, not an integer) or a contradiction
   within the header tier (`x-lmgw-reasoning: on` with effort `none` or
   budget `0`) is a 400 in the caller's dialect naming the header *(rev)*.
2. Body, per dialect:
   - OpenAI chat: `reasoning_effort` (string); `reasoning` object
     (`{effort, max_tokens, enabled, exclude}` — OpenRouter shape);
     `reasoning_budget_tokens` / `thinking_budget_tokens` (llama-server
     shape). `reasoning_effort`, `reasoning_budget_tokens`,
     `thinking_budget_tokens` become modeled keys. The `reasoning` object is
     read for control but **stays in passthrough** *(rev)* so `exclude` /
     `max_tokens` still reach an OpenRouter-style upstream; egress rewrites
     its `effort`/`enabled` keys to the resolved control (§5.3) so the two
     cannot disagree. `chat_template_kwargs` stays passthrough; its
     `enable_thinking` key is read as a source at the same tier as
     `reasoning_effort`.
   - Anthropic messages: `thinking {type: enabled|adaptive|disabled,
     budget_tokens}` and `output_config.effort`.
   - Responses: `reasoning {effort}`; on the native passthrough for
     `supports_responses` upstreams *(rev)* the header controls are injected
     into the forwarded body as `reasoning.effort` (the only control that API
     has), the rest reported ignored.
3. Alias `param_defaults.reasoning`.
4. Nothing ⇒ `None`, egress emits nothing, the model's own configuration
   applies.

### 5.3 Egress rendering of the normalised triple

| Route | `enabled=false` | `enabled=true` alone | `effort=L` | `budget=N` |
|---|---|---|---|---|
| llama-server (`UpstreamKind::LlamaServer`, local rows) | `chat_template_kwargs.enable_thinking: false` and no `reasoning_effort` key at all *(rev 2: a live probe of the shipped build showed `reasoning_effort: "none"` does not switch thinking off there — that special case is newer than the build — and Qwen3.8's template raises on an unknown level)* | `chat_template_kwargs.enable_thinking: true` | `reasoning_effort: L` + `chat_template_kwargs.enable_thinking: true` (so a `--reasoning off` row honours it) | `reasoning_budget_tokens: N` — forwarded; whether the running llama.cpp build honours a per-request budget is the build's business (the shipped build did not in the probe), so `control` for local rows does not list the budget header |
| generic OpenAI-protocol upstream | `reasoning_effort: "none"` | ignored ⇒ `enabled` | `reasoning_effort: L` | ignored ⇒ `budget` |
| Anthropic upstream | `thinking: {type: "disabled"}` | `thinking: {type: "adaptive"}` | `thinking: {type: "adaptive"}`, `output_config: {effort: L}` | `thinking: {type: "enabled", budget_tokens: N}`; `max_tokens` raised to `N + 1024` when smaller (the API requires it) |
| Gemini upstream | `generationConfig.thinkingConfig.thinkingBudget: 0` | ignored ⇒ `enabled` | `thinkingConfig.thinkingLevel: L` | `thinkingConfig.thinkingBudget: N` |

`chat_template_kwargs` on the llama-server route is an **object-level deep
merge** *(rev)*: the client's object (from passthrough) first, control keys
over it. This is a named step in the OpenAI egress, not the whole-key
`passthrough` rule (which would silently discard the client's object).
Likewise the passthrough `reasoning` object gets its `effort` / `enabled`
keys rewritten to the resolved control.

Values are passed verbatim — no vocabulary check in lmgw. A level the
template does not know is the template's business (Qwen3.8 raises; the
llama-server 400 comes back with its message), a level the provider rejects
is the provider's 400. `budget_tokens` on a current Anthropic model is
likewise the provider's 400 (they removed it) — the model's `levels` say
effort is the way, the notes say so too.

**Ignored controls** *(rev)*: computed in the **handler** by a pure function
`reasoning_ignored(protocol, upstream_kind, &control) -> Vec<&'static str>`
(the table's "ignored" cells) and stamped on the response as
`x-lmgw-reasoning-ignored: enabled,budget` next to `x-lmgw-fallback`. The
`Egress` trait signature does not change.

### 5.4 Anthropic `max_tokens` default *(rev)*

`egress/anthropic.rs` defaults `max_tokens` to 4096 when neither client nor
alias set one — a hidden cap the moment `/v1/models` publishes 64000 for the
same alias. The handler resolves the catalog's `max_tokens` for the route
(cached lookup, same call `/v1/models` makes) into `route.param_defaults`
before egress. When the catalog has none, 4096 stays as the last resort but
is surfaced: `x-lmgw-max-tokens-defaulted: 4096` on the response and a
`warn!` line. The same default applies to the Anthropic-shaped ingress'
required `max_tokens`? No — that field is required by the *client* dialect
and already parsed; only the egress default changes.

### 5.5 Responses API

`reasoning.effort` is finally read (`ingress/responses.rs`) and feeds the
same control; the agent loop in `agent.rs` inherits it through `Params`.

## 6. Audio input for chat

- IR: `ContentPart::Audio { mime: String, data: String /* base64 */ }`.
- OpenAI chat ingress: `{"type": "input_audio", "input_audio": {"data": <b64 or data: URI>, "format": "wav"|"mp3"|"flac"|…}}` — `format` → mime (`audio/<format>`), a `data:` URI keeps its own mime.
- Responses ingress: `input_audio` item, same fields.
- Anthropic messages ingress: no audio block exists in that API ⇒ unchanged
  (`Unsupported` error with the same wording as today).
- Egress: OpenAI/llama-server → `input_audio` part (`data` as raw base64,
  `format` from the mime — llama-server ignores `format` and sniffs; cloud
  OpenAI wants it). Gemini → `inlineData {mimeType, data}`. Anthropic →
  `Unsupported("audio input has no Anthropic block type")`.
- Formats: whatever the model's decoder takes; the note for a local model
  says "wav, mp3, flac (llama.cpp's miniaudio)".

`video` is not added (no local model here has a video projector; the shape is
analogous and can follow).

## 7. Owner overrides

A `capabilities_override` JSON column on `local_models` and `models`
(aliases), patched through `lmgw__local_model_set` / `lmgw__model_set` (and
the dashboard forms), deep-merged **over** the derived object; `source` is
reported as `owner` when the `capabilities` object was touched *(rev 3:
`source` describes that object only — an overridden `max_output_tokens`
is announced by an auto-appended note instead)*. This is the consuming spec's "explicit
per-model field the owner sets by hand" and the only way to fill a hole the
catalog leaves (a stock OpenAI alias's modalities, `tool_calls.kind: text`
with a `format`). Sequenced after the derived path but inside this change
*(rev — it is the escape hatch that keeps "unknown" from meaning "ask the
owner")*.

## 8. Work packages *(rev: controls land before they are advertised)*

1. **gguf** — `TemplateSignals`, `has_vision_encoder` / `has_audio_encoder`,
   `GgufSummaryCache`. Unit tests on the checked-in template fixtures and on
   hand-built GGUF headers.
2. **catalog** — the parsed fields of §4 with the Anthropic arm split off;
   tests on captured catalog fixtures (Kilo entry with `opencode.variants`,
   Gemini entry with `thinking`, Anthropic entry with `capabilities`).
3. **n_predict** — `LlamaParams.n_predict` (`--n-predict`), api-types
   mirror, MCP `local_model_set` field, dashboard field, `MANAGED_FLAGS`.
4. **reasoning control** — §5 entire: IR + field-wise merge, headers →
   `RequestCtx`, three ingress parsers, four egress renderers incl. the
   deep-merge step, `agent.rs` / Responses passthrough, ignored-header
   function, Anthropic `max_tokens` default. Tests: ingress round-trip per
   source, egress adapters per cell of §5.3, e2e header precedence and the
   400s, the Responses `reasoning.effort` regression, the passthrough
   `chat_template_kwargs` merge.
5. **audio content part** — §6 with ingress/egress tests and one e2e through
   a mock upstream.
6. **capabilities module** — `capabilities.rs`: schema structs (serde,
   `skip_serializing_if = Option::is_none`), `for_local_row` (incl. §3.6
   alias path), `for_aux`, `for_audio`, `for_catalog`. Unit tests per class.
7. **notes writer** — its own module and fixtures: one expected-notes test
   per class × reasoning kind × `reasoning_format`, so every sentence is
   checked against the row it describes.
8. **/v1/models** — `ExposedEntry` grows; `list_models` emits §2 in both
   dialects; `GET /v1/models/{id}`; the `lmgw` block; stable `created`.
   `web_pages.rs` table-driven test gains capability assertions per class;
   e2e test for `/v1/models/{id}` in both dialects and its 404s.
9. **MCP** — `lmgw__models` carries `capabilities` + `max_output_tokens`
   (same builder); `lmgw__local_model_test` reports `/props`
   `chat_template_caps` + `modalities` beside the static derivation and names
   disagreements.
10. **owner overrides** (§7).
11. **docs** — README "Model capabilities & reasoning control" section (the
    headers table, the JSON, the audio shape), `core-notes.md` pointers, the
    consuming app's notes file updated with the final names.

1–3 are independent (parallel); 4 and 5 are independent of 1–3 but 5 follows
4 (same files); 6–7 need 1–2; 8 needs 4–7; 9–11 last.

## 9. Out of scope / follow-ups

- Dashboard model page showing static vs `/props` capabilities.
- `video` input part.
- Alias-level reasoning defaults in the alias form / `lmgw__model_set`
  (the IR field exists after WP4; the UI does not expose it yet).
- Gemini effort ↔ `thinkingLevel` vocabulary per model generation; passed
  verbatim, the provider validates.
- Translating `response_format` to Anthropic / Gemini egress.
- `vram::plan` on the GGUF summary cache.
