# LLM API Gateway (lmgw) — Design

**Date:** 2026-06-09
**Status:** Draft for review

## 1. Summary

`lmgw` is a self-hosted LLM API gateway for personal/homelab use. It exposes a
stable, OpenAI- **and** Anthropic-compatible API to local clients, routes each
request — by a configurable model *alias* — to an upstream provider
(OpenAI-compatible, Anthropic, or Gemini), translating protocols as needed. It
manages local llama.cpp models through **llama-server router mode** (generating
its model preset and controlling its Podman container) and provides a web UI for configuration and
live observability.

It ships as a **Tauri v2 desktop app** that lives in the system tray. The tray
controls a long-running Axum HTTP server embedded in the same process; the
Tauri window simply displays the server's web UI. The same UI and API are
reachable from any browser/app on the trusted LAN.

### Goals
- One stable client API (OpenAI + Anthropic shapes) decoupled from where a model
  actually runs.
- Configure upstreams and map friendly local aliases → `{upstream, model, params}`.
- Auto-switch local llama.cpp models via llama-server router mode, configured
  and lifecycle-managed from the app.
- Web UI for configuration + live request observability.
- Tray-resident desktop app for quick local control.

### Non-goals (v1)
- Multi-user accounts / RBAC / team usage accounting.
- Cost dashboards, time-series charts, long-term analytics (logs + live metrics only).
- Load balancing / automatic upstream failover / retries across upstreams.
- Encryption of secrets at rest (plaintext + file perms, see §13).
- Headless/server deployment as a shipped artifact (architecture supports it; not built in v1).

## 2. Locked decisions

| Decision | Choice |
|---|---|
| Local model switching | Delegate to **llama-server router mode**; gateway **generates & reloads its model preset** |
| llama-server runtime | Gateway **manages its Podman container** (start/stop/restart/reload) |
| Deployment scope | **Personal homelab** — single user, trusted LAN, minimal auth |
| Backend language | **Rust** (Axum + Tokio) |
| Frontend | **HTMX + Askama** server-rendered, assets embedded; SSE for live data — *superseded 2026-08-29 by a Leptos CSR SPA, see [docs/design/ui-rebuild/plan.md](ui-rebuild/plan.md)* |
| Desktop shell | **Tauri v2** app with **system tray**; window loads the local web UI |
| Client (ingress) API | **OpenAI** (`/v1/chat/completions`, …) **+ Anthropic** (`/v1/messages`) |
| Upstream (egress) protocols | **OpenAI-compatible + Anthropic + Gemini** (protocol translation) |
| Translation architecture | **Canonical IR + adapters** (N+M, not N×M) |
| Persistence | **SQLite** (config + request logs) |
| Secret storage | **Plaintext** in SQLite, protected by `0600` file permissions |
| Observability | **Request logs + live metrics** (no charts in v1) |

## 3. Architecture overview

One Rust binary (the Tauri app). A single Tokio runtime hosts:

- **Axum server task** — the gateway API + web UI + SSE, bound to a configurable
  address (default `127.0.0.1:8787`; may bind `0.0.0.0` for LAN access).
- **Tauri main thread** — system tray icon + menu, and a window whose webview
  loads `http://127.0.0.1:<port>/` (the Axum-served dashboard — HTMX when this
  was written, the Leptos SPA since the 2026-08-29 cutover; same URL either way).

Both share one application state (`Arc<AppState>`): config snapshot, SQLite pool,
telemetry bus, and the Podman/llama-server manager. Tray menu actions call the same
core functions the HTTP handlers do — no internal HTTP hop.

```
                       ┌─────────────────────────── lmgw (Tauri app, one process) ──────────────────────────┐
                       │                                                                                      │
  LAN browser / app ──HTTP──▶  Axum server  ──┐                          Tauri tray + window                 │
  (OpenAI / Anthropic SDK)     /v1/* , /ui ,   │                          - status, Open Dashboard,           │
                               /events (SSE)   │   Arc<AppState>          - Start/Stop llama-server, Quit     │
                                               ├──▶ store (SQLite)  ◀──────┘  (calls core fns directly)        │
   Tauri webview ──loads──▶ http://127.0.0.1:<port>/ (same HTMX UI)                                            │
                                               ├──▶ telemetry bus (broadcast → SSE)                            │
                                               ├──▶ router (alias → upstream)                                  │
                                               ├──▶ adapters (IR ↔ openai/anthropic/gemini)                    │
                                               └──▶ podman mgr ──▶ llama-server router ──▶ model instance procs │
                       └──────────────────────────────────────────────────────────────────────────────────────┘
```

> Route names in the diagram are as-of-2026-06: the dashboard plane is now the
> SPA at `/` plus `/api/*` (JSON + SSE), and `/ui` — the SPA's parallel mount
> during the rebuild — permanently redirects to `/`.


### Crate layout
- **`lmgw-core`** (lib): ingress parsing, IR, router, adapters, proxy engine,
  store, telemetry, llama-server + Podman manager, web (Askama/HTMX) handlers, the
  Axum `Router` factory. No Tauri dependency.
- **`lmgw`** (`src-tauri`, bin): Tauri v2 shell — tray, window, lifecycle,
  single-instance; spawns the `lmgw-core` Axum server on startup and wires tray
  actions to core functions.

This split keeps the gateway testable headless and makes a future headless bin
(`lmgw-headless`) a thin wrapper.

## 4. Request data flow (chat completion)

```
client → POST /v1/chat/completions (OpenAI)  ─┐   or  POST /v1/messages (Anthropic)
   ingress parser (openai|anthropic) → IR     │
   router: alias "my-claude" → {upstream: anthropic, model: "claude-…", overrides}
   egress(anthropic): IR → /v1/messages request (reqwest, streaming)
   upstream SSE (message_start, content_block_delta, …) → IR deltas
   ingress serializer (matches the client's protocol): IR deltas → client SSE
   telemetry: record {alias, upstream, tokens, ttfb, total, status} → broadcast + SQLite
```

**Uniform local/remote routing:** the llama-server router is just an
`openai`-protocol upstream of kind `llama_server`. A "local model" is an alias
pointing at it, so local and remote requests share one proxy path. Local-model
entries *also* feed the router's preset generation (§8).

## 5. Canonical IR (internal representation)

Provider-neutral types, rich enough for both ingress shapes and all three
egress shapes. Built incrementally (text → tools → multimodal).

- `ChatRequest { model_alias, messages, params, tools, tool_choice, stream, stop, … }`
- `Message { role: System|User|Assistant|Tool, content: Vec<ContentPart> }`
- `ContentPart::{ Text(String), Image{ mime, data|url }, ToolUse{ id, name, args }, ToolResult{ id, content } }`
- `Params { temperature, top_p, top_k, max_tokens, presence/frequency_penalty, seed, … }` (optional fields; unknown/unsupported params dropped per-egress with a logged note)
- `StreamDelta::{ TextDelta, ToolCallDelta, Usage, Stop{ reason }, Error }`
- `Usage { prompt_tokens, completion_tokens }`
- `Completion { content, finish_reason, usage }`

Mapping notes: OpenAI `tools`/`tool_calls` ↔ Anthropic `tool_use`/`tool_result`
content blocks ↔ Gemini `functionDeclarations`/`functionCall`/`functionResponse`;
system prompt is a top-level field for Anthropic/Gemini but a message for OpenAI;
finish/stop reasons normalized.

## 6. Ingress (client-facing API)

Two protocol surfaces, both → IR:
- **OpenAI:** `POST /v1/chat/completions`, `POST /v1/completions`,
  `POST /v1/embeddings`, `GET /v1/models` (aggregates enabled aliases).
- **Anthropic:** `POST /v1/messages`, `GET /v1/models` (Anthropic-shaped list).

Each ingress has a parser (request → IR) and a serializer (IR / IR-deltas →
that protocol's response + SSE framing). Streaming and non-streaming supported.
Embeddings: OpenAI-shape in/out; routed to OpenAI/Gemini embedding endpoints
(Anthropic has none — alias to an embeddings-capable upstream).

### 6.1 Anthropic context management on `/v1/messages` (added 2026-11-05)

A Messages request's `context_management` (`{edits: [...]}`, Anthropic's
context editing) is Anthropic's own field with no equivalent anywhere else.
The ingress keeps it whole on the IR (`ChatRequest::context_management`):

- **Anthropic upstream:** the egress sends it on as written, and Anthropic
  answers for it — a 400 for an edit type the model lacks reaches the client
  like any other. Anthropic serves it as a beta: the client sends the beta
  flag (`context-management-2025-06-27`) in `anthropic-beta`, which reaches
  the upstream as every beta flag does; lmgw adds none. The answer's own
  `context_management` (`applied_edits`) is relayed on the Messages wire: in
  the message object, and on a stream's closing `message_delta`, where
  Anthropic sends it. A compaction edit (`compact_20260112`) goes out as
  written too *(changed 2026-11-05: refused with a 400 until then)*; its
  `compaction` block comes back as below.
- **Every other upstream** (OpenAI, OpenAI-compatible, Gemini, llama.cpp):
  not sent, and named in `x-lmgw-params-ignored` (`context_management`).
- `/v1/chat/completions` and `/v1/responses` do not carry the field to an
  Anthropic upstream yet; only the Messages ingress reads it.
- `/v1/messages/count_tokens` on an Anthropic route passes the client body
  through, `context_management` included, and relays the answer's own
  `context_management` (`original_input_tokens`, the input before the
  edits) beside `input_tokens`. The IR-rendered count body (another
  dialect's count on an Anthropic route) keeps the field too.

**Compaction** *(changed 2026-11-05: both kinds were refused with a 400
until then, because the IR had no part for the block)*. Anthropic's
protocol, as its docs state it on 2026-11-05
(<https://platform.claude.com/docs/en/build-with-claude/compaction>,
[…/compaction-on-demand](https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand),
[…/compaction-threshold](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold),
the beta API reference
[…/api/beta/messages/create](https://platform.claude.com/docs/en/api/beta/messages/create)
and […/api/beta/models/list](https://platform.claude.com/docs/en/api/beta/models/list)):

- **Two ways to ask.** *On demand*: the top-level `compaction:
  {type: "summarize", instructions?}` (beta `compact-2026-09-04`;
  `instructions` up to 16,384 characters, replacing the default prompt).
  The request is a compaction request: it summarizes every message, answers
  the block alone, no reply, `stop_reason: "compaction"`. It cannot be
  combined with `context_management`. *At a threshold*: a `compact_20260112`
  edit in `context_management.edits` (beta `compact-2026-01-12`) with
  `trigger: {type: "input_tokens", value}` (default 150,000, at least
  50,000), `pause_after_compaction` (default `false`) and `instructions`.
  The answer starts with the block and goes on with the reply; paused, it
  ends after the block with `stop_reason: "compaction"`.
- **The block**: `{type: "compaction", content, …}`. `content` is the
  summary text, or `null` when none was produced (Anthropic then treats the
  block as a no-op); the beta API reference's request-side block says an
  empty string is not allowed. Beside it, fields the client sends back
  verbatim, as the beta API reference lists them on the block:
  `signature`, `encrypted_content` ("opaque metadata from prior compaction,
  to be round-tripped verbatim"), `tool_changes`, and a `cache_control` the
  client may put on it. The threshold page itself shows only `content`
  (and `cache_control`). A changed `signature` or
  `content` is a 400 (`compaction_signature_invalid`,
  `compaction_content_mismatch`), and so is a field the API did not return
  (`citations: null`).
- **What Anthropic does with the history before the block.** On demand,
  the client replaces the summarized messages with the block, sent first
  (its own assistant message or the first block of the first message);
  messages left in front of it are a 400 (`compaction_block_misplaced`);
  exactly one block per request. At a threshold, the client appends the
  answer as usual and "the API automatically drops all content blocks prior
  to the `compaction` block"; with several blocks, the last one is the
  state. The top-level `system` is not part of `messages` and stays.
- **Streaming.** An on-demand block arrives whole in its
  `content_block_start`, then `content_block_stop`, no delta. A threshold
  block's `content_block_start` is followed by "a single
  `content_block_delta` with the complete summary content", then
  `content_block_stop` (threshold page; its SDK samples name the delta
  `compaction_delta` and read its `content`). The beta API reference's
  `BetaCompactionContentBlockDelta` lists `content` and
  `encrypted_content`. lmgw relays whatever fields a delta carries,
  verbatim, either way.
- **Usage.** `usage.iterations` lists every sampling iteration, a
  `compaction` entry for the compaction beside the `message` ones; the
  top-level counts leave the compaction entries out (zero on an on-demand
  answer), and a request is billed as the sum over every iteration. With a
  compaction beta, every answer carries `iterations`.
- **Counting.** `/v1/messages/count_tokens` ignores the `compaction`
  parameter, and counts a request with existing blocks applied (no new
  compaction).
- **Models.** With the beta flag, Anthropic's `/v1/models` entry has a
  `capabilities.compaction` block (`supported`, `summarize`) beside
  `context_management.compact_20260112`.

How lmgw serves it:

- **IR.** A `compaction` block is `ContentPart::Compaction`
  (`ir::compaction`): the summary as text and every other field verbatim,
  so it goes back to Anthropic as it came (serde_json's sorted keys aside).
  Streamed, `StreamDelta::Compaction` carries the block's start and each
  delta at it as they came; `StreamDelta::UsageIterations` and
  `Completion::usage_iterations` carry `usage.iterations`. The top-level
  `compaction` field is `ChatRequest::compaction`, verbatim.
- **Anthropic upstream.** The edit and the top-level `compaction` go out as
  written, the client's beta flag in `anthropic-beta`; a block in the
  request's messages goes back as the client sent it. *(Changed
  2026-11-06)* A block lmgw read from an answer, whole or streamed, carries
  the scope of the row that answered (`Compaction::scope`), as a signature
  does ("Where a signature goes", below): its `signature` and
  `encrypted_content` are that provider's. lmgw's own records send it to an
  Anthropic-protocol row at that scope as it came; a row at another scope
  gets the conversation folded as every other upstream gets it (below,
  `ir::compaction::folded_for`, which folds only those blocks and keeps the
  rest), and the request names `content.compaction` and counts it in the
  send's INFO line. Before, a recorded block went to every Anthropic row. The answer's block,
  whole and streamed, reaches a Messages client as Anthropic wrote it: the
  same events, the same JSON, `stop_reason: "compaction"` as sent, and
  `usage.iterations` verbatim beside Anthropic's own top-level counts. The
  IR's usage (what the logs and the prices read) adds the compaction
  entries in, since Anthropic bills them; the Messages wire takes them out
  of its top-level counts again. A Chat Completions or Responses client,
  which cannot ask for compaction, reads a block's summary as text, and its
  usage counts the compaction in.
- **Every other upstream** (OpenAI, OpenAI-compatible, Gemini, llama.cpp:
  a fallback, an alias, a route of the client's choice): the conversation
  is translated, never refused (`ir::compaction::folded`): the last block
  with a summary is in effect, and every content block before it is
  dropped. Its summary opens what is left as a **user** text part, with no
  framing text: its own user message, or the first part of the next
  message when that is a user message too. Whatever followed the block
  inside its own message stays a message of that message's role after it.
  Both kinds of block come back in an assistant message, and folded that
  way neither opens the conversation with the model's turn nor puts two
  assistant turns in a row, which llama.cpp chat templates (Gemma's,
  Mistral's) and Gemini's `contents` would not take *(changed 2026-11-05:
  the summary kept its message's role at first, so a folded request opened
  with the model's turn)*. A summary of `""` (which Anthropic's request-side
  block does not allow) is in effect all the same, the history before it
  dropped, but adds no empty text part. A block without a summary
  is left out and drops nothing. System messages stay. The on-demand rule
  that a block must come first is Anthropic's check of its own protocol,
  not a meaning: off Anthropic the earlier content is dropped either way.
  What each egress gets:

  | Client sends (after `system`) | OpenAI-wire, llama.cpp, Responses, Gemini get |
  |---|---|
  | `assistant[block]`, `user[q]` (on demand) | `user[summary, q]` |
  | `…`, `assistant[block, a]`, `user[q]` (threshold) | `user[summary]`, `assistant[a]`, `user[q]` |
  | `user[x, block, y]`, … | `user[summary, y]`, … |
  | a block with `content: ""` | the history before it dropped, no summary part |
  | a block with `content: null` | the block left out, nothing dropped |

  None of this was checked against a live provider yet.
- **Asking for a compaction off Anthropic.** The request fields that ask
  for one (`compaction`, the edit inside `context_management`) are left
  out on every other upstream and named in `x-lmgw-params-ignored`; an
  on-demand `compaction` request there is answered as a normal reply. It
  is not yet translated into a summarization request of the upstream's
  own.
- **Counting.** `/v1/messages/count_tokens` on an Anthropic route passes
  the client's body through, blocks included. On every other route the
  count is of the folded conversation: the summary, and nothing before the
  block. lmgw's own estimates count the folded conversation on every
  route, Anthropic's included, since Anthropic drops the content before
  the block too: a stopped call's prompt estimate, the platform-tokenizer
  floor, and the media parts the gate bounds and a candidate's
  capabilities are checked against.
- **Usage on the Messages wire.** The top-level counts are Anthropic's own,
  field set included: a cache counter Anthropic's top level left out stays
  out, though a compaction entry carried it and the IR's usage counts it
  in.
- **Capabilities.** lmgw's `capabilities.context_editing.edits` lists a
  compaction edit where the catalog does, and `context_editing.compaction`
  the catalog's on-demand types (`summarize`); the catalog lists Anthropic's
  models with the `compact-2026-09-04` beta flag, which is what makes
  Anthropic publish `capabilities.compaction`. Anthropic's blocks on the
  Anthropic-shaped entry follow them (model-capabilities design §2.2.1).
  An Anthropic-compatible upstream that answers the catalog read with the
  flag with a 4xx is asked once more without it (a log line says so; the
  next read tries the flag again), and keeps its catalog, without the
  on-demand types.
- **Not yet.** OpenAI's own compaction on the Responses API
  (`/v1/responses/compact`, `compaction` input/output items) is a separate
  protocol lmgw does not carry yet. The live check (`scripts/compat_live`)
  has an on-demand scenario (`compaction`, api-compat design §4); threshold
  compaction has none yet, because its trigger needs a prompt of at least
  50,000 input tokens.

**`thinking.display`** *(2026-11-05)*: how Anthropic returns the thinking
text (`summarized`, `omitted`, a beta `updates`). Carried verbatim
(`ChatRequest::thinking_display`) and sent only to an Anthropic upstream,
inside the `thinking` object of a type that takes it (`adaptive`,
`enabled`); beside `disabled` or `between_tools` (an off from a header or the
reasoning fit), and to every other upstream, it is left out and named in
`x-lmgw-params-ignored` (`thinking.display`). The `between_tools` type
itself is the reasoning control's (model-capabilities design §5.2, §5.6).

**Thinking blocks in an Anthropic answer** *(2026-11-05)*: each one is a
`ContentPart::Reasoning` part with its `signature`, where it stood among the
answer's blocks (`egress/anthropic/thinking.rs`), and a stream's
`signature_delta` is `StreamDelta::ReasoningSignature`, which closes the
block. A Messages client gets every block as Anthropic sent it, an omitted
display's block (empty `thinking`, its signature; the default on current
Claude models, which think adaptively when the request names no `thinking`)
included, and sends it back signed. Before, both paths dropped the
signature, and the omitted block whole: the client saw output tokens and no
block they went to (seen in a live compatibility run). An answer with no
signed block keeps its trace apart, as before. *Changed 2026-11-05*:
`redacted_thinking` is carried verbatim, as every block the IR does not
model (below).

- **Where a signature goes** *(changed 2026-11-06)*. A signature lmgw read
  from an answer names the scope of the row that answered
  (`signature_scope`, an `ir::Signer`, kept in clear): the row's base URL
  normalized, its origin (scheme, host and port, lower-case, the scheme's
  own port dropped) and its path without trailing slashes or the `/v1` the
  Anthropic egress adds itself; no query, user, password, key, header or
  model. lmgw's own records (the agent loop's turns, the Chat's tool turns,
  a stored Responses chain) send the block to every Anthropic-protocol row
  at that scope, whatever its key, its headers or the model; to a row at
  another scope (a third-party Anthropic-compatible provider, a proxy) the
  block is left out whole. A signature a Messages client sends back
  carries no scope: it is the client's own data and goes as the client
  wrote it, to whichever Anthropic-protocol row answers. No other egress is
  sent a signature; they read the block's text as reasoning. Verbatim
  blocks, a text's citations and a compaction block (below and above,
  *changed 2026-11-06*) follow the same rule. One
  predicate, `ir::AnthropicSent`, decides what a row is sent: the egress
  renders by it, `x-lmgw-params-ignored` names by it, and lmgw's own
  estimates count by it.
  - *Why the path too* (changed 2026-11-06; the first 2026-11-06 rule was
    the bare origin). A gateway in front of several providers tells them
    apart by path at one origin: Cloudflare's AI Gateway
    (`https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic`),
    a LiteLLM proxy on `localhost:4000` with one path per provider. The
    bare origin made those one scope, so a block one of them signed went to
    the other. Two rows at `https://api.anthropic.com`, with or without
    `/v1`, still share one.
  - *Why the scope and not the row's key or the model.* Anthropic's
    thinking docs
    (<https://platform.claude.com/docs/en/build-with-claude/thinking>,
    "Thinking encryption" and "Switching models mid-conversation"): signature
    values work across the Claude API, Bedrock and Vertex, except that
    Claude Sonnet 5.5 and Claude Haiku 5.5 blocks work only in the account
    that made them; a block the model asked cannot read (another model's,
    another account's) is dropped by the API, unbilled, and the request
    succeeds; "keep passing thinking blocks back unchanged when you switch
    models", and stripping a block yourself can earn the 400s of a modified
    turn. The 2026-11-05 rule (the row's id plus a fingerprint of its key,
    address and headers) left out blocks Anthropic would have read (Claude
    Haiku 5.5 to Claude Sonnet 5.5 or Claude Opus 5.5 on the Claude API).
  - *Probes 2026-11-06* (a build that passes a Messages
    client's signed block as written, `anthropic/claude-haiku-5-5`,
    `anthropic/claude-sonnet-5-5`, `anthropic/claude-haiku-4-5-20251001`):
    a forged signature in a tool loop's thinking block is 400
    `invalid_request_error` "messages.1.content.0: Invalid `signature` in
    `thinking` block" (what another server's signature earns); a tool loop
    whose assistant turn has no thinking block, thinking on, is 200 on
    Claude Haiku 5.5 and Claude Sonnet 5.5 (adaptive) and on Claude Haiku 4.5
    (manual `budget_tokens`, answered with no thinking: Anthropic turns
    thinking off for such a request instead of refusing it, as the thinking
    docs' "mid-turn conflicts degrade gracefully" say). A real signature
    replayed to another model was not probed: that build relays thinking
    blocks with an empty signature, so no signed block could be had through
    it; the docs above are the source for that case.
  - *A trailing tool loop.* When the left-out thinking led the final
    assistant message of a tool loop, the request still goes out with the
    thinking control as asked (probes above): Anthropic goes on without that
    reasoning. Thinking `disabled` would itself be refused by Claude Opus 5.5
    and Claude Sonnet 5.5.
  - *Surfaced.* Every send that leaves blocks out logs one INFO line (how
    many thinking blocks, cited texts and folded compaction blocks, which
    block types, the scopes that signed them, the row's, whether the
    trailing tool loop's thinking was among them, and *changed 2026-11-06*
    how many cited texts with no text it left out, which the header named
    before the line did), and a response whose headers have not left names
    them in `x-lmgw-params-ignored` as `content.thinking`,
    `content.citations`, `content.compaction` and `content.<type>`: a `/v1/responses` run that continues a stored
    chain, at open (a client's own blocks on `/v1/messages` carry no scope,
    so they are never left out this way; the handler asks
    `ChatRequest::blocks_ignored` all the same). The agent loop and the
    Chat's tool turns have no client headers; their note is the log line.
  - *Not yet:* lmgw cannot tell two scopes that reach the same provider (a
    proxy in front of Anthropic, Bedrock behind a gateway), so a block never
    crosses scopes.
- **Text as it came.** A signed block's text reaches the Messages client
  and lmgw's records byte for byte, a leading line break included. The
  blank line lmgw's OpenAI decoder puts between two reasoning items'
  summaries is still left off where an item opens a block of its own; it
  can only follow unsigned or sealed reasoning, so that is where the trim
  applies.
- **Surfaces.** Like seals (WP8 review S-R2), a record's signatures are the
  upstream's to read: `GET /chat/api/threads/{id}`, the device feed and both
  exports show a record without `signature` and `signature_scope`, the text
  kept, and *(changed 2026-11-06)* without a text's `citations` and
  `citations_scope` (a web search citation's `encrypted_index` is the
  provider's too), the cited text kept, and without a compaction block's
  `signature`, `encrypted_content` and `scope`, its summary kept. The Chat's plain turn keeps no signature yet: it stores the trace as
  text, which the Anthropic egress drops unsigned. A signed block counts as
  reasoning for the reasoning-off check, an omitted display's empty one too.

**Anthropic blocks lmgw does not model** *(changed 2026-11-05, the owner's
ruling: carry them verbatim)*: `redacted_thinking`, the provider's own
tool blocks (`server_tool_use`, `web_search_tool_result`, `mcp_tool_use`,
…), and any type Anthropic adds after the pinned schema are one IR part,
`ContentPart::AnthropicBlock` (`ir/anthropic_block.rs`): the block exactly
as it came. Before, the answer parse and the stream decoder dropped them
silently, and a request carrying any type but `redacted_thinking` (also
dropped) was refused with 400 `Unsupported`.

- **Answers.** A whole answer's block stands where it stood. A streamed
  block is its `content_block_start`, every `content_block_delta` at its
  index relayed as it came (`StreamDelta::AnthropicBlock`,
  `BlockEvent::Start`/`Delta`), and its `content_block_stop` when the
  upstream's came (`BlockEvent::Stop`, *changed 2026-11-06*: before, the
  Messages writer closed it only where the next block started, so a
  `server_tool_use` block stayed open for the seconds a web search ran): the
  Messages client gets the same event names, the same JSON (key order is
  serde_json's, as in every frame lmgw writes) and the same index order
  relative to the other blocks. The in-process assembler folds the deltas
  into the whole block for its record (`AnthropicBlock::apply_delta`:
  `input_json_delta` into `input`, `citations_delta` into `citations`, any
  other delta's fields extend or set the block's field of that name).
- **Requests.** A Messages client's block goes to an Anthropic-protocol
  upstream as the client wrote it, whatever its type. Every other upstream
  is sent none of it, and `x-lmgw-params-ignored` names each type once as
  `content.<type>` (a type that is no plain token is named
  `content.block`). Only a block without a string `type` is refused, 400,
  as no Anthropic block. *Changed 2026-11-06*: an `image` block whose
  source is neither base64 nor a URL (a Files API `file`) is carried the
  same way, named `content.image` off an Anthropic route, as a `document`
  with that source already was; before, it was refused with 400
  `Unsupported`. An image block without a source type still is. *(Changed
  2026-11-06, second pass)* Such an image is an image only where the row is
  sent it (`AnthropicBlock::image_sent_to`, by `ir::AnthropicSent`): on an
  Anthropic-protocol fallback that cannot see it gets the placeholder
  "[image by Files API reference — omitted: …]", no file id in it
  (`gate::fallback_images`), and `/v1/messages/count_tokens` there puts the
  same placeholder into the client's body. On every other row it is a
  dropped part, named `content.image`, that costs nothing: no per-image
  bound (`gate::media_parts`), no placeholder, no `media_omitted` on a
  count. The Vision facet of a candidate alias does not count it either, as
  a candidate is a local row. The first 2026-11-06 pass counted it as an
  image on every row, which asked a local row's projector to bound and a
  blind OpenAI-shaped fallback to placeholder an image neither was sent.
  The same image inside a `tool_result` is a verbatim block too
  (`ToolResultBlock::AnthropicBlock`): an Anthropic row gets it as written,
  every other row none of it, named `content.image`; before, it was kept as
  JSON, which an OpenAI-shaped egress inlined into the tool's text, file id
  and all. *(Changed 2026-11-06, third pass)* So is every other block a
  `tool_result` carries but `text` and an inline or URL image: Anthropic's
  `tool_result.content` takes `text`, `image`, `document` and
  `search_result` blocks (handle-tool-calls docs; a browser use member's
  result a `browser_state` too, a tool search's a `tool_reference`). An
  Anthropic row gets each as written, a block; every other row gets what of
  it reads as text (`AnthropicBlock::tool_text`, the one predicate the
  egresses' flattening, the header and the estimates read): a plain-text
  (`text`) document's `title`, `context` and text; a custom-content
  (`content`) document's text blocks, an image among them named
  `content.document`; a `search_result`'s `title`, `source` and text
  content. A `document` with a base64 (PDF), `url` or Files API `file`
  source has no text form, and no egress here sends a document input yet
  (no OpenAI `input_file`, no Gemini `inlineData` PDF): it is left out and
  named `content.document`, its URL or file id never inlined. Any other
  type (`browser_state`, `tool_reference`, one Anthropic adds next) is its
  JSON as text there, as it was *(changed 2026-11-06: without its
  `cache_control`, and a block that holds what no local counter reads,
  base64 `data`, a `file_id`, `encrypted_content`, a URL, file or base64
  `source`, is left out and named `content.<type>` instead)*. *(Changed
  2026-11-06)* A document or search result that asked for `citations` is
  also named `content.citations`; one with nothing to read but a title (an
  empty `data` or `content`) is its title, one with nothing at all is left
  out and named; two verbatim blocks' texts are joined by a blank line
  rather than a line break, so one document's end and the next title tell
  apart. Before, all of them were kept as JSON:
  an Anthropic row got the JSON as a text block instead of the block, and
  every other row the JSON inlined, a document's file id and all. A
  message's own (top-level) `document` and `search_result` blocks are still
  sent to no row but an Anthropic one; their text form is not used there
  yet.
- **Other clients.** A Chat Completions or Responses client is never given
  one. A whole answer's response names each type in `x-lmgw-params-ignored`
  as `answer.<type>`; a streamed answer's headers have left before the
  block comes, and lmgw has no late note for a stream yet, so a stream's
  drop is an INFO log line. *(2026-11-06)* A run that continues a stored
  chain (`previous_response_id`) names, at open, what of the chain's
  recorded blocks its row is not sent (`content.<type>`, `content.thinking`
  and `content.citations` from another scope). `GET /v1/responses/{id}`
  serves the stored body as it was served, which never held the blocks;
  the response that first
  served it named them, and the GET carries no such header yet.
- **Records.** lmgw's own records keep the block with the scope of the
  row that answered and replay it to rows at that scope only, as a
  signature (above, *changed 2026-11-06*); a client's block has none. Pages, the device feed and both exports
  never show one. The Chat's plain turn keeps none yet (its reply has no
  column for one).

**Citations** *(2026-11-06)*: an Anthropic `text` block's `citations` (a
document's `char_location`, `page_location`, `content_block_location`, a
search result's or a web search's `web_search_result_location`) are the
text part's own (`ContentPart::Text`'s `citations`, verbatim). Before, the
answer parse and the stream decoder dropped them on both paths, which the
relayed `web_search_tool_result` made visible.

- **Messages clients on an Anthropic route** get them as Anthropic sent
  them: in a whole answer on the block; streamed, each `citations_delta` at
  the text block it cites, after that block's `text_delta` events and
  before its stop, the order the citations docs' "Streaming support"
  example shows (<https://platform.claude.com/docs/en/build-with-claude/citations>);
  the block opens as `{"type": "text", "text": ""}` with no `citations`
  key, as the streaming docs' text blocks do
  (<https://platform.claude.com/docs/en/build-with-claude/streaming>,
  checked 2026-11-06). The stream decoder
  holds a text block's citations until its `content_block_stop` and sends
  them with it (`StreamDelta::TextBlockEnd`), which also ends the client's
  text block there, so a cited block never merges with the text around it;
  the citations reach the client a few milliseconds later than Anthropic's
  own events did, in the same order, on the same block.
- **Back to Anthropic.** A cited block a Messages client or lmgw's own
  record sends back reaches an Anthropic-protocol row with its citations
  (the in-process assembler keeps each cited block a text part of its own,
  as a whole answer's parse does). *(Changed 2026-11-06)* Citations lmgw
  read from an answer carry the scope of the row that answered
  (`citations_scope`), as a signature does (above): a web search's
  `encrypted_index` points into results only that provider can decrypt.
  A row at another scope is sent the text without them, logged and named
  `content.citations`; a client's citations carry no scope and go as
  written. A text with citations but no text is not sent, as Anthropic's
  `TextBlockParam.text` has `minLength: 1`
  (<https://platform.claude.com/docs/en/api/messages/create>), and its
  citations are named `content.citations` and counted in the send's INFO
  line (`textless_citations`, changed 2026-11-06). Two user texts the Chat merges
  keep both texts' citations, in order; texts whose citations came from
  two scopes stay two texts.
- **Everyone else.** The text reaches every other client and upstream; the
  citations do not. A whole answer to a Chat Completions or Responses client
  names `answer.citations` in `x-lmgw-params-ignored`; a stream's headers
  left before, so its drop is an INFO log line; a request sent to a row of
  another protocol names `content.citations`. Mapping them onto Chat
  Completions' and Responses' `url_citation` annotations is not yet done:
  those need character offsets into the whole output text, and only a web
  search's citations carry a URL.

**Counting** *(changed 2026-11-06)*: lmgw's own prompt estimates (a stopped
or unanswered call's row, ~4 characters a token, and the budgets' floor
count; `proxy/count_messages/estimate.rs`) count what the request sent on
its route. On an Anthropic route that is a verbatim block's strings too (a
`document`'s plain-text source and title, a `search_result`'s content, a
provider tool's input and result) and a cited text's citations; before, a
verbatim block counted as nothing. *(Changed 2026-11-06, second pass)* A
citation counts without its `cited_text`, which Anthropic does not count
as input when it is passed back (citations docs, "Token costs"), and a web
search citation without its `title` and `url` too (web search tool docs,
"Citations": those three fields "do not count toward input or output token
usage"). And an Anthropic route counts only what its row at its scope is
sent (`ir::AnthropicSent`): a thinking block, verbatim block or citations
another scope signed are neither counted nor named, and an unsigned trace,
which the egress drops, is not counted there. What no local counter can read is left
out and said: images and audio on every route, and on an Anthropic route a
base64 or Files API `document` or `image`, encrypted content, and a signed
thinking block's thinking (its signature holds what Anthropic bills). The
row's message ends "the estimate leaves out what lmgw cannot count locally
(content.document, …)", and a floor's note says how many kinds of part the
prompt count leaves out, the platform tokenizer's count on an `openai` row
included. Off an Anthropic route a verbatim block is not sent, so neither
counted nor named. *(Changed 2026-11-06, third pass)* A `tool_result`'s
verbatim block counts as its row is sent it: on an Anthropic route its
strings (a PDF or Files API document named `content.document`), on every
other route its text form, which leaves nothing out; *(changed 2026-11-06)*
a by-URL document or image counts as a Files API one does: its URL is not
counted and the block is named; `/v1/messages/count_tokens`
off an Anthropic route counts the same text, as the egress sends it.

What a model takes is on `GET /v1/models`: `capabilities.context_editing`
on an Anthropic route, and Anthropic's own `context_management` block on the
Anthropic-shaped entry (model-capabilities design §2.1, §2.2.1). Both list
only what lmgw serves, and the catalog says per model: `context_editing.edits`
names a compaction edit where the catalog does, and the block's
`compact_20260112` says the same *(changed 2026-11-05: both left it out
before)*; the block's `supported` is `true` only where the catalog says so.
On-demand compaction is `context_editing.compaction` and Anthropic's own
`compaction` block.

## 7. Egress adapters

One isolated module per upstream protocol, each implementing a trait:

```
trait Egress {
    fn build_request(ir: &ChatRequest, model: &str, overrides) -> http::Request;
    fn parse_response(resp) -> Completion;            // non-stream
    fn parse_chunk(bytes) -> Vec<StreamDelta>;        // SSE → IR deltas
    fn map_error(status, body) -> GatewayError;
}
```

- **openai** — near pass-through; also serves llama-server (kind `llama_server`).
- **anthropic** — `/v1/messages`, `x-api-key` + `anthropic-version` headers,
  content-block SSE events.
- **gemini** — `:generateContent` / `:streamGenerateContent`, `contents[]`/`parts`
  shape, `role: "model"`, key via query/header.

Upstream calls use `reqwest` streaming + `reqwest-eventsource`. Per-upstream
timeout; client cancellation propagates (drop → abort upstream request).

### 7.1 Gemini thought signatures (added 2026-10-09)

Google's rules ("Thought signatures" for `generateContent`,
<https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures>,
read 2026-10-09):

- A signature is an opaque `thoughtSignature` on a response part. Gemini 3
  puts one on the **first `functionCall` of each step** (parallel calls: on
  the first only), and on the last part of an answer without calls when it
  thought. Gemini 2.5 puts one on the first part of a response with calls,
  whatever its type. Streamed, a text answer's signature may come in a part
  with empty text, so a client reads to `finishReason`.
- Gemini 3 **validates** the first `functionCall` of each step of the
  **current turn** — everything after the most recent user message with
  standard content (text, not a `functionResponse`) — and answers 400
  without its signature. Earlier turns are not checked. Text-part signatures
  are recommended back but never checked. For Gemini 2.5 returning any of
  them is optional.
- A call the API did not generate (injected by a client, or from a model
  without signatures) may carry `skip_thought_signature_validator` (or
  `context_engineering_is_the_way_to_go`) to skip the check.
- Google's OpenAI-compatibility layer carries the signature in a
  non-standard `extra_content.google.thought_signature` on each tool call.

**Where lmgw keeps it: in the call's id.** Gemini has no call ids, so the
egress mints them: `call_` + a 12-hex-digit random tag per answer + a hex
counter, as the realtime session mints its own. Ids are unique across
answers, not numbered per answer. A per-answer `call_0` would repeat across
the steps of one history: an Anthropic upstream (a fallback, an alias
switched mid-conversation) refuses duplicate `tool_use` ids, a result would
pair with a newer step's call of the same id, and `@openai/agents` refuses a
reused id. For a signed part the egress appends a marker, a CRC-32 of the
signature (eight hex digits), and the signature as a payload
(`ir::call_id_with_signature` and `split_call_id`):

- Google's signatures are standard padded base64. lmgw decodes them to bytes
  and writes those in unpadded URL-safe base64 after `__thoughtsig_`, a
  quarter shorter than encoding the text again. A signature that is not
  canonical standard base64 goes after `__thoughtsigtxt_` as its UTF-8 text
  in unpadded URL-safe base64. Both are lossless.
- The CRC guards against a client that truncated or rewrote the id. A tail
  that does not decode or does not match its CRC reads as a call without a
  signature (bare id before the marker, skip value on Gemini). A corrupt
  signature would be a 400.

Reasons for the id:

- The id is the one value every client shape echoes verbatim: OpenAI chat
  `tool_calls[].id` and `tool_call_id`, Anthropic `tool_use.id` and
  `tool_use_id`, Responses and Realtime `call_id`. A client that drops
  unknown fields still sends it back.
- An OpenAI- or Anthropic-shaped answer gains no field its protocol lacks.
  Google's own `extra_content` would be a visible extension, which
  OpenAI-shaped routes do not add (stay true to the OpenAI API), and most
  clients would drop it anyway.
- The IR, its stream deltas, the tool loops and the Chat's stored records
  carry ids untouched. A thread on Gemini therefore keeps its signatures in
  storage, and no type, migration or encoder changes.
- The encoding stays inside `[A-Za-z0-9_-]`, the alphabet Anthropic allows
  in an id. The cost is length: a signed id runs to hundreds of characters.
  The API docs tell clients to echo ids exactly and not to send lmgw's ids
  to another provider themselves.

The rejected alternative was a signature field on `ContentPart::ToolUse`,
encoded into the id by each ingress's serializer and decoded by each
parser. That touches every construction site and every stream encoder
to land in the same place, the id.

**Replay.** The Gemini egress puts a captured signature back on its own
`functionCall` part, in every turn, as Google recommends. Then
`egress::gemini::signatures::sign_unsigned_steps` gives the skip value to
the first `functionCall` of **every** model step that has none (a call from
another model or a fallback, a client that rewrote the id, lmgw's own
synthetic `lmgw_task_` calls). Gemini 3 validates only the current turn,
but lmgw does not depend on drawing that boundary exactly where Google
does: the skip value is documented for any call the API did not generate,
and an older step is not validated, so it is harmless there. Later calls of
a parallel step get nothing. A result whose message carries no tool name
pairs with the nearest call of its id before it
(`ChatRequest::tool_name_for_result`), falling back to bare-id matching for
a client that kept the signature on the call but not on its result.

Every other egress (OpenAI wire, llama.cpp, Anthropic) sends the bare id
(`ir::wire_call_id`). Those upstreams see the id the call was minted with,
and no signature reaches a prompt template or another provider's id checks.
The routes that forward the client's own body instead of one built from the
IR strip it there: the native `/v1/responses` passthrough (every input
item's `call_id`, `ir::wire_call_ids_in_responses_body`) and
`/v1/messages/count_tokens` on an Anthropic upstream (`tool_use.id` and
`tool_use_id`, `ir::wire_call_ids_in_messages_body`), where a signature
would also be counted as prompt. The other body-forwarding routes carry no
tool call ids: `/tokenize` forwards `{model, content}` text, `/v1/count_tokens`
takes a string, the legacy `/v1/completions` a prompt, and the local
counts send the body the egress built.

**Not carried:** signatures on text parts (Gemini 3's last part, Gemini
2.5's first part when it is text). No client-echoed slot exists for them,
and the API never checks them. The skip value goes to every Gemini model.
The docs do not say whether a pre-3 model, which checks nothing, accepts
it; Gemini 2.5 could not be checked live (below), Gemma 4 accepts it.
Tests: `tests/it/gemini_signatures`
(capture, replay goldens across the three client shapes, damaged ids,
unique ids across steps and on an Anthropic upstream, the passthrough
routes, the Chat tool loop).

**Verified against the live API, 2026-10-09** (build 4bae309e, a dev
instance with a `gemini` upstream on `generativelanguage.googleapis.com`).
One tool, `get_weather(city)`, and a prompt asking for Berlin, then Paris,
one call per step: two tool steps and a final answer. Every client id was
echoed exactly. Control, sent straight to the API: a `functionCall` without
a signature in the current turn gets 400 from `gemini-3.1-flash-lite`
("Function call is missing a thought_signature in functionCall parts") and
200 from `gemma-4-26b-a4b-it`.

| # | Route | Stream | Model | Ids sent back | Result |
|---|-------|--------|-------|---------------|--------|
| 1 | `/v1/chat/completions` | no | `gemini-3.1-flash-lite` | signed (195 chars) | 200, 200, 200 (call, call, answer) |
| 1 | `/v1/chat/completions` | yes | `gemini-3.1-flash-lite` | signed (195 chars) | 200, 200, 200 |
| 2 | `/v1/messages` | no | `gemini-3.1-flash-lite` | signed (195 chars) | 200, 200, 200 |
| 2 | `/v1/messages` | yes | `gemini-3.1-flash-lite` | signed (195 chars) | 200, 200, 200 |
| 3 | `/v1/chat/completions` | no | `gemini-3.1-flash-lite` | made-up `call_x` | 200 (skip value) |
| 3 | `/v1/chat/completions` | no | `gemini-3.1-flash-lite` | signed id cut to its bare part | 200 (skip value) |
| 4 | `/v1/chat/completions` | no | `gemini-3.1-flash-lite` | signed id clipped mid-payload (106 of 195) | 200 (CRC fails, skip value) |
| 4 | `/v1/chat/completions` | no | `gemini-3.1-flash-lite` | intact signed id (control) | 200 |
| 5 | `/v1/chat/completions` | no | `gemma-4-26b-a4b-it` | signed (93 chars) | 200, 200, 200 |
| 5 | `/v1/chat/completions` | no | `gemma-4-26b-a4b-it` | made-up `call_x` | 200 (skip value accepted) |

Every new tool call carried `__thoughtsig_`, streamed or not, Gemma 4
included: it signs its calls but does not check them. The replays (3, 4)
are one tool step in the current turn; each answer was a further signed
call. Gemini 2.5 was not testable: `gemini-2.5-flash`, `-flash-lite` and
`-pro` answer 404 "no longer available to new users" for the key used, so
whether 2.5 accepts the skip value is still open.

## 8. llama-server router mode + Podman management

The app owns the router's **model preset and container lifecycle**. llama-server
launched without `-m` runs in *router mode*: a parent process that spawns one
llama-server instance per requested model, loading/unloading on demand
(`--models-max` bounds concurrently loaded models; `sleep-idle-seconds` per
model frees memory after idle).

- **Preset generation:** local-model entries render `models-preset.ini`
  (one `[model-id]` section per model: `model = /models/<gguf>` + llama-server
  flags as INI keys + `sleep-idle-seconds`) to a known config path.
- **Container lifecycle:** the Podman manager shells out to the `podman` CLI via
  `tokio::process` (predictable, no API-version coupling) to run/stop/restart the
  llama-server container (`ghcr.io/ggml-org/llama.cpp:server-cuda`, entrypoint
  `/app/llama-server`, `--models-preset /config/models-preset.ini`), mounting the
  generated preset and the models/GGUF dir, with GPU access
  (`--device nvidia.com/gpu=all` / CDI) and the API port.
- **Reload on change:** after writing the preset, restart the container
  (llama-server also offers `GET /models?reload=1` as a lighter refresh;
  restart is the always-correct fallback).
- **Health/status:** manager tracks container state (running/stopped/health) and
  surfaces it to the UI/tray.
- **Speculative decoding / MTP:** a local model can reference an external draft
  GGUF (e.g. MTP heads) via structured params; the preset renders
  `model-draft = /models/<path>` plus `spec-type` (`draft-mtp`, …),
  `spec-draft-n-max`/`-n-min` and `spec-draft-ngl`. The draft file lives in the
  same mounted models dir as the main model.
- **HF model manager:** browse a Hugging Face repo's GGUF files (tree API),
  download into `<models dir>/<owner>/<repo>/…` (streamed to `.part`, atomic
  rename; split `-NNNNN-of-MMMMM.gguf` files fetch all parts), tracked in
  `hf_models` together with the resolve-URL ETag. "Check for updates" HEADs the
  resolve URL and flags rows whose ETag changed; re-download updates in place.
  Interrupted downloads restart on app launch. `HF_ENDPOINT` overrides the hub
  base URL; an optional token (Settings) unlocks gated/private repos.

The router is registered as an `openai`-protocol upstream of kind `llama_server`
pointing at its local port, so routing to local models needs no special case —
the router itself dispatches on the request's `model` field.

## 9. Data model (SQLite)

```
upstreams(
  id, name, protocol TEXT[openai|anthropic|gemini], kind TEXT[generic|llama_server],
  base_url, api_key, extra_headers JSON, timeout_ms, enabled, created_at, updated_at)

models(  -- aliases
  id, alias UNIQUE, upstream_id FK, upstream_model_id,
  param_overrides JSON, enabled, created_at, updated_at)

local_models(  -- belong to the llama_server upstream; feed preset generation
  id, model_id UNIQUE, params JSON, args JSON, idle_seconds, gguf_path,
  enabled, created_at, updated_at)

hf_models(  -- Hugging Face downloads tracked in the models dir
  id, repo, file, dest_path, etag, size_bytes,
  status TEXT[queued|downloading|done|failed|update_available],
  error, downloaded_at, created_at, UNIQUE(repo, file))

api_keys(  -- gateway ingress keys (auth optional, see §13)
  id, name, key_hash, enabled, created_at)

request_logs(
  id, ts, client_key, ingress_proto, requested_alias, upstream_id, upstream_model,
  egress_proto, status, ttfb_ms, total_ms, prompt_tokens, completion_tokens,
  streamed, error_kind, error_msg)

settings(key PRIMARY KEY, value)
```

Access via `sqlx` (SQLite, async, compile-checked queries). DB file `0600`.
Config is cached in-memory (`Arc<Snapshot>`), atomically swapped on edit so the
hot path does no DB reads.

*Changed 2026-10-08:* every write transaction takes the write lock at its BEGIN
(`BEGIN IMMEDIATE`, `store::begin_write`), in the knowledge and corpus stores too.
The file is in WAL mode with eight connections, and a deferred transaction that
read before it wrote failed at once with "database is locked" when another
connection committed in between; the busy timeout does not cover that, and a
client saw a 500. Nothing slow runs inside a write transaction, since every other
writer waits for its lock. The busy timeout is named (`store::BUSY_TIMEOUT`, 5 s,
sqlx's default). Each wait for the write lock is logged at debug. A wait past half
the busy timeout opens a contention episode with one warning that names the waiting
caller (the holder is not tracked), and the episode lasts until no writer waits for
that database any more; its end is logged with how many writers waited past the
threshold, the longest wait and how many gave up, at info, or as a warning when one
gave up. One slow holder used to warn once per writer queued behind it.

## 10. Observability (logs + live metrics)

Every request produces a `request_logs` row and a push onto a
`tokio::broadcast` channel. The dashboard subscribes via SSE (`GET /events`,
HTMX `sse-swap`) to render a live request feed and live counters (req/s, error
rate, tokens, active-by-model/upstream). A filterable log table backs it with
history; clicking a row shows request detail. Retention configurable (keep last
N rows / N days); a periodic task prunes. Secrets are never logged.

## 11. Web UI (HTMX + Askama)

> **Superseded (2026-08-29).** The htmx/Askama UI was replaced by a Leptos CSR
> single-page app (`crates/lmgw-ui`) over a JSON + SSE `/api` plane, and deleted
> at the P8 cutover. What is still accurate below is the *feature* inventory —
> the same surfaces exist, regrouped into a sidebar. See
> [docs/design/ui-rebuild/plan.md](ui-rebuild/plan.md) for the target
> architecture and [parity.md](ui-rebuild/parity.md) for the as-is
> inventory this replaced.

Server-rendered pages, embedded via `rust-embed`, progressively enhanced with
HTMX; SSE for live regions. No JS build step.

- **Dashboard** — live request feed + counters + recent errors; llama-server status.
- **Upstreams** — CRUD + "test connection".
- **Models / aliases** — CRUD; map alias → upstream + model; param defaults.
- **Local models** — CRUD; generate/reload the router preset; container controls + status.
- **HF models** — browse a repo's GGUFs, download with live progress (polled
  HTMX partial), update checks, one-click "add as local model".
- **Logs** — filterable table; per-request drill-down.
- **Settings** — gateway keys + auth toggle, bind address, retention, paths,
  HF access token.

## 12. Tauri shell & tray

- **Tray menu:** status line (running / port / llama-server state) · Open Dashboard
  (show/focus window) · Open in Browser · Start/Stop llama-server · Restart gateway
  · Quit.
- **Window:** single webview loading `http://127.0.0.1:<port>/`. Closing the
  window hides to tray (server keeps running); Quit exits.
- **Single instance:** `tauri-plugin-single-instance` so a second launch focuses
  the existing tray app.
- **Startup order:** spawn the Axum server in Tauri `setup()`, then create the
  window pointing at the local URL (webview retries until the server is ready).
- **Optional (flagged, not committed for v1):** autostart on login
  (`tauri-plugin-autostart`); native notifications on upstream errors.

### Wayland / NVIDIA (required on NVIDIA systems)
> **Superseded 2026-09-28:** lmgw now sets `__NV_DISABLE_EXPLICIT_SYNC=1` instead
> (NVIDIA only, and only when no WebKit render variable is already set). It
> avoids the Error 71 crash *and* keeps the zero-copy hardware renderer;
> `WEBKIT_DISABLE_DMABUF_RENDERER=1` means software compositing, measured
> 2026-09-27 at 21 fps vs 60 (1600×1000) and 4 vs 60 (5000×1400) on a 24 GiB GPU.
> An explicit `WEBKIT_DISABLE_DMABUF_RENDERER=1 lmgw` stays the escape hatch.
> See `render_workaround` in `src-tauri/src/main.rs`. The text below is the
> original design.

WebKitGTK (Tauri/`wry`) crashes on NVIDIA+Wayland with
`Gdk-Message: Error 71 (Protocol error)` unless `WEBKIT_DISABLE_DMABUF_RENDERER=1`
is set. Mitigation:
- Set it **early in `main()`** (`std::env::set_var(...)`) before the webview/GTK
  initializes, and in the dev command:
  `WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo tauri dev`.
- Also set it in the packaged launcher (`.desktop`/wrapper) as a belt-and-suspenders.
- Do **not** use `GDK_BACKEND=x11` (mis-renders on NVIDIA via XWayland).
- *Verify the in-`main` `set_var` reliably prevents the crash on this box during
  the first Tauri spike; if not, fall back to the launcher env.*

Fedora prerequisites: `webkit2gtk4.1-devel`, `libsoup3-devel`, GTK/`ayatana`
appindicator dev packages for the tray.

## 13. Auth & secrets

- **Auth off by default** (trusted LAN). When enabled, ingress requires a gateway
  API key (`Authorization: Bearer` / `x-api-key`); keys stored hashed in
  `api_keys`. Binding `0.0.0.0` should prompt enabling auth in the UI.
- **Upstream secrets:** plaintext in SQLite; DB file `0600`; never logged or
  rendered in full in the UI (masked). Optional at-rest encryption is a v2 item.

## 14. Error handling

Single `GatewayError` type → normalized error responses **in the client's
protocol** (OpenAI- or Anthropic-shaped) with sensible HTTP status:
unknown alias → 404, auth → 401, bad request → 400, upstream/transport → 502,
timeout → 504, upstream-returned errors mapped through with provider detail
preserved in the log. Mid-stream upstream failure terminates the client stream
cleanly and logs the partial (ttfb captured, completion marked errored). UI
config/validation errors render inline.

## 15. Testing strategy (TDD)

- **Adapter golden tests:** OpenAI↔IR↔Anthropic↔Gemini for non-stream and
  streamed chunk sequences, from recorded fixtures (per provider).
- **Ingress round-trips:** OpenAI-in and Anthropic-in → IR → re-serialized out.
- **Router tests:** alias resolution + param-override merge precedence.
- **llama-server preset:** snapshot tests of generated INI.
- **Podman manager:** command-construction tests (mock the process runner); a
  gated integration test that actually starts/stops a container.
- **End-to-end proxy:** against a `wiremock` upstream, asserting translated
  output (both directions) + a correct `request_logs` row + a live-feed event.
- Build order to de-risk translation: **text → tool calling → multimodal/images.**

## 16. Crates & tooling

`tauri` v2 (tray-icon, single-instance; optional autostart/notification) ·
`axum` · `tokio` · `tower-http` (trace/cors/timeout/compression) ·
`reqwest` (stream) + `reqwest-eventsource` · `serde`/`serde_json` ·
`sqlx` (SQLite) · `rust-embed` (was: `askama`, removed at the P8 UI cutover) ·
`tracing` + `tracing-subscriber` · `secrecy` (in-memory key handling) ·
`thiserror`/`anyhow`. Build/run via `cargo tauri` (no Node toolchain required;
the webview loads the Axum URL rather than a JS bundle).

## 17. Build milestones (for the implementation plan)

1. **Skeleton:** workspace (`lmgw-core` lib + `lmgw` Tauri bin); Axum server task
   started from Tauri setup; tray with Quit + Open Dashboard; window → localhost;
   Wayland env workaround verified. SQLite + migrations.
2. **Config plane:** upstreams + aliases CRUD (UI + store + in-memory snapshot);
   `GET /v1/models`.
3. **Proxy core (text):** IR + OpenAI ingress + OpenAI egress, streaming pass-through;
   request logging + live feed; Dashboard + Logs pages.
4. **Multi-egress:** Anthropic + Gemini egress adapters (text → tools → images).
5. **Anthropic ingress:** `/v1/messages` parser + serializer (stream + non-stream).
6. **llama-server router + Podman:** local-model CRUD, preset generation,
   container lifecycle/reload, status in UI + tray.
7. **Polish:** auth toggle + keys, retention/pruning, test-connection, settings,
   error normalization pass, packaging (.rpm/AppImage) with the env workaround.

## 18. Open verification items (resolve during implementation, not blocking)
- Whether `GET /models?reload=1` suffices as the preset **reload** mechanism (vs container restart).
- Reliability of `WEBKIT_DISABLE_DMABUF_RENDERER` set inside `main()` vs launcher.
- Podman **CDI / GPU** flags for the llama-server container on this Fedora+NVIDIA host.
- Gemini streaming detail (SSE vs chunked JSON) and exact usage-token reporting.
- Tauri v2 external-URL window + tray feature flags / Fedora system deps.
