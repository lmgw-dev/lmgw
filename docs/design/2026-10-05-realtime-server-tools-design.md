# Server-side MCP tools on `/v1/realtime`

Requested 2026-10-05: `/v1/realtime` had no tool support. **Draft v2**: v1
plus an adversarial review against the code and the `@openai/agents` sources (appendix).
Companion to [2026-10-01-realtime-voice-design.md](2026-10-01-realtime-voice-design.md)
("realtime §n"); it builds realtime §19's "server-side tools" for **sessions not bound to a chat
thread**. A session bound to a thread already runs the thread's MCP tools through the Chat's own
turn (chat-voice §8.2) and is not touched.

**Today.** An unbound session takes client `function` tools only (realtime §7.4). A
`{type: "mcp"}` tool fails to parse and the client gets an `error` naming the variant
(`realtime/protocol/tools.rs:4`). `/v1/responses` already runs `mcp` tools server-side
(`ingress/responses.rs`, `mcp/exec.rs`), resolved against lmgw's own MCP servers and the
built-in toolsets `lmgw`, `docs` and `kb`, under the key's tool scope (`mcp/scope.rs`).

**What OpenAI's Realtime API does** (protocol research 2026-10-05: openai-python 3.22.1 types,
`@openai/agents-realtime` 0.18.0 sources, the Realtime MCP guide, and an OpenAI support answer of
2026-07-27):
- A session tool `{type: "mcp", server_label, server_url | connector_id, allowed_tools?,
  require_approval?, headers?, authorization?, server_description?}`. Defining it lists the
  server's tools once, as an `mcp_list_tools` conversation item.
- **An MCP call ends the response.** The server runs the call, and the client must send
  `response.create` for the answer: "The Realtime API doesn't create these follow-up responses
  automatically." `@openai/agents` sends it by itself on the `mcp_call`'s
  `conversation.item.done` (`automaticallyTriggerResponseForMcpToolCalls`, on by default), whatever
  the call's outcome, and holds and merges the ones that arrive while a response runs.
- Items `mcp_list_tools`, `mcp_call` (`output` string|null, `error` null|object),
  `mcp_approval_request`, `mcp_approval_response`. Events `mcp_list_tools.{in_progress,completed,
  failed}`, `response.mcp_call_arguments.{delta,done}`, `response.mcp_call.{in_progress,completed,
  failed}`.

**The first client**, a voice client coordinated 2026-10-05, declares its toolsets in
`session.update`, sends the follow-up `response.create` when a `response.done` carries an
`mcp_call`, shows a tool spinner from the `mcp_call`'s
`output_item.added` to its `mcp_call.completed`/`.failed`, and treats `mcp_list_tools_failed` as
non-fatal. It relies on exactly these names: item type `mcp_call`, `response.output[]` carrying
the `mcp_call` items, `name` and `server_label` on the item.

## Decisions (taken in this draft; overturn any in review)

1. **Protocol-faithful: an MCP call ends the response, and the client asks for the answer** (§2).
   *Confirmed 2026-10-05: staying true to the OpenAI API was the correct decision* — no server
   follow-up, no default toolset. The gateway runs the response's calls **before** its
   `response.done`, so the stock client's automatic follow-ups for every call (and a client
   function call's) merge into one `response.create`. *Why:* a server that answered by itself
   would answer twice for `@openai/agents`. It is also how client function tools work today.
   The first client implements it.
2. **Labels, not URLs: one resolver with `/v1/responses`** (§1). `server_label` names a registered
   server (by tool prefix or name) or a built-in toolset (`lmgw` owner-only and bound to the
   `self_admin` mode, `docs`, `kb`), via `mcp::exec::resolve` under `ToolScope::of_request` and the
   owner's per-tool disables. `server_url`, `connector_id`, `headers`, `authorization` and
   `server_description` are accepted and never used: lmgw never dials a URL a client names.
   `authorization` and header values are redacted in every echo.
3. **No implicit toolsets.** A session has the MCP tools its client names, nothing else, as on
   `/v1/responses`. The first client does not need a server-side default.
4. **No approvals yet** (§6). `require_approval` is accepted when it gates nothing (absent, `null`,
   `"never"`); anything that would gate a tool is refused with an `error` saying approvals are not
   built on `/v1/realtime` yet. *Why:* `@openai/agents` sends `"never"` unless told otherwise,
   the first client does not use them, and most of the review's hard findings were approval state
   (who owns a call that outlives its response). lmgw's own default stays `never` on both routes.
5. **A cancel or barge-in abandons a running call** (§2.5). The call's wait ends at once, and its
   item gets the Chat's "abandoned, may or may not have run" error (`ABANDONED_CALL`). *Why:* one
   rule with the Chat. **Speech while a tool runs and nothing plays is a turn, not a barge-in**
   (§2.5): the user can add to a request without cancelling it.
6. **Names on the wire are the server's own** (§1.3). `mcp_list_tools.tools[].name` and
   `mcp_call.name` carry the exposed name minus its `<prefix>__` part, and `server_label` echoes
   the client's own label, as OpenAI writes them. The model sees the exposed name, unique across
   servers. `/v1/responses` writes exposed names today; aligning it is §6.
7. **Nothing is capped.** A call is bounded by its server's own `timeout_ms`, results pass
   verbatim, and an oversized conversation surfaces as `context_length_exceeded` (realtime §7.5).
   A response has exactly one model call, so `max_tool_calls` has nothing to count: the loop is
   the client's.
8. **`read_only` filters are refused, visibly** (§1.1): lmgw does not keep MCP tool annotations
   yet.

## 1. Session tools

### 1.1 Parsing

`protocol/tools.rs` gains `Tool::Mcp(McpTool)`: the fields of decision 2 plus `allowed_tools`
and `require_approval`, kept verbatim for the echo except the redacted ones. `allowed_tools` and
`require_approval` are parsed by **the code `/v1/responses` uses**, moved out of
`ingress/responses.rs` into `mcp/spec.rs` (`McpToolSpec`, `ApprovalRule`,
`parse_require_approval`). Both routes gain:
- `allowed_tools` in the object form `{tool_names: […]}`, which `@openai/agents` always sends.
  Today `/v1/responses` takes only the array form.
- A refusal of `read_only` in `allowed_tools` and in either `require_approval` filter: "lmgw does
  not read MCP tool annotations, so `read_only` cannot be honoured; list the tool names".
- Built-in tools matched by both spellings in `allowed_tools` and `require_approval`
  (`docs__query` and `query`), as registered servers' tools already are. Today a built-in matches
  only its full name (`exec.rs:155-160`, `:392-394`), so a gate written with the short name would
  silently not gate.
- An empty `tool_names` list says "allowed_tools left nothing", not "the server exposed no tools"
  (`exec.rs:236-246`).
- A tool name that is not a string, in `allowed_tools` or a `require_approval` filter, refused with
  its index: dropped, it would leave another filter than the one written (final review #8).

Realtime-only rules, all as `error` (`invalid_value`, `param` naming the entry); a refused
update changes nothing:
- a duplicate `server_label` in one `tools` array (OpenAI's rule);
- a gating `require_approval` (decision 4);
- a client function whose name equals an exposed MCP name the session has;
- an `mcp` tool on a bound session: `owned_by_thread`, as today. Listing starts only after the
  bound-session check passed, so a refused update never wakes a service agent's container.

The URL-side rules (one of `server_url` / `connector_id`, not both) are **not** enforced: nothing
reads those fields. A `{type: "mcp", server_label}` entry alone in a later `session.update`
reuses the session's earlier definition of that label, as OpenAI's guide allows.

**`response.create` tools.** A response's `tools` may name an `mcp` label the session has
already listed (it selects or narrows, by the same parser); a label the session has not listed
is an `error` ("list it in `session.update` first"). There is no per-response listing.
`@openai/agents` sets tools at session level only.

`tool_choice` gains `{type: "mcp", server_label, name?}`. With a `name`, the session's reverse
table maps `(label, name)` to the exposed name: `ToolChoice::Tool{name}`. Without one, the
response's tools are narrowed to that label's and the choice is `Required`. A label or name the
session does not have is an `error`, as an unknown function name is today. Never build an
exposed name by concatenation: servers without a prefix, renamed tools and name-addressed
labels break it (`mcp/mod.rs:451-458`).

### 1.2 Listing

When the session's tool set gains a label (or changes its `allowed_tools`), the session resolves
it in a task of its own:
1. `conversation.item.added` with an `mcp_list_tools` item `{id, type, server_label, tools: []}`;
2. `mcp_list_tools.in_progress {item_id}`;
3. resolve (below);
4. `mcp_list_tools.completed` or `.failed`, then `conversation.item.done` with the tools filled in.
   Each tool is `{name, description, input_schema, annotations: null}`. `description` is always a
   string (`""` when the server gave none), because `@openai/agents` drops the whole event over a
   null.

**Resolve connects only the named servers.** Today `resolve` always calls `state.mcp.list_tools`,
which starts a lazy connect for *every* enabled server that is not ready and waits up to
`LAZY_LIST_BUDGET` (10 s, `mcp/mod.rs:85`, `:1418-1494`) — even for a built-in label. Idle reaping
makes "not ready" the normal state. `resolve` changes, for both routes: built-in labels never call
`list_tools`, and a server label connects and lists that server only (its own lazy connect,
within the same budget). One mechanism, cheaper for `/v1/responses` too.

A failed label (unknown, disabled, out of the key's scope, server down; `resolve` says which and
lists what is available) also gets an `error` event with code `mcp_list_tools_failed` and the
resolver's message. The session stays open. *Why:* `mcp_list_tools.failed` carries only an item
id, and a voice client that cannot say why its tools vanished is the silent failure the
gateway's rules forbid.

The session keeps a tool table: per label its `ToolDef`s, and the reverse map exposed name →
`(client label, wire name)`. An unchanged definition is not listed again, except that a
`session.update` whose `tools` name a label whose last listing failed lists it again, unchanged or
not: a client keeps one socket open for hours, and the server may be up now. A `session.update` that
drops a label removes its tools; its `mcp_list_tools` item stays in the conversation, as OpenAI
keeps it. A listing an update supersedes while it runs (its label dropped or listed again) closes
its item at once, before any new item is added: `mcp_list_tools.failed` and
`conversation.item.done` with no tools, and no `error`. Its result is ignored when it comes:
`@openai/agents` replaces a label's tools on every listing it sees done, so a stale listing that
finished last would win. Listing items are never rendered to the model; the tools go as tool
definitions.

**A response waits for the listings in flight** before it renders (OpenAI does not wait). A cold
server's first listing connects it, up to the 10 s budget. *Why:* a response that silently
lacked a tool listed a moment later cannot be told apart from "the model ignored it". The wait is
the timing mark `tools_list_ms` (realtime §11), not a silent delay.

### 1.3 Names and echo

The wire name is the exposed name minus its `<prefix>__` part, from the reverse table (for a
server with no prefix the exposed name is already the upstream's). `server_label` on every item
is the label **the client wrote**, also when it addressed a server by name rather than prefix:
the SDK files tools under `cfg.server_label` (`realtimeSession.mjs:1159-1166`).

`session.updated` echoes each `mcp` tool as given, except that `authorization` becomes
`"[redacted]"` and so does each `headers` value (names stay). The response object has no `tools`
field and gains none.

**Always serialized.** `output` (string or `null`), `error` (object or `null`), `arguments`,
`name`, `server_label` and `approval_request_id` (`null`) on every `mcp_call`, on every event that
carries one (`added`, `done`, `retrieve`, `output_item.*`, `response.done`). The SDK validates
with `.parse` inside a listener with no try/catch, so a missing `output` throws there; the
codebase's `skip_serializing_if = "Option::is_none"` convention (`protocol/item.rs:47-79`) must
not reach these fields. A golden test pins it.

### 1.4 Discovery: `GET /v1/mcp/servers`

Added 2026-10-05: a client should be able to render a tool picker instead of a
free-text list of labels. OpenAI has no such route; these are lmgw extensions under `/v1`, like
`/v1/audio/voices`, and change nothing OpenAI-shaped. A client builds its picker against this
shape, so it is a contract.

`GET /v1/mcp/servers` lists the labels the caller may use, **connecting nothing**:

```json
{"object": "list", "data": [
  {"object": "mcp.server", "server_label": "docs", "kind": "builtin", "name": "docs", "description": "…"},
  {"object": "mcp.server", "server_label": "ha", "kind": "server", "name": "Home Assistant", "description": ""}
]}
```

- `kind`: `builtin` (`docs`, `kb`, and `lmgw` only where `resolve` would allow it: an owner
  credential and the `self_admin` mode), `server` (a registered MCP server), `agent` (a service
  agent's tools, a label `resolve` accepts).
- `server_label` is exactly what goes into `{type: "mcp", server_label}`: the label `resolve`
  matches first (the tool prefix, else the name).
- Only enabled servers within the caller's `ToolScope::may_reach`. A server's `description` is
  `""` (the config has no such field); a built-in's is its one-line purpose.
- A caller with a tool scope of its own is told of a server by the rule that names the available
  labels in `resolve`'s refusals: a tool of it already listed that the scope admits, or, with
  nothing of it listed, a namespace the scope can reach. A bare server (no tool prefix) has no
  namespace to read, so `may_reach` only says it is worth connecting: it is not listed to a scoped
  key until a tool of it the key admits is, and once listed, one whose every tool the scope keeps
  out is answered like a label that does not exist, on both routes and in the detail's 404
  (final review #3).

`GET /v1/mcp/servers/{label}` returns the same object plus `tools: [{name, description,
input_schema}]`, **exactly** the tools `mcp_list_tools` would carry for that label (§1.2: wire
names, `description` always a string), produced by the same `resolve` with the caller's scope and
the owner's disables, so the two can never disagree. It connects that one server (§1.2's targeted
connect, up to the 10 s budget). An unknown or out-of-scope label is a 404 whose message names the
available labels, as `resolve` words it; a server that cannot be listed is a 502 with its error.

Both take the `/v1` inference credential (`Cap::Inference`), as `/mcp` does. They write no
`request_logs` row (no model or tool call). They get a `DocRoute` on the API docs page and an
OpenAPI entry.

## 2. A call in a response

### 2.1 Offering

`render.rs` adds the session's resolved MCP tools (narrowed by the response's `tools`, §1.1) to
`ir.tools` after the client's functions, by their exposed names. The model's call to an exposed
name the response offered is **server-side**; anything else is a client function call, as today.

### 2.2 The stream

`Output` (`realtime/output.rs`) classifies at `ToolCallStart`:
- client function: unchanged;
- server-side: an `mcp_call` item `{id, type, server_label, name, arguments: "",
  approval_request_id: null, output: null, error: null}`, announced with
  `response.output_item.added` and `conversation.item.added`. The argument deltas go out as
  `response.mcp_call_arguments.delta` (with `response_id`, `item_id`, `output_index`). They carry
  no `obfuscation`, so the SDK's strict schema files them as generic events, which it ignores
  anyway; `function_call_arguments.delta` does the same today.

There is no `call_id` on an `mcp_call`. The session keeps the upstream's call id per item for
rendering, minted session-unique as for functions (realtime §7.4).

At the model's `Stop`, `response.mcp_call_arguments.done` goes out per server call, text and
function items close as today, and **the MCP items stay open** with a per-call state in `Output`:
*unmade* (announced, not started), *running*, *done*. `all_closed()` counts an open MCP item as
open, so `cancellable()` stays true while a call runs. These rules live in `output/closing.rs`;
`ending.rs` only calls them.

### 2.3 Which calls run

- A response that stopped for `Length` or `ContentFilter` runs **nothing**: its calls close with
  `UNMADE_CALL` (the SDK runs no `incomplete` function call either).
- A call whose arguments do not parse to a JSON object fails at once with a
  `tool_execution_error` naming the parse error. The model reads it next turn.
- A stream that fails after `Stop` (`ending.rs:169-183`) runs nothing; its calls close with
  `UNMADE_CALL`.
- Everything else runs.

### 2.4 Running

The calls run **inside the responder's chat future** (`responder.rs:274-291`), from the model's
`Stop`, not after the `join!` with the speaker: a slow voice does not hold the tools, and a failed
voice does not wait for them. They go through the `/v1/responses` executor stack with the session's
`RequestCtx`: `ScopedExecutor(SplitExecutor{SelfAdmin, Docs.with_client(key), Kb, builtin names,
McpExecutor})`, every layer with the proto `realtime-tool` (`ScopedExecutor` gains `with_proto`;
today it is hard-coded to `responses-tool`, `mcp/scope.rs:327-334`, and `SelfAdminExecutor`
defaults to `admin-tool`, `exec.rs:541`). The executors write their own `request_logs` rows
(`exec.rs:466-479`); nothing else records them. They run concurrently, or in model order with
`parallel_tool_calls: false`. Each is raced against the response's `StopSignal`, up to its
executor's own row (`RowWatch`): a call whose row is being written has returned, and finishes, so
every call has exactly one row (final review #5).

**A call runs on the server its label was listed from, or not at all** (final review #2). An
exposed name is unique within one aggregate only: two servers without a tool prefix that offer a
tool of one name share it, and the first connected by server name owns it. A name listed from
`zeta` while `alpha` was reaped is `alpha`'s once `alpha` connects. The table keeps each tool's
server id, and the executor calls by name and server (`McpManager::call_listed`): a name that
routes to another server now is refused with a `tool_execution_error` saying to list the label
again; a listed server that was let go since (idle-reaped) is connected again for the call. The
owner's per-tool switch and the per-call scope check apply as before. `/v1/responses` pins its
run's names the same way (one request is a shorter window, not a different one).

Per call the responder sends the core:
- `ToolRunning{item}` → `response.mcp_call.in_progress {item_id, output_index}`;
- `ToolDone{item, result}` → `response.mcp_call.completed` or `.failed`, then
  `response.output_item.done` and `conversation.item.done` with `output` (the text the
  `/v1/responses` encoder writes), or `output: null` with `error: {type: "tool_execution_error",
  message}`. A routing or transport failure is a `tool_execution_error` too: the model reads it on
  the next response and can recover.

**Order in speech mode.** In speech mode the call's own deltas reach the core through the
speaker's work queue, after the clauses before them are synthesized (`responder/speech.rs:369-375`,
`:446-448`); a fast `docs` call would otherwise be reported before its item exists. So the tool
messages go through the same queue (a `Work::Pass` of a tool message): the tool **runs** at once,
its events are **delivered** in stream order.

`Msg::Finished` follows the last call, so `end_call` and the drained ack already order
`response.done` after every call and after the audio. `response.done` carries the `mcp_call`
items with their outputs. The core keeps each call's result blocks (images included) for
rendering (§2.6); only the wire `output` is flattened text.

`realtime-tool` joins `responses-tool` and `chat-tool` in `telemetry.rs`, including
`counts_in_token_stats` (`:204-208`), or token stats and the `active` count go wrong.

### 2.5 Cancel, barge-in and speech during a tool

A cancel (`response.cancel`, a barge-in, a session close) raises the response's stop. The core
closes the open MCP items itself in `Output::abandon`, because the responder's later messages are
dropped as an old generation's (`ending.rs:57-60`): a *running* call gets `ABANDONED_CALL`, an
*unmade* one `UNMADE_CALL`, each as `output: null`, `error: {type: "tool_execution_error",
message}`, with `response.mcp_call.failed` and `conversation.item.done`. The responder writes a
`canceled` row for each call the stop dropped (a dropped future writes none).
`conversation.item.delete` of an `mcp_call` that is not *done* is refused (`conversation.rs:181-190`
checks only the `in_progress` status, which an `mcp_call` does not have).

*Pinned by WP4.* "Running" means sent: in speech mode a call's `in_progress` waits behind the
clauses before it, so the responder also tells the core directly that the call went out
(`ToolSent`), and a cancel in between closes it as abandoned, not as never made (which would invite
the model to make it again). The call's own deltas wait behind those clauses too, so `ToolSent`
carries the call's id, name and arguments: a response that ends before the call's item was
announced — a cancel, a barge-in, a voice that failed with them queued — has the core announce
the call then (its item, its arguments, their done) and close it as abandoned, so `response.done`
and the conversation always carry a call the server had (final review #1). The `canceled` row is
the executors' row with status 200 (as a stopped model call's), `error_kind: "canceled"` and the
`ABANDONED_CALL` words, naming the server the executors would name; a call the stop ended before it
was sent writes none, and one whose executor was already writing its row finishes with that row
alone (§2.4). A session close drops the stop with the core: the calls end the same way, with
nobody told.

**The follow-up after an abandoned call.** `@openai/agents` sends `response.create` on every
`mcp_call` `conversation.item.done`, so it sends one after an abandoned call too. When the barge-in
already re-carried that response's own create (`again`, `interrupt.rs:203-210`,
`pending.rs:175-190`), today's rule would refuse the SDK's new one with
`conversation_already_has_active_response` — the error `pending.rs`'s module doc says must not
happen. New rule: **a client `response.create` that arrives while the only held create is a
re-carried one is absorbed into it**, with no error: the held create answers the same turn and
renders the same conversation.

*Pinned by WP4.* It absorbs once, and only a create that asks for nothing the held one would not
give: no overrides of its own (the SDK's follow-up is a bare `{type: "response.create"}`), the held
create's own, or overrides where the held one had none (judged at once, as a held create is).
Overrides that differ are two requests, and the newer is refused as before, visibly. The held
create takes the newer `event_id`, which is the one the SDK waits on. The rule keys on the
re-carry, not on tools: a create sent while any cut nobody heard is carried is absorbed the same
way, tools or none — safe, since the held create has not rendered yet and answers the newer
request too (realtime §4.3; final review #7).

**Speech while a tool runs.** Today "producing" (the answer still counts as playing, so speech is
judged as a barge-in) is true until the phase reaches `Playing` (`interrupt.rs:89-94`). With
`Finished` held for the tools, the response stays `Closing` for the whole run: every utterance
would face the barge-in gate, `half_duplex` would not listen at all (`input.rs:13-17`), and
`PlayedOut` would never come (`interrupt.rs:182`). New: the speaker sends a **speaker-done**
message when its last clause has been handed to the writer, and "producing" derives from it and
the playback end, not from the phase. Speech while only a tool runs is then an ordinary turn
(realtime §4.3): it is committed, the response finishes with its call's result, and the client's
follow-up is held until the turn ends and answers both. A barge-in while the preamble
still plays cancels as before.

*Pinned by WP4.* A text response stops producing at its end of generation, as before; a bound
session's turn sends no speaker-done and produces until Playing, as before. Only the calls are left
when the response is Closing, no longer produces, has a call open, and nothing of it plays at the
speech's capture: it never spoke, or its audio played out by the modelled end with none of it
waiting in the writer. Such a turn cuts nothing and keeps the plain silence window (realtime §6.5:
nothing the user listened to is interrupted).

**Speech never cancels a silent call** (final review #9). A turn spoken while only a call runs is a
turn, not a barge-in, so a voice-only client has no spoken way out of a long or hung call: a
built-in toolset's call has no `timeout_ms`, and a registered server's is its own. The call ends
when its server answers or its timeout fires; `response.cancel` still ends it at once, as above.

**A turn that ends before the call does** is owed its automatic response, which starts right at
the call's `response.done` and renders its result. The SDK's follow-up, sent on the call's
`conversation.item.done`, meets that response generating and is refused once, as in realtime §6.4's
"an answer can end while a check is in flight". *Decided (final review #10): the refusal
stays* — absorbing a create into a response already running could swallow a deliberate second one,
as §6.4 reasons. Its words say what happened instead: when the running response is the automatic
one that started first after the calls' results were in, the `conversation_already_has_active_response`
message says it already answers with the results of the session's last MCP calls
(`lifecycle/refusal.rs`). Any other second create keeps the plain message.

### 2.6 Rendering back

An `mcp_call` renders as an assistant tool call (its exposed name from the reverse table, its
arguments) followed directly by its tool result: the kept result blocks, or the error message
marked as an error. Consecutive calls from one response share one assistant turn, as function
calls do. A call with neither output nor error (only a client's replayed item can be one) gets the
synthetic `"(no result yet)"`, as a function call does (`render.rs:437`). `mcp_list_tools` items
are never rendered. A call renders even if its label has since left the session: it is history;
a label the session never had renders by `<label>__<name>` as a last resort.

`mcp_call` items a client creates (history it replays) are accepted with `output`/`error` and
render as above. `mcp_list_tools`, `mcp_approval_request` and `mcp_approval_response` items from a
client are refused with `invalid_value` naming the type (the last two until approvals exist).

## 3. Mixed turns

A model turn may call a client function and a server tool together. The function item completes
at `Stop` as today; the server call runs; `response.done` comes after both. `@openai/agents` runs
the function at its `output_item.done` and sends its output and `response.create`; its MCP
follow-up comes with the call's `conversation.item.done`. Both arrive while the response still
runs, so the SDK holds them and sends one `response.create` after `response.done`. A client
that sends them anyway gets the existing queue (Closing phase) or
`conversation_already_has_active_response` (realtime §4.3).

## 4. Telemetry and surfaces

- `realtime-tool` rows, one per call, in Usage's tool class with the existing columns.
- The per-response timing line gains `tools_list_ms`, `tools_ms` and `tool_calls`, and names a
  first server-side call `first MCP call`, a client's `first function call`.
- `session.lmgw.resolved` is not extended: the `mcp_list_tools` items are the resolution.
- Realtime §4.3 (a cancel after generation, producing), §7.4 and §19 point here. The OpenAPI
  realtime text drops its "Refused … server-side (MCP) tools" clause
  (`openapi/planes/inference.rs:413-422`), and the API docs page's realtime entry lists the new
  events and item types.
- No setting, no UI. Bound sessions and the dashboard's voice mode are unchanged.

## 5. Tests and live checks

Integration tests (`tests/it/realtime_mcp*.rs`), scripted with `chat_fake` turns and a fake MCP
server (the `mcp_ingress`/`mcp_live` tests have one). WP1 rewrites the pinned refusals in
`tests/it/realtime_protocol.rs:80-88, 498, 677-681`.
- parser: both `allowed_tools` forms on both routes, short and full built-in names, `read_only`
  and gating refusals, a duplicate label, a function/MCP name clash, label-only reuse, redaction,
  `tool_choice` with and without `name`, `response.create` naming an unlisted label;
- listing: item and event order, a failed label's `error`, an out-of-scope label on a client key,
  `lmgw` refused to a non-owner, only the named server connects, a response waiting for a listing;
- a text and a spoken round trip as `@openai/agents` sends it (golden: `output_item.added` →
  arguments → `response.mcp_call.in_progress` → `.completed` → `output_item.done` →
  `conversation.item.done` → `response.done`, every `mcp_call` with `output` present), then the
  follow-up renders call + result; a fast call in speech mode still reports after its item;
- a mixed function + MCP turn yielding one follow-up; `parallel_tool_calls: false` ordering;
  `Length`, bad arguments and a post-`Stop` failure running nothing;
- cancel, barge-in and session close during a running call; the absorbed follow-up after a
  barge-in on a re-carried response; speech during a silent tool run is a turn, answered once
  with the result;
- the rows (`realtime-tool`, `canceled` for dropped calls) and token stats unchanged.

Live check on a dev instance (`scripts/dev-instance.sh`; never the real data dir): the pinned
`@openai/agents` 0.18.0 (`target/realtime-live/nodework`) with a `hostedMcpTool` on the `docs`
built-in or a throwaway stdio MCP server, in text mode and in audio mode with a local chat model,
TTS-generated speech only: the SDK lists the tools (`mcp_tools_changed`) and sends exactly one
follow-up per tool turn. Then the first client runs against it.

*Done 2026-10-05 (WP5)*, on a dev copy with a throwaway Podman stdio server (`--network=none`,
one tool) and the `docs` built-in, chat `qwen3.5-2b` local, parakeet ASR and supertonic TTS on
the CPU. Text: `mcp_tools_changed` with the wire names, two tool turns in one session gave four
`response.create` (two typed, one follow-up per tool turn), answers quoted the tool's result, no
`error` and no event the SDK's schema rejected. Audio, from a TTS-generated question: a response
carrying a spoken preamble and the `mcp_call` reported the call after the preamble's clause, sent
`response.done` after the preamble played, and got one follow-up whose spoken answer used the
result. One `realtime-tool` row per call. Barge-in and cancel during a call were left to the
integration tests.

## 6. Later

- **Approvals**: `mcp_approval_request` / `mcp_approval_response`, with the review's rules —
  gated calls handed to the session at `Stop` (outside `all_closed`, `close_items` and `abandon`),
  a turn's ungated siblings held until its approvals are decided (`/v1/responses`' rule,
  `agent.rs:100-106`), `require_approval` judged at offer time, approval `arguments` always a JSON
  object.
- **`session.lmgw.answer_after_tools`**: the gateway sends the follow-up itself, for clients that
  never send one. It must stay off for `@openai/agents` (a double answer).
- **A server-side default toolset** for sessions that send no `tools`.
- **`read_only`**: keep MCP tool annotations (`readOnlyHint`) in the aggregate, on both routes.
- **`/v1/responses` names**: wire names on its items, as here (decision 6).
- **Spoken filler** while a tool runs (realtime §19).

## 7. Work packages (build order)

| WP | Scope | Tests |
|---|---|---|
| 1 | `mcp/spec.rs` shared parser (object `allowed_tools`, both built-in spellings, `read_only` refusal, honest empty-list message) for both routes; `resolve` connects only named servers; `Tool::Mcp`, `tool_choice` mcp, `mcp_call`/`mcp_list_tools` items (always-serialized fields), the events, redacted echo, label-only reuse, the realtime refusals | parser, protocol, `/v1/responses` regressions |
| 2 | Listing task, the session's tool table and reverse map, the response's wait, `mcp_list_tools_failed`; `response.create` label rules and label-only reuse there; the function/MCP name clash; `GET /v1/mcp/servers[/{label}]` (§1.4) | listing, discovery |
| 3 | Offering, `Output` classification and per-call state, which calls run, the run in the chat future with `ScopedExecutor` and `realtime-tool`, speech-mode ordering, rendering back | round trips, mixed, golden |
| 4 | Cancel and abandon in the core, the absorbed follow-up, speaker-done and "producing", item-delete refusal, `canceled` rows | cancel, barge-in, speech during a tool |
| 5 | Realtime spec pointers, OpenAPI text, API docs page; live check with `@openai/agents` | `ci/check.sh`, live |

## Appendix: review finding → resolution (2026-10-05)

| # | Finding | Resolution |
|---|---|---|
| 1 | Speech mode: tool messages overtake their call's deltas | Tool messages through the speaker's queue; calls run in the chat future (§2.4) |
| 2 | `Length` / bad arguments / post-`Stop` failure would still run calls | Run nothing; `UNMADE_CALL` or a parse error (§2.3) |
| 3 | No owner for calls outliving the response; abandon in the core | Approvals deferred (decision 4); per-call state and core-side abandon (§2.2, §2.5) |
| 4 | SDK follow-up after an abandoned call refused on a re-carried response | Absorbed into the held create (§2.5) |
| 5 | Barge-in window open for the whole tool run; half duplex deaf | Speaker-done drives "producing"; speech during a tool is a turn (§2.5) |
| 6 | `resolve` connects every server, up to 10 s | Connect only the named servers, both routes (§1.2) |
| 7 | Built-in short names bypass gates; label/prefix echo; concatenated names | Both spellings; client's label echoed; reverse table; name clash refused (§1.1, §1.3) |
| 8 | Tightened `require_approval` not re-evaluated | Moot until approvals; noted for them (§6) |
| 9 | Pending-call rendering claim wrong; gated siblings answered early | Synthetic result as for functions (§2.6); sibling rule noted for approvals (§6) |
| 10 | `output` must never be skipped | Always serialized, golden-pinned (§1.3) |
| 11 | Double rows, `ScopedExecutor` proto, token stats, dropped-call rows, `docs` client | §2.4, §2.5 |
| 12 | Image results flattened, `obfuscation`, empty list message, delete, bound-check order, pinned tests, OpenAPI | §2.4, §2.2, §1.1, §2.5, §5, §4 |
| 13 | Cuts: response `tools` echo, approvals, per-response listing | All three taken (§1.1, §1.3, decision 4) |

## Appendix: final review → resolution (2026-10-05)

An adversarial review of the built branch, each finding checked against the code before it was
fixed.

| # | Finding | Resolution |
|---|---|---|
| 1 | Speech mode: a call sent before its item was announced vanished on a cancel, barge-in or voice failure | `ToolSent` carries id, name and arguments; the core announces such a call at the end and closes it abandoned (§2.5) |
| 2 | Calls routed by the current aggregate: a bare sibling that connected later took a listed name; `clash_of` said "same server" | The table keeps server ids; `McpManager::call_listed` refuses a name another server took and reconnects a reaped listed server; `/v1/responses` pinned too; `clash_of` compares ids (§2.4) |
| 3 | Discovery listed every bare server to every scoped key; its detail was a 502, not a 404 | One rule (`shown_to`) for the list, `resolve` and the 404 (§1.4) |
| 4 | A superseded listing's late result reached the client, and the SDK let it win | Closed at once when superseded (`.failed`, no `error`); its result ignored (§1.2) |
| 5 | A `canceled` row beside the executor's own when the stop came during its row write | The stop races a call only up to its executor's row (`RowWatch`): one row per call (§2.4, §2.5) |
| 6 | `cancel.rs` said a cancel after generation changes nothing | Module doc: an open `mcp_call` keeps the response cancellable |
| 7 | The absorb rule applies to responses without tools too | Kept; said in realtime §4.3 and §2.5, pinned by a no-tool test |
| 8 | Non-string `allowed_tools` entries dropped silently | Refused with the entry's index, both routes (§1.1) |
| 9 | Speech never cancels a silent call: no voice escape from a hung one | Said in §2.5; `response.cancel` still ends it |
| 10 | Turn ends before the call: the follow-up is refused into a running automatic response | Refusal kept; its message says the running response answers with the tool results (§2.5) |
| 11 | Timing line said "first function call" for an MCP call | "first MCP call" (§4) |
