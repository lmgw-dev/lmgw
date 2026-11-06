# API compatibility: measuring and testing what lmgw speaks

**2026-11-04.** How lmgw's OpenAI and Anthropic API compatibility is measured and kept. Two
levels: an offline level 1 inside the existing gate (built, §2) and a
manual live check against the running gateway (built, §3 to §8).

## 1. Why

A tool with no arguments, called by Claude through the Chat, was stored with arguments `""`.
The next request failed at Anthropic with `messages.5.content.0.tool_use.input: Input should
be an object`. The hand-written wiremock fixtures never caught it for two reasons. Nothing
checked the bodies lmgw *sends* against what the provider accepts, and nothing ran turn two
after a tool call. Both levels below close those two gaps: they validate what lmgw sends and
writes against the providers' own schemas, and they replay history after tool calls.

## 2. Level 1: the providers' schemas check every test's wire (offline, every gate)

### What is validated

- **(a) Every request body an egress builds**, in every test that drives a fake upstream (wiremock) or calls an egress directly: OpenAI Chat Completions and Responses, Anthropic Messages (non-streaming and streaming variants) and count tokens, Gemini generateContent / streamGenerateContent, countTokens and batchEmbedContents, and OpenAI-shaped embeddings. Checked against the provider's request schema, made **strict**: an unknown key is a violation, because all three providers refuse one.
- **(b) Every answer and stream event** lmgw sends on `POST /v1/chat/completions`, `POST /v1/responses`, `POST /v1/embeddings`, `GET /v1/models` (both dialects), `POST /v1/messages` and `POST /v1/messages/count_tokens`, in every test that serves the router through `common::serve`/`serve_on`. Whole JSON answers are read and checked. SSE streams are checked event by event as they pass, nothing held back; for Messages and Responses the `event:` name also has to equal the data's `type`. Error answers are checked against the route's error shape. The answer side is not strict: the providers' own answer schemas allow fields a client does not know.
- **(c) A property test** (`tests/it/api_compat/ir_props.rs`, proptest, 256 cases per run, from a fixed seed so the gate is deterministic: a red run is a real regression and reproduces; `LMGW_PROPTEST_SEED=<number>` explores another seed and `LMGW_PROPTEST_CASES=<n>` runs more cases, and a failure prints its seed) generates IR conversations and pushes each through every egress (OpenAI platform, a compatible row, llama.cpp with tool images allowed, Anthropic, Gemini), streamed and not, then asserts on the check directly. The conversations cover system and text turns, base64 and URL images, tool calls with no, nested, array and several arguments, stored arguments as `""`, `null`, unparseable text and a string holding an object, tool results with errors, JSON, resources and images, in both shapes (one `tool` message per call, or one user message holding all of them), signed and unsigned reasoning, refusals, and turns that continue after the tools.

### Where the hooks sit

- **(a)** `lmgw_core::egress::watch` (doc-hidden): `built(up, passthrough, rb)` hands the built request (URL, body bytes, the client's own passthrough map, `&Map`, empty for Gemini; on Anthropic the one key it sends verbatim) to a watcher installed for the whole process. It is called at every egress builder: the five `build_chat`s, the shared OpenAI-shaped embeddings, Anthropic's count_tokens, and Gemini's countTokens (both kinds) and batchEmbedContents. Outside the tests nothing is installed, and the cost is one atomic load (no clone of the body or the map). The test binary installs the watcher at process start with `#[ctor]` (`tests/it/common/api_compat/mod.rs`), so no test asks for it. There was no shared wiremock builder to hook into (168 files call `MockServer::start` directly); the egress builders are the one place every upstream body passes.
- **(b)** An `axum::middleware::from_fn` layer that `common::serve_on` puts around the router (so `common::serve` gets it too): `tests/it/common/api_compat/answers.rs`.
- **Failing.** The checks run inside the gateway, often on a task of their own, where a panic would only cost that request. So a violation is printed at once (with the test thread's name) and kept, and a `#[dtor]` turns the test process's exit status into a failure. nextest runs each test in its own process, so the failure belongs to that test. `api_compat::a_violation_fails_the_test_process` runs an ignored probe in a child process to prove the mechanism.

### Schema sources and pinning

`crates/lmgw-core/tests/fixtures/api_schemas/`: `openai.json` (184.1 KiB), `anthropic.json` (114.4 KiB), `gemini.json` (20.6 KiB), and `SOURCES.json` (per source: URL, commit or version, fetch date, sha256 of the raw bytes and of the canonical JSON, size). Each file has a `request` side and an `answer` side, each with its `entries` and the reachable `$defs`. All `$ref`s are local, and there is one line per schema so a diff names what changed.

- **OpenAI**: the official OpenAPI 3.1 document, `openai/openai-openapi` at commit `b4cad10d7cff7fc286f2dbf1992569989522c6f0` (fetched 2026-11-04, 4.9 MiB).
- **Anthropic**: Anthropic publishes no standalone OpenAPI document. Its SDKs' `.stats.yml` stopped naming an `openapi_spec_url` after 2026-09-02, and the last one named points at a Stainless bucket that no longer exists. The schemas are therefore **derived from the official Python SDK** `anthropic==2.4.0` (wheel sha256 recorded): pydantic 2.14.2's JSON Schema writer over the request TypedDicts (`MessageCreateParamsNonStreaming`/`Streaming`, `MessageCountTokensParams`) and the response models (`Message`, `RawMessageStreamEvent`, `MessageTokensCount`, `ErrorResponse`, `SyncPage[ModelInfo]`).
- **Gemini**: the Generative Language API discovery document, v1beta, revision 20261103 (fetched 2026-11-04). It has no version to pin, so its revision and digests are recorded. Google serves it with its keys in a different order on each fetch, so changes are detected on the canonical sha256.

### Extraction rules (`scripts/api_schemas_extract.py`, all documented in its header)

- Keep what is reachable from the entry points. Drop annotations (description, title, examples, default, `x-*`, `discriminator`).
- Write OpenAPI 3.0 `nullable` as a null alternative (the OpenAI spec is 3.1 and has none today).
- Write OpenAI's `oneOf` unions as `anyOf`. The spec's alternatives overlap: an input message is both an `EasyInputMessage` and an `InputMessage`.
- Convert Gemini's discovery format to JSON Schema: 64-bit integers as string-or-integer, enums in upper or lower case (Google's JSON reads both, and its REST docs write `"thinkingLevel": "low"`).
- Make the request side strict: `additionalProperties: false` on every object schema that lists properties and says nothing about extra keys, after merging an `allOf` of plain objects into one object.

### Update procedure

`bash scripts/api-schemas-update.sh` re-fetches the pinned sources and re-extracts. A rerun that changes nothing proves the extraction is reproducible (verified twice on 2026-11-04). `--latest` first moves the pins to the newest OpenAI commit and Anthropic SDK release. Anthropic's derivation runs in a throwaway `python:3.12-slim` Podman container: only a copy of the two helpers is mounted (`:Z` on that copy, never on the checkout's `scripts/`). The image is pinned by its index digest (`python@sha256:b18b261e…`, `python:3.12-slim` on 2026-11-04). The SDK wheel is downloaded on the host, checked against the sha256 PyPI publishes for it, and that file is what the container installs; `pip freeze` of the container and the image digest are recorded in `SOURCES.json` (`anthropic.container_image`, `anthropic.pip_freeze`). A rerun on 2026-11-04 reported all three schema files unchanged. The script prints, per file, its size and every schema that was added (+), removed (-) or changed (~). The update is manual and the gate does not run it: it needs the network and Podman. Review the fixture diff, run the suite, and commit.

### Rules the check applies (each one explicit, none an off-switch)

- **The client's own keys.** On OpenAI-shaped egresses, a client's top-level passthrough key and its contents belong to the client (`Built::passthrough`) only when the body carries the key's value as the client sent it. A key lmgw wrote into (the `reasoning` object it reconciles with the resolved control, Responses' own `reasoning`) is lmgw's and is checked. *(2026-11-05)* The Anthropic egress hands over one such key too: a Messages request's `context_management`, which it sends as written (gateway design §6.1) and which the pinned non-beta schema does not have. The `/v1/messages/count_tokens` request does the same for the key wherever a count body carries it (the passed-through client body, or the IR rendering), since lmgw never writes into it.
- **Extensions lmgw sends on purpose** (`tests/it/common/api_compat/extensions.rs`), one row each, with the protocols and the reason. They are removed before validation, and the rest of the body must match OpenAI's schema:
  - top-level `top_k`, `min_p`, `repeat_penalty` (llama.cpp and compatible rows);
  - `chat_template_kwargs`, `reasoning_budget_tokens`, `thinking_budget_tokens` (llama.cpp);
  - `reasoning_content` on assistant messages (llama.cpp and compatible);
  - a `json_object` `response_format` with a schema, and a `json_schema` one without its object (llama.cpp and compatible);
  - `image_url` parts in a tool message (llama.cpp, design §8.1);
  - a top-level `reasoning` object on a chat request (llama.cpp and compatible rows: OpenRouter's, the client's own, reconciled by lmgw).
  - *(2026-11-05)* `compaction` blocks in a message's content (Anthropic): the beta block the pinned non-beta request type has no block for, sent back to Anthropic as Anthropic wrote it (gateway design §6.1). The top-level `compaction` is the client's own key (`Built::passthrough`), as `context_management` is.
- **Answer dialects.** With `anthropic-version`, `/v1/models` answers Anthropic's model list. A refusal outside the Messages routes is read by its shape. A non-2xx `/v1/responses` answer is checked as the error envelope like every other route's (*changed 2026-11-05*, finding 10 below: a failed loop run answered its Response object with the error's status, and was checked as a Response).
- **Relayed answers.** An answer lmgw relays as its upstream wrote it (native `/v1/responses`, marked by the `egress::watch::Relayed` response extension) is not checked: it would check the test's fake upstream.

### The known-violations rule

`tests/it/common/api_compat/known.rs` is the one list of violations lmgw has not fixed yet. Each entry gives the entry point, a JSON-pointer pattern, the keyword, optionally the exact unknown keys it covers (`additionalProperties`), the exact missing properties it covers (`required`: `strict` on a tool, `name`/`schema` on a format) and a predicate on the failing value (llama.cpp's two format spellings, the `regex` type, the two fixture service tiers), and a reason ending in "Not fixed yet" (or saying it is a fixture quirk). A match never fails the test but is printed every time it matches ("api-compat known violation, not fixed yet: …"). A fixed entry leaves the list in the fixing change. The property test also has a short list of refusals an egress makes on purpose (Gemini and image URLs); any other refusal fails it.

### Not covered yet

- The native `/v1/responses` passthrough, in both directions (the client's body forwarded, the upstream's answer relayed).
- Legacy `/v1/completions`, `/v1/audio/*`, images, rerank, System One, llama-server's `/tokenize` and `/apply-template`, `GET /v1/responses/{id}`, and the realtime WebSocket events.
- Request headers (`anthropic-version`, `anthropic-beta`). The Anthropic check reads the non-beta request type even when a beta header is sent.
- Sealed reasoning (`encrypted_content`) in generated conversations, structured output, sampling parameters and passthrough keys in the property test.
- Answers from tests that drive `build_router` with `oneshot` (or `tower::ServiceExt`) instead of serving it, and the few listeners that are not the gateway's router (fake upstreams, MCP stubs, the agent proxy's own stubs): only a router served through `common::serve_on` or `common::serve` is answer-checked. The suites that served the router with a bare `axum::serve` were switched to `serve_on` (21 sites).
- An unused-entry report: nextest runs one process per test, so no process sees whether every `known.rs` entry matched; the entries are kept honest by hand (a fix takes its entry off) and `a_known_entry_hides_only_what_it_names` pins the narrow ones. A cross-run report is not built yet.
- The validator compiles each schema entry lazily under one global `Mutex` (tests only; the lock is held for the compile, not for validation). A test process that checks a body pays roughly 15 to 50 ms of schema compile per entry point it touches (measured: about 15 ms for a Gemini entry, about 45 ms for an Anthropic one over a 15 ms baseline test), which the 8000-test gate absorbs.
- Request bodies of FLAC audio: the OpenAI wire takes `wav` and `mp3` only, so the chat's native-audio decision (`web::chat_attach_gate::native_audio`) now sends FLAC natively only where the answering egress takes it (Gemini); on an OpenAI-wire row it takes the transcript path. An API client that sends `input_audio` with another format still has it forwarded as its (lowercased) subtype for the upstream to judge; a typed refusal in the egress is not built yet.
- Known violations found when the remaining suites were served through `serve_on` (2026-11-04): an Anthropic-shaped `/v1/models` entry's `capabilities` is lmgw's own object, where Anthropic's schema wants its fixed capability blocks; a Responses run that hits the tool-call ceiling answers `incomplete_details.reason: max_tool_calls`, which OpenAI's enum lacks. Both are in `known.rs` as not fixed yet.
- A stream that ends without the blank line after its last event has that event checked at the end of the stream; one the client cuts off early is not.
- Anything a live provider accepts or refuses beyond its published schema: that is level 2.

Level 1 catches shape bugs without a network or a key. It cannot catch what a provider
does with a valid body, which is what the live check is for.

## 3. The live check (built)

`scripts/compat-live.sh` is run by hand against the running lmgw. It drives real multi-turn
scenarios and writes a local report. It never runs in `ci/check.sh`, because it costs real
requests. Only its pure parts are tested there (§8).

- **Target:** lmgw at `--base` (default `http://127.0.0.1:8001`).
- **Key:** `LMGW_API_KEY`, else `~/.config/lmgw/compat-key`, which must be mode 600. The key
  reaches the container as an inherited environment variable (`podman run --env LMGW_API_KEY`
  with no value), so it is never in an argv. The recorder drops `authorization`, `x-api-key`
  and cookie headers, and every stored body is passed through redaction first. It is never
  printed.
- **Container:** a rootless Podman run of `python:3.12-slim`. The package dir is mounted
  read-only and `target/compat/` read-write, both with `--security-opt label=disable` instead
  of `:Z`, so the checkout is never relabelled. A loopback lmgw is reached through
  `--network=pasta:-T,<port>` as `127.0.0.1` inside the container. The venv
  (`target/compat/venv-<requirements hash>-<image hash>`) and the pip cache live under
  `target/compat/`, so a rerun installs nothing.
- **Clients:** every request goes through the official SDKs (`openai` 3.45, `anthropic` 2.4,
  pinned in `scripts/compat_live/requirements.txt`), streaming through their stream helpers
  (`chat.completions.stream`, `messages.stream`, `responses.stream`). The SDKs run on an httpx2
  transport that records the exact bytes, so one request gives two readings: the SDK's own
  parse (any exception it raises is a finding) and the raw wire, which the fixed rules (§5)
  read. The SDK builds its objects without validation (`construct`), so the captured JSON is
  also validated against the same SDK pydantic types (rule prefix `sdk.schema`). A malformed
  body, lmgw's own `/v1/count_tokens` and OpenAI's `/v1/responses/input_tokens` go out as raw
  httpx requests.

## 4. Scenarios and matrix

The matrix is scenario × mode (`plain`, `stream`) × ingress route (`chat` =
`/v1/chat/completions`, `messages` = `/v1/messages`, `responses` = `/v1/responses`) × model
alias. `--scenarios`, `--routes`, `--modes` and `--models` narrow it, and `--list` prints the
catalogue.

| id | what it does | routes | modes |
|---|---|---|---|
| `text` | one-word answer | all | both |
| `system` | system prompt (`system`, `instructions`) obeyed | all | both |
| `multi_turn` | two text turns, the first answer replayed as history | all | both |
| `tool_noargs` | call a no-argument tool, send its result, get the final answer (turn 2) | all | both |
| `tool_nested` | object and array arguments, then result and replay | all | both |
| `tool_parallel` | two calls in one turn, both results, final answer | all | both |
| `tool_error` | a tool result flagged as an error (`is_error`; on OpenAI routes the text says it) | all | both |
| `tool_forced` | `tool_choice` naming one of two tools | all | both |
| `tool_none` | `tool_choice: none` with tools present | all | both |
| `image` | a generated 16×16 red PNG | all | both |
| `json_schema` | structured output (`response_format`, `output_config.format`, `text.format`) | all | both |
| `stop_seq` | stop sequence `"5"` while counting to 10 | chat, messages | both |
| `max_tokens` | a 16-token cap: the length finish reason | all | both |
| `reasoning_tool` | reasoning on, tool call, replay including reasoning content, final answer | all | both |
| `prev_response` | turn 2 through `previous_response_id` | responses | both |
| `count_tokens` | the route's counter (`/v1/count_tokens`, `/v1/messages/count_tokens`, `/v1/responses/input_tokens`) | all | plain |
| `compaction` *(2026-11-05)* | on-demand compaction (beta `compact-2026-09-04`): one turn, a compaction request (the block alone, `stop_reason: compaction`), then the block sent back first and a question only the summary answers | messages | both |
| `error_unknown_model` | an unknown alias: 404 and the dialect's error shape | all, once per route | plain |
| `error_malformed` | a truncated JSON body: 400 and the dialect's error shape | all, once per route | plain |

Replay is what a real client does: the assistant message or content blocks or output items
exactly as received (tool call arguments included, nulls dropped), then the tool results.
`reasoning_tool` runs only for models whose `/v1/models` card lists a reasoning control. It
sends `reasoning_effort: high` (chat), `reasoning.effort: high` with
`include: ["reasoning.encrypted_content"]` (responses), and on messages
`thinking: {type: adaptive}` with `output_config.effort: high` for a "levels" card, or the
budgeted `thinking: {type: enabled}` for a "toggle" card. Prompts are a sentence each. They
are sent at temperature 0 for models without reasoning; models with reasoning get no
temperature, because those APIs refuse one.

`compaction` runs only for models whose card lists the on-demand type `summarize`
(`capabilities.context_editing.compaction`, an Anthropic route). Its beta requests go through the
Anthropic SDK's beta client (`betas=[…]`), and their answers are judged by the SDK's beta types
(`BetaMessage`, `BetaRawMessageStreamEvent`), which know the `compaction` block; the fixed rules
take the block, its `compaction_delta` and the `compaction` stop reason. Threshold compaction is
not a scenario yet: its trigger is at least 50,000 input tokens, about $0.01 a run on
`claude-haiku-5-5` before the compaction itself, against well under a tenth of a cent for the
on-demand scenario (a few hundred tokens in and out over its three requests).

**Preset.** `--preset default` is `kilo/anthropic/claude-haiku-5.5`, `kilo/openai/gpt-6-luna`,
`kilo/google/gemini-3.8-flash` (an OpenAI-compatible aggregator upstream) and
`anthropic/claude-haiku-5-5` (lmgw's native Anthropic egress). `--preset kilo` is the first three.
The preset follows the providers' current generation and is raised when a provider ships a new
one. It uses fixed ids and never a drifting `~…-latest` alias, so runs stay comparable.
`--preset direct` (`anthropic/claude-haiku-5-5`, `openai/gpt-6-luna`,
`aistudio/gemini-3.8-flash`) bills the direct provider keys and is run on purpose only:
`scripts/compat-live.sh --preset direct`. Any alias works through `--models`, local llama.cpp
models included. No preset holds a local model, because the GPU is shared.

## 5. Checks

Every answer goes through the fixed rules of its protocol (`scripts/compat_live/rules_*.py`).
A violation has the form `<protocol>.<rule>: detail`.

- **chat:** `object`, `id`, `created`, `model`; `finish_reason` in OpenAI's enum; role
  `assistant`; content a string or null; every tool call has a non-empty unique `id`, type
  `function`, a name, and `arguments` that parse as a JSON **object**; `finish_reason:
  tool_calls` implies calls; usage ints with `total = prompt + completion`. On a stream:
  data-only SSE, `[DONE]` last, one chunk id, role on the first delta, a tool call's first
  delta carries id, name and type, the concatenated arguments are a JSON object, exactly one
  finish reason and nothing after it, and exactly one usage chunk with `choices: []` (the
  check sends `stream_options.include_usage`).
- **messages:** `type`, `role`, `id`, `model`; blocks typed (`text`, `tool_use` with an
  object `input`, `thinking` with a signature, `redacted_thinking`); `stop_reason` in the
  enum, `stop_sequence` set exactly when the reason is `stop_sequence`, `tool_use` reason
  and blocks agree; unique tool ids; usage ints. On a stream: event name equals data `type`,
  `message_start` first and `message_stop` last, blocks start, delta and stop in index order,
  delta types match their block, a `tool_use` start carries `input: {}`, the `input_json_delta`
  parts make a JSON object (no parts means `{}`), one `message_delta` with the stop reason and
  `usage.output_tokens`, no `error` event.
- **responses:** `object`, `id`, `created_at`, `model`; `status` completed or incomplete
  with a known `incomplete_details.reason`; items typed, a `function_call`'s `arguments` a JSON
  object string, unique `call_id`s, usage with both details objects. On a stream: event name
  equals data `type`, `response.created` first and a terminal event last, `sequence_number`
  counting up by one, every added item done, the added item's arguments plus the deltas equal
  the final arguments, and the terminal response passes the body rules with as many items as
  were streamed.
- **errors:** OpenAI's `{error: {message, type, param, code}}` with all four keys; Anthropic's
  `{type: "error", error: {type, message}}` with Anthropic's type for the status
  (400 `invalid_request_error`, 404 `not_found_error`, and so on); JSON content type.
- **sdk:** the SDK raised nothing, and the captured JSON validates against the SDK's own
  types, one event model per stream event type.
- **scenario rules:** the expected finish reason (normalised: stop, length, tool), a no-argument
  call's arguments are exactly `{}`, nested values are not JSON strings, forced and `none`
  tool choice are honoured, the structured output matches the schema, the text stops before
  the stop sequence, and the answer after the tool results carries text.

Semantic checks are cheap and **soft**: the tool was called, the code word recalled, the
square called red, reasoning present. A miss makes the cell `model`, not `fail`, because it
says nothing about lmgw.

**Statuses:** `pass`; `fail` (any rule broken, or an HTTP error that is not one of the
following); `unsupported` (lmgw's own typed refusal, code `unsupported` or
`response_format_unsupported`, or an Anthropic-wire message starting `unsupported:`, with lmgw's
message); `skipped` (503 `gpu_hold`, 429, a model not on `/v1/models`, a reasoning scenario
for a card without a reasoning control; never retried, and the SDKs run with `max_retries=0`);
`model` (rules held, the model did not do what was asked).

## 6. Report

`target/compat/runs/<UTC timestamp>/`:

- `results.json`: the run's metadata (base, models, routes, modes, scenarios, request count,
  cost note, totals, the run it was compared with), the diff, and one record per cell. The run
  compared with is the newest earlier one that covers this one: its `run` object's models,
  routes, modes and scenarios each include this run's, so a rerun of a few cells is never the
  next full run's baseline. When no earlier run covers it, the run compared with is the newest
  earlier one sharing the most cells (models × routes × modes × scenarios) with it, and the
  console line and `report.html` say it is a partial baseline and how many of this run's cells
  it covers (`previous_partial`); only a run sharing no cell leaves nothing to compare with.
  Its cells outside this run are not listed as gone. A record holds
  the key `scenario|mode|route|model`, the status and its reason, the violations, model notes, the
  `x-lmgw-*` response headers, usage and request count. A cell that did not pass also holds
  every request and response body. A body longer than 60,000 characters is cut in the middle,
  with a marker saying how much was cut.
- `report.html`: one table per route (scenario and mode × model, plus the model-free error
  rows), cells coloured by status, each linking to its details. It also lists the new failures,
  fixed cells and other changes against that run's `results.json`. It is a local file
  and is never uploaded.

The console prints one line per cell, the counts per route × model, the request count, the
cost, the diff summary and the report path. The exit code is 1 when a cell failed, 2 on a
usage error, else 0.

## 7. Cost

lmgw reports no per-request cost on the wire yet. The check estimates it from the usage each
answer reports and the per-token `pricing` on `GET /v1/models`, and names the models that have
no price there (for example the direct `anthropic/` aliases) instead of counting them as free.
A full default matrix is 354 cells and about 500 requests, a few US cents on the preset's
models. `--jobs` (default 6) bounds the cells in flight. Iterate with `--scenarios` and
`--models` subsets.

## 8. How to run

```
scripts/compat-live.sh                                # the default preset, the whole matrix
scripts/compat-live.sh --models kilo/openai/gpt-6-luna --scenarios tool_noargs,tool_parallel
scripts/compat-live.sh --routes messages --modes stream --jobs 2
scripts/compat-live.sh --preset direct                # the direct provider keys
scripts/compat-live.sh --list
```

Offline, `python3 scripts/compat_live_test.py` tests the pure parts (SSE parsing, every rule
family, error classification, stream assembly, the diff, redaction) on fixed samples. It runs
in `ci/check.sh`, and in `--changed` mode when `scripts/compat_live*` changed.

## 9. What the first runs found (2026-11-04)

Two full default-preset runs (354 cells and 506 requests each, about $0.06 for the priced
models) gave the same result: 259 to 260 pass, 74 to 75 fail, 19 to 21 model. These are lmgw
failures, each shown on more than one upstream:

1. **A no-argument tool call streams as empty arguments.** On streamed chat the arguments are
   `""`. On streamed responses they are `"\"\""`, a JSON string. Both Claude legs show it. Replayed
   to the native Anthropic egress, turn 2 fails with `tool_use.input: Input should be an object`.
   *Fixed:* the stored and replayed half in d3fa11d1 (`ir::parse_call_args`,
   `ir::object_call_args`); the stream in 00253026: every stream's decoder is wrapped by
   `egress::no_args`, which sends one `{}` arguments delta ahead of the stop for a call that got
   no argument text, so chat, messages (`input_json_delta`), responses, the Chat and the tool
   loop all read `{}`.
2. **Streamed responses, function calls:** `output_item.added` carries `arguments: "{}"`. The
   deltas are the upstream's raw text (`{"city": "Paris"}`), while `arguments.done` and the item
   carry a re-serialised `{"city":"Paris"}`. A client that adds the deltas to the added item gets
   `{}{"city": "Paris"}`. Seen on every model. *Fixed in 00253026:* the added item says
   `arguments: ""`, and `.done`, the item and the final response carry exactly the deltas
   joined (the upstream's own text); a call that streamed no text (a unary turn) streams its
   arguments as one delta. The same for MCP calls.
3. **Streamed responses, text:** `response.output_text.delta` and `.done` have no `logprobs`,
   which OpenAI's schema requires. The SDK's own types reject every such event. *Fixed in
   726c2268* (`logprobs: []`; lmgw relays no log probabilities yet).
4. **Responses, max tokens:** a reached `max_output_tokens` ends with `status: completed`, where
   OpenAI's API says `incomplete` with `incomplete_details.reason: max_output_tokens`. Seen on
   every model. Chat and messages report the length reason for the same models. On the native
   Anthropic leg the output is also empty (`output: []` with 16 output tokens, none of them
   reasoning). *Fixed in 00253026:* a length stop is `incomplete` with `max_output_tokens`
   (`response.incomplete` on a stream), a content-filter stop `content_filter`. *The empty
   output is the model's, not a drop:* Claude Haiku 5.5 thinks adaptively by default with
   thinking display `omitted`, so the 16 tokens all went into a thinking block whose text is
   empty (the Kilo leg of the same run reports `reasoning_tokens: 16`; Anthropic itself
   publishes no reasoning counter, hence 0 there). There was no text to keep. Not yet: the
   Anthropic egress reads a thinking block's text only, not its `signature` (no seal), so such
   a block leaves no reasoning item in `output`, where OpenAI's would show one with an empty
   summary.
5. **Messages, stop sequences:** the text stops at the sequence, but the answer says
   `stop_reason: end_turn, stop_sequence: null`, on the native Anthropic egress too. *Fixed for
   the native Anthropic egress in 00253026:* `FinishReason::StopSequence` carries Anthropic's
   `stop_sequence`, and `/v1/messages` answers `stop_reason: stop_sequence` with it, unary and
   streamed; Chat Completions says `stop`, as it did. Not yet for the other upstreams: OpenAI,
   Gemini and OpenAI-compatible answers say only `stop`, and llama-server's OpenAI-compatible
   answer names no stop word (its native `/completion` has `stopping_word`, and the chat route
   carries it only in `__verbose`, at server verbosity, which lmgw does not run); those stay
   `end_turn` with `stop_sequence: null` rather than a guessed sequence.
6. **`POST /v1/responses/input_tokens`** is not served. It falls through to a 405 with an empty
   body. *Fixed in 90a71d02:* a thin adapter onto the counter (`proxy/count_responses.rs`),
   answering `{"object": "response.input_tokens", "input_tokens": N}`, its answer checked by
   level 1 (`TokenCountsResource`). Exact on Anthropic (the rendered Messages body on its own
   counter), Gemini and a local llama.cpp chat row, flattened elsewhere
   (`x-lmgw-count-approximate`). Not yet: the OpenAI platform's own counter, and `mcp` tools'
   definitions (`tools_omitted`).
7. **Anthropic-wire error types:** an upstream 400 (Anthropic refusing
   `thinking.type.enabled` for a current model) is relayed as `api_error` instead of
   `invalid_request_error`. *Fixed in 00253026:* an upstream's error answer types by its
   status: 400/422 `invalid_request_error`, 404 `not_found_error`, 413 `request_too_large`, 429
   `rate_limit_error`, 529 `overloaded_error`, other 5xx (a 503 included, which is answered 502) `api_error`. A 401 or 403 is lmgw's
   own upstream key refused, answered 502, and stays `api_error` rather than telling the client
   to fix a key that is not its own. The OpenAI wire's `type` moved the same way
   (`invalid_request_error`, `rate_limit_error`).
8. **Streamed responses, upstream failure:** `response.failed` carries
   `error.code: upstream_error`, which is not in OpenAI's code enum. The SDK's types reject it.
   *Fixed in 00253026:* the code is always a word of OpenAI's `ResponseErrorCode` enum
   (`ingress::responses::failed_code`): the upstream Response's own where it is one,
   `rate_limit_exceeded`, `invalid_prompt` for a refused request (structured-output refusals
   included), else `server_error`; lmgw's text stays the message and its kind the request
   log's. Not yet: `incomplete_details.reason` of a loop that hit its tool-call ceiling
   (`max_tool_calls`), its time limit or a cancel; OpenAI's enum has only `max_output_tokens`
   and `content_filter`, so these stay in `known.rs`.

Found beside them and fixed with them: sampling parameters a current model refuses
(Anthropic "`temperature` is deprecated for this model.", OpenAI "Unsupported parameter: …" /
"Unsupported value: …") are retried once without exactly the named one and left out of that
model's later sends, said in `x-lmgw-params-refused` (model-capabilities design §5.7;
caf14cbc).

A later run (2026-11-04) found that retry missing on one route: **native `/v1/responses`
did not retry a refused parameter.** Direct `openai/gpt-6-luna` answered every Responses request
that carried `temperature` with `400 invalid_request_error: Unsupported parameter:
'temperature' is not supported with this model.`, while chat and messages for the same model
worked, because only the chat sends (`proxy::reasoning_fit::send_chat`) had the retry, and the
native passthrough forwards the client's body itself. *Fixed:* the passthrough uses the same
matcher, notes and per-route memory (`responses/native/refused_params.rs`, on
`proxy/reasoning_fit/params.rs`). It takes exactly the refused top-level key out of the body and
sends once more, whole and streamed alike, since a stream's refusal is an HTTP status that
arrives before any byte is relayed. The refused attempt gets its own row. Later requests leave
the parameter out before their first send, with an INFO line and `x-lmgw-params-refused` naming
what this forward's own sends left out. The retry comes before the hop's hand-on decisions, and
a fallback that answers names nothing the hop left out.

The same fix found the reasoning off unfitted on that route: an off without an effort was not
sent and was reported in `x-lmgw-reasoning-ignored`, an effort `none` went out as asked, and a
refusal of it stood, with no retry and no lesson; an off learned on chat for the same model was
not applied. *Fixed (owner-approved 2026-11-05):* the passthrough fits the off with the chat
sends' rule, retries and per-route memory (`responses/native/reasoning_off.rs`, on
`proxy/reasoning_fit`'s `fit_off` and `OffRetry`), written into `reasoning.effort`:
`"none"`, the model's lowest level, or no `effort` key (model-capabilities design §5.6). A
refused attempt gets its own row, the form that answered is remembered for both routes,
`x-lmgw-reasoning-ignored` says `enabled` when the off went out in another form, and an explicit
level the model refuses stands, as on chat. The off's retries share one loop with the
parameters', before the hand-on decisions. The review of 55e49bfe found one more thing there,
fixed with it: a stream whose client left after a refusal went on to the retry, and its row
charged the prompt's local count as a lower bound, although the only finished attempt was a
free `400`. It now sends no retry, and its row is the `499` with nothing charged, as on the chat
stream.

The review of that fix (30be5de7) found, and its fix round changed: a refusal that depends on the
body taught the whole route — "The following tools cannot be used with reasoning.effort 'minimal':
web_search" made every later off of the model, on chat too, go out at `low`. A lesson is now kept
only when every refusal retried was about the control alone (its `param` the control, its `code`
about one parameter, its words naming nothing else the request carried, and a refusal naming no
control only on a request that carried nothing else); any other is retried for its request, its
note saying nothing is kept and why (`proxy/reasoning_fit/off_retry.rs`, `refusal.rs`; chat's sends
by the same rule, from the request's tools, tool choice, format and own keys). A stream whose client
left after a `4xx` refusal gets one row, the `499`, naming the refusal and saying nothing was
retried; a `5xx` it left during is the plain hang-up (a lower-bound count). A refusal whose body
never arrived earns no retry. The off is fitted after the out-of-quota skip, so a skipped hop reads
no facts from its upstream.

A later run (2026-11-05) found two more, numbered on from the list above:

9. **Streamed native turn replayed with its reasoning item → 404.** Direct `openai/gpt-6-luna`,
   stream mode, stateless multi-turn: turn 2 replays turn 1's output from the SDK's final response
   (`response.completed`), its reasoning item included, and lmgw answered `404 no stored response
   'rs_…'`. OpenAI encrypts an item anew each time it writes it (Fernet, a random IV), so the
   stream's `response.output_item.done` and `response.completed` carry different
   `encrypted_content` for the same id; lmgw recorded the first and checked the replay against it
   (WP8 review S-R3). The whole answer passed. *Fixed:* every encryption an answer carried for an
   item is recorded (migration 0100, `native_response_item_contents`, one row per item and hash),
   and a returned item passes with any of them; encrypted content the answer never carried still
   answers as an unknown id (`store/responses/items.rs`).
10. **Synthesized `/v1/responses` timeout answered `504` with a failed Response object.**
   `kilo/google/gemini-3.8-flash` (the loop, not native), whole answer: the upstream timed out
   after 120 s, and lmgw answered `504` with `{"object": "response", "status": "failed", …}`. The
   OpenAI SDK reads any non-2xx as `APIStatusError` with the error envelope, and the rules found
   `error.type` and `error.param` missing. That shape was the loop's from its start; only a
   structured-output refusal answered the envelope (structured-output design §11.15). *Fixed:* a
   whole run that fails answers its status with the envelope (`message`, `type` by lmgw's
   status→type mapping, `param` null but for a format error, `code`), here `504` with
   `{"error": {"type": "api_error", "code": "timeout", "param": null, …}}`, and an upstream rate
   refusal keeps its headers. Nothing is stored, as before: the client got no id to fetch. A
   stream is unaffected — it has answered `200` with `response.created` before any turn runs, so
   it ends with `response.failed`. Level 1 now checks a non-2xx `/v1/responses` answer as the
   error envelope (`answers.rs`).

The same run's five `max_tokens|stream|responses` failures were the check's own: the openai
SDK's stream helper raises on any terminal event other than `response.completed`, and a cut
answer ends in `response.incomplete`. `scripts/compat_live/transport.py` now asks the SDK for
its final response only after `response.completed`, and leaves any other terminal event to the
rules (`rules_responses.TERMINAL`).

Provider and model behaviour, not lmgw: `kilo/openai/gpt-6-luna` ignores `stop` (the aggregator
drops it for a reasoning model, and lmgw sends it on). The current Claude and GPT models chose
not to reason at effort high on the tiny prompt, so the reasoning round-trip was exercised on
Gemini only (it passes there, signatures included). Claude calls the failing tool again after an
error result. `anthropic/claude-haiku-5-5` refuses budgeted thinking, so a "levels" card gets
adaptive thinking.

## 10. Not yet

- Level 1 (§2) as a whole.
- `/v1/completions`, `/v1/embeddings`, `/v1/models`, `/v1/realtime`, audio and images routes.
- Anthropic prompt caching (`cache_control`), PDFs and documents, citations, server tools
  (web search), `pause_turn` continuation, and the batch and files APIs.
- Responses: built-in tools, `background`, retrieving a stored response
  (`GET /v1/responses/{id}`), the `input_items` listing, `conversation`.
- OpenAI `n > 1`, `logprobs`, `seed`, audio output, `parallel_tool_calls: false`, and
  `tool_choice: required`.
- A check of HTTP headers beyond the content type: Anthropic's `request-id` and the rate-limit
  headers.
- Per-request cost read from lmgw itself rather than estimated.
- Running the same scenario directly against a provider as a reference, to tell an lmgw
  translation from a provider or aggregator behaviour automatically. Today that classification
  is done by hand from the evidence in the report.
