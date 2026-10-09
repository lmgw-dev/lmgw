# MCP Tasks for hosted servers, and late tool results

**Draft v1, 2026-10-09; built the same day (WP1–WP4) and accepted (WP5, the live checks of §7).**
The sections below are the design as drafted; where the build went another way they carry an
*Amended* note, and the "As built" notes at the end of §8 hold the detail. Paths are relative to
`crates/lmgw-core/src` unless they name a crate, and line numbers are at `main` a35b82af (the
draft's base). Companion
records are cited by short name: "client-apps §n" (2026-10-06), "server-tools §n" and "server-tools
decision n" (2026-10-05), "chat-voice §n" (2026-10-03), "realtime §n" (2026-10-01), "mcp-gateway
§n" (2026-06-29). The desktop client's own design record is "the client's record" (its K26, ruling
22, §15.1.2 M4 and WP18).

**Today:**
- A southbound `tools/call` is answered in the call: the Chat's tool loop waits for it, bounded by
  the row's `timeout_ms` (`mcp/host/calls.rs`, `mcp/listed_call.rs`), and a turn cannot end while
  a call runs. A tool whose work takes minutes either times out or holds the turn (and a bound
  voice session's response) for its whole length.
- lmgw's client role speaks MCP **2025-11-25**: rmcp 2.0's `ProtocolVersion::LATEST`, sent in
  every `initialize`, the host link's included (`mcp/handler.rs:105-118`, `mcp/host/link.rs`).
  rmcp 2.0 already models that revision's Tasks: `CallToolRequestParams::with_task`,
  `ServerResult::CreateTaskResult`, `tasks/get` / `tasks/result` / `tasks/cancel` requests,
  `ClientHandler::on_task_status`, `Tool::task_support()`, `ServerCapabilities.tasks`
  (`rmcp-2.0.0/src/model/task.rs`, `model.rs:3314-3465`, `model/tool.rs:45-96`). lmgw reads none
  of it: no code reads `execution.taskSupport`, and `/mcp` passes each tool's JSON through
  verbatim, `execution` included (`mcp/ingress.rs:247-256`).
- A stored tool turn is replayed from its record, `chat_messages.ir_messages`: the assistant's
  calls and their results, then the final text (`web/chat_turn.rs:1181-1210`). Adjacent user
  messages merge for strict templates (`web/chat_turn/merge.rs`); every call in a record has its
  result, an unmade one included (`agent::close_trailing_calls`, `web/agentchat.rs:938`).
- One live turn per thread and a generation per thread guard every history write
  (`web/chat_live.rs:1-45`): a write that moves the generation while a turn runs makes that turn's
  reply unsavable.
- A bound session's `response.create` that answers no owed turn and has no new words is refused
  `empty_turn` (`realtime/lifecycle/bound.rs:150-200`). An unbound session renders a client
  function call with no output yet as `"(no result yet)"` (`realtime/render.rs:74`).

## The protocol, read 2026-10-09

**Verdict:** the stable Tasks are the 2026-07-28 revision's, and lmgw does not speak that revision
yet (its rmcp 2.0.0 cannot; rmcp 3.5.1 can, see below); the revision lmgw speaks has Tasks as an
experimental, now frozen feature. This design pins
**2025-11-25's Tasks** (T1) and keeps everything above the wire revision-neutral (T2, §1.9).

- **2025-11-25** (`/specification/2025-11-25/basic/utilities/tasks`): Tasks are a core utility,
  marked "currently considered **experimental**" (SEP-1686). Requestor-driven: the receiver
  declares `capabilities.tasks.requests.tools.call` in its `initialize` result, each tool declares
  `execution.taskSupport` (`forbidden` default, `optional`, `required`), and the requestor adds
  `task: {ttl?}` to `tools/call`. The receiver answers `CreateTaskResult {task}` at once; the
  requestor polls `tasks/get` (respecting `pollInterval`), fetches the payload with `tasks/result`
  (blocking until terminal), cancels with `tasks/cancel`. `notifications/tasks/status` is
  optional and "requestors **MUST NOT** rely on" it. Statuses `working`, `input_required`,
  `completed`, `failed` (a JSON-RPC error, or a tool result with `isError`), `cancelled`; the last
  three are terminal. An unknown or expired task id is `-32602`. A note invites hosts to "return
  control to the model while the task is executing", with an optional
  `_meta["io.modelcontextprotocol/model-immediate-response"]` text.
- **2026-07-28** (the GA revision): Tasks leave the core and become the official extension
  `io.modelcontextprotocol/tasks` (SEP-2663, status *Final*, Extensions Track). It is **not
  wire-compatible** with 2025-11-25: no `task` parameter (the server alone decides, per request,
  to answer `resultType: "task"`), no `execution.taskSupport` on tools, no `tasks/result`
  (`tasks/get` carries `result` or `error` inline), no `tasks/list`, a new `tasks/update` for
  input, `notifications/tasks` only on a `subscriptions/listen` stream naming `taskIds`, and
  `ttlMs` / `pollIntervalMs`. The extension is negotiated per request in
  `_meta["io.modelcontextprotocol/clientCapabilities"]`, because the same revision **removes
  `initialize`** (SEP-2575: stateless, `server/discover`, version and capabilities on every
  request).
- **SEP-2663's own compatibility table:** under 2025-11-25 "the experimental feature from that
  release applies", and the extension "is not defined under the `2025-11-25` protocol version".
- **rmcp:** 2.0.0 (lmgw's) and 2.2.0 both keep `LATEST` at 2025-11-25, model only the SEP-1686
  shapes, and have no `server/discover`, `subscriptions/listen`, `resultType` or `tasks/update`.
  *Verified 2026-10-09 by reading the crate source:* **rmcp 3.5.1** (newest on crates.io) has
  `ProtocolVersion::LATEST` = 2026-07-28 and keeps `LATEST_WITH_INITIALIZE` = 2025-11-25 for peers
  that need `initialize`. It implements the `io.modelcontextprotocol/tasks` extension
  (`resultType: "task"`, `tasks/update`, a task manager), `server/discover` and
  `subscriptions/listen`, and has a WebSocket transport. lmgw is still on rmcp 2.0.0.

So the client's record's pin ("the 2026-07-28 spec's Tasks, if that revision is stable") cannot
hold as written for lmgw today: its Tasks are stable, but they need the stateless revision, which
the host link (built on `initialize`, client-apps §5.1) does not speak and lmgw's rmcp 2.0.0 does
not offer. rmcp no longer blocks the move (3.5.1 speaks it); what remains is the rmcp 2 → 3
upgrade (lmgw and Kai), a stateless redesign of the host link, and speaking both revisions while
third-party servers stay on 2025-11-25. Moving lmgw's whole southbound side to 2026-07-28 is its
own change (Later). No lmgw-only dialect is needed either:
the revision both ends negotiate defines Tasks, and a namespaced copy would be lmgw-only where the
protocol already has the feature (Rejected).

## Decisions

*From the client's design record:*
1. **K26:** sync or async is decided per tool class at design time, not per call; the open-ended
   class returns "started, <id>" at once. lmgw needs Tasks support as the MCP client of hosted
   servers (useful for every registered server), and its realtime route keeps the conversation
   going while a call is pending. Delivery: a late result enters the thread like any tool result;
   the client picks when to deliver it, never while the user talks or the model speaks. Control:
   `jobs` and `job_cancel {id}` tools, so nothing runs unseen.
2. **Ruling 22:** the client may speak an async result on its own, opening a voice session without
   a key press. Each job has an `announce` flag (the client's concern).
3. **M4's contract** (§15.1.2), for this design to confirm or change: (1) one revision pinned, a
   task-augmented `tools/call` for a tool whose `execution.taskSupport` asks for one; (2) the
   turn's tool result is at once `started, job <task id>` and the turn goes on; (3) lmgw follows
   the task (notifications, else `tasks/get` at a visible interval) across turns and with no
   session bound, and "a link that drops ends the task with the abandoned wording"; (4) the result
   "enters the thread as a tool result the next turn renders (valid for strict templates)",
   recorded as `job.done {thread_id, label, task_id, status}` and sent to a bound session as
   `lmgw.job.done`; (5) "lmgw starts no response for it"; a bound session's `response.create`
   with no new user item runs a continuation turn; the dashboard's next turn sees it; a task
   cancelled in lmgw (thread deleted, the owner's cancel) sends `tasks/cancel`.
4. **Standing rules:** OpenAI- and MCP-shaped surfaces copy their protocol, lmgw-only behaviour is
   opt-in and namespaced; no hidden limits (client-apps decision 12); configured fallbacks are
   always used; every SQLite write through `begin_write`; new logic in new child modules; every
   new route, op or `x-lmgw` header gets its API-docs entry.

*Taken in this draft (open to the owner's veto), each with its reason:*
- **T1. Tasks of MCP 2025-11-25, on every southbound link** (device rows and registered servers
  alike). *Why:* it is the revision lmgw negotiates and its rmcp 2.0.0 implements (the move to 2026-07-28 needs
  the rmcp 2 → 3 upgrade and a host-link redesign, Later); SEP-2663 itself says the
  experimental feature is the one that applies under it; its text no longer changes.
- **T2. Revision-neutral above one wire module.** Only `mcp/tasks/wire.rs` knows 2025-11-25's
  shapes; the table, the follower, delivery, feed and events speak lmgw's own `TaskState`. §1.9
  maps every wire fact to 2026-07-28's. *Why:* the move to the stateless revision then changes one
  module, not the feature.
- **T3. Only `required` tools are called as tasks.** `optional` and absent are called normally.
  *Why:* a normal call answers inside the turn; `required` is the class the server marks
  open-ended (K26's class per tool), and the only value where the spec refuses a normal call.
- **T4. A late result needs a stored Chat thread.** Chat turns (dashboard, devices, bound voice
  sessions) get the late path. Every other caller (`/mcp`, `/v1/responses`, unbound realtime,
  agent runs, temporary threads) gets **the bridge**: lmgw makes the task-augmented call, follows
  the task and answers the caller with its result, within the row's `timeout_ms`, and sends
  `tasks/cancel` when it stops waiting. *Why:* only a stored thread has a place a result can land
  later; the OpenAI-shaped routes stay synchronous, as their protocols are.
- **T5. The immediate tool result is `started, job <task id>`,** plus the server's
  `model-immediate-response` text on the next line when it gives one. *Why:* the contract's text;
  the spec's note invites the server's own sentence.
- **T6. A task belongs to its thread,** not to the turn or message that started it. *Why:* the
  work happened; an edit or regenerate of the turn does not undo it (T13).
- **T7. lmgw ends a task only on the receiver's word**: a terminal status, `-32602` for its id
  (abandoned), or the server row removed. Never by its own clock, never on a link drop; a device
  that reconnects is polled again. *Why:* tasks are durable by design and bound to the key's
  authorization, not to a link; ending them at a Wi-Fi blip or a lmgw restart throws away a
  running job. **Changes contract (3).**
- **T8. Poll interval:** the task's own `pollInterval` when the receiver gives one, else
  `mcp.task_poll_interval_s` (Settings → MCP, default 5). A status notification acts at once;
  polling continues beside it. *Why:* the spec's "SHOULD respect `pollInterval`" and "MUST NOT
  rely on" notifications; the setting is the visible fallback.
- **T9. No caps.** Open tasks are unlimited (counted on the MCP page and the thread); lmgw asks no
  `ttl` (the receiver states its own, shown); a waiting result is kept until delivered, an owed
  cancel until sent. *Why:* the receiver knows its work and enforces its own limits (the spec
  asks it to), and its refusal reaches the model as the call's error. *Decided by the owner,
  2026-10-09:* a task with no `ttl`, or one held in `input_required`, is followed until its
  receiver ends it or its row is removed (kept as drafted).
- **T10. A finished result is a message row of role `tool`** with a `task` column, written only
  while no turn of its thread runs. *Why:* a write during a turn moves the generation and would
  make that turn's reply unsavable, or slip the result in front of a reply that never saw it.
- **T11. The result renders as a synthetic call and its result, in chronological order:** where
  its row is stored among the thread's messages, the call joined to a directly preceding
  assistant message, whatever was stored after the row following it (§3.2). *Decided by the
  owner, 2026-10-09:* chronological order. What the OpenAI, Anthropic and Gemini APIs accept is
  the rule; a local chat template that refuses it gets fixed, or the model is not the one for the
  task, and lmgw builds no template workarounds. *Why:* the draft placed the pair immediately before the first reply stored after
  the row, or at the end, so that a user message never followed a tool result (a rule of some
  strict local templates); a message sent after an unanswered result then went out as `… A1 U2
  [call R] R`, and the model answered the job instead of the user (the client's WP18, WP5's
  check 1). In stored order the request is `… [A1 + call R] R U2`: the model answers the user,
  the result before it as context. A row's place never changes once stored, so every later
  turn replays the same bytes. *Built 2026-10-09.*
- **T12. lmgw starts no turn.** A continuation is asked for: a bound session's `response.create`
  with no new words, or `POST /chat/api/threads/{id}/answer` (the dashboard's Answer button, a
  phone). *Why:* contract (5) and K26 (the client picks the moment); the route is the same
  continuation for a client without a voice session.
- **T13. History edits leave result rows standing.** An edit or regenerate truncates replies, not
  results; the owner may delete a result row by hand. *Why:* T6; a regenerated reply then answers
  the same result again.
- **T14. Wire names `task.*`, not `job.*`:** feed `task.started` and `task.done`, realtime
  `lmgw.task.done`, the label field `server_label` (as `approval.*` has it). The model-facing text
  keeps "job". *Why:* they are MCP Tasks, and lmgw already has background "jobs" (downloads,
  builds; `store/jobs_table.rs`) that these are not. **Changes contract (4)'s names.**
- **T15. A task-augmented device call carries `_meta["lmgw/task"]`:** `{delivery: "thread" |
  "wait", thread_id}`. *Why:* a device deciding whether to announce a result (ruling 22) needs to
  know whether it lands in a thread later or is awaited now; lmgw states facts (client-apps L12).
  *Amended (WP2):* `thread_id` is `null` for Admin Chat and for any thread that carries lmgw's
  admin tools (whether such a thread is in the device's reach depends on its level, which the
  call does not judge).
- **T16. Cancel from both sides.** The owner or a device that reaches the thread cancels through a
  route; deleting the thread (by hand or by the sweep) cancels its open tasks; a cancel the
  device cannot receive now is owed and sent at its next link. The device's own cancel arrives as
  status `cancelled`.
- **T17. A server with open tasks is never reaped.** *Why:* reaping a stdio server kills its
  process and every task in it; this is the in-flight guard's rule extended (`mcp/mod.rs:1178`).
- **T18. `/mcp` lists tools without `execution`.** *Why:* `/mcp` declares no `tasks` capability and
  bridges every call (T4); a client that saw `required` would, by the spec, refuse to call it.
- **T19. `input_required` is followed as the spec says:** one `tasks/result` held open, polling
  beside it; what the server then asks is answered as lmgw answers it today (sampling where the
  row allows it, elicitation refused, no capability declared). The thread shows the status
  message. *Why:* spec-true and small; elicitation is client-apps Later.
- **T20. Each task writes two request rows:** the call as today (its text `started, job …`), and
  one at its end under the starting principal (status, duration). *Why:* the spec asks requestors
  to log task lifecycle; Logs then show what ran for how long.

## Changes to the client's contract (M4)

| Point | Contract | This design | Why |
|---|---|---|---|
| (1) | the 2026-07-28 Tasks; "the host link's `initialize` offers it" | 2025-11-25 Tasks (T1). The device declares `capabilities.tasks {cancel: {}, requests: {tools: {call: {}}}}` in its `initialize` result and `execution.taskSupport: "required"` on its async class; lmgw's `initialize` declares nothing (a requestor declares nothing for task-augmented `tools/call` in 2025-11-25) | 2026-07-28 removes `initialize`; SEP-2663 undefined under 2025-11-25 |
| (2) | `started, job <task id>` | kept, plus the server's `model-immediate-response` line (T5) | — |
| (3) | a link drop ends the task, abandoned | a link drop ends nothing; the device is polled again on its next link; abandoned when the device answers `-32602` (it restarted) or its row is removed (T7) | durable tasks; the client keeps its job table across reconnects |
| (4) | `job.done {thread_id, label, task_id, status}`, `lmgw.job.done` | `task.done {thread_id, message_id, task_id, server_label, tool, status, by}`, `lmgw.task.done` (same fields); plus `task.started`; status `completed`, `failed`, `cancelled` or `abandoned` (T14) | lmgw's "jobs" are another thing; `server_label` as in `approval.*` |
| (5) | continuation on `response.create`; the dashboard's next turn | kept; plus `POST …/answer` (T12) | a text client can ask too |
| new | — | `_meta["lmgw/task"]` on task-augmented calls (T15); callers without a thread are bridged (T4) | announce needs it; `/mcp` callers get a result |

Accepted by the owner 2026-10-09: points (1) to (5) and the `_meta["lmgw/task"]` / bridge row, as built.

## 1. Protocol

### 1.1 Which calls become tasks

A call is task-augmented when all of these hold, read from the server's live session:
- the server's `initialize` result declares `capabilities.tasks.requests.tools.call`;
- the tool's `execution.taskSupport` is `"required"` (`Tool::task_support()`);
- the protocol negotiated is 2025-11-25 (an older one has no Tasks).

A `required` tool on a server that lacks the capability or an older revision is not callable; the
call's error says which ("server 'x' requires a task for 'y' but does not declare
`tasks.requests.tools.call`"). Everything else is a normal call, as today.

### 1.2 The call (late path)

- `tools/call` with `task: {}` (no `ttl`, T9) and, on a device link, `_meta` as client-apps §5.5
  plus `"lmgw/task": {"delivery": "thread", "thread_id": 812}`. `thread_id` is `null` when the
  thread is Admin Chat for that device (client-apps L3: the id is not the device's to know).
- `lmgw/timeout_ms` bounds the wait for the `CreateTaskResult` only. A timeout there is a normal
  call's timeout (client-apps §5.4: `notifications/cancelled`, then the error).
- The answer `CreateTaskResult {task}`:
  1. one `begin_write` transaction inserts the `mcp_tasks` row (§2.1) and records the feed's
     `task.started`;
  2. the call returns the text of T5 as its tool result, and the loop goes on;
  3. the tool frame of that result carries `task: {id, task_id, server_label}` (§4.3).
- A normal `CallToolResult` to a task-augmented call (a server that ignored `task`) is a normal
  result. A `CreateTaskResult` to a call lmgw did not augment is a protocol error, reported as the
  call's error.

### 1.3 Following a task

The follower (`mcp/tasks/follow.rs`, one task in `McpManager`) holds every row in state `open`:
- **Notifications.** `GatewayClientHandler::on_task_status` hands each `notifications/tasks/status`
  to the follower, keyed by the session's server and `taskId`. A terminal status fetches the
  result at once (§1.4); any other updates the row's `status`, `status_message` and
  `poll_interval_ms`.
- **Polling.** `tasks/get` every `pollInterval` (the task's own), else every
  `mcp.task_poll_interval_s`, and only while the server is connected. A device that links again
  is polled at once for all its open tasks; a registered server is connected by the lazy path as
  any call would be. Polls stop when the row leaves `open`.
- **`input_required`** (T19): one `tasks/result` held open, polling beside it; its answer is the
  result.
- **Status writes** are each one `begin_write` transaction on the row; the network call is never
  inside it (`store.rs:85-96`).
- **At start** the follower loads the open rows and owed cancels (§1.5) and resumes. No turn
  survives a restart, so every result that was waiting is deliverable at once (§3.1).

### 1.4 The result

- On a terminal status lmgw sends `tasks/result` (it returns at once for a terminal task). rmcp
  answers it as `CustomResult`; `wire.rs` parses it as `CallToolResult` (rmcp's note,
  `task.rs:197-215`).
- What the row stores (`result`, as the tool result the model will see):

| Receiver says | `status` | Result text, after the line `job <task id> (<tool>) <status>` |
|---|---|---|
| `completed` | `completed` | the `CallToolResult`'s content, images included |
| `failed` with a result (`isError`) | `failed` | its content |
| `failed` with a JSON-RPC error | `failed` | the error's message, then `statusMessage` |
| `cancelled` | `cancelled` | "cancelled on the server" or "cancelled by <who>" (§1.5) |
| `-32602` to `tasks/get` or `tasks/result` | `abandoned` | "server '<name>' no longer knows the job: it was abandoned; it may or may not have finished" (server-tools decision 5's wording) |
| the server row removed | `abandoned` | "server '<name>' was removed (<why>): the job was abandoned; it may or may not have finished" |

- The content is what the model is given, and the result's `structuredContent` only when the
  content is empty (client-apps design §7.6, 2026-10-09), as for every call. The row keeps the
  structured content beside the blocks (`{blocks, structured_content}`, `mcp::tasks::stored`; a
  bare block array when there is none, as rows of earlier builds are), and the delivered result
  row's `task` carries it as `structured_content` for the thread's readers.
- One `begin_write` transaction moves the row to `ended` with that result. Delivery is §3.1.
- The end writes the second request row (T20).

### 1.5 Cancel

**From lmgw** (T16):
- **The owner's or a device's cancel**, `POST /chat/api/threads/{id}/tasks/{task}/cancel` (§5.1):
  - linked: `tasks/cancel`. Its answer `cancelled` ends the row `cancelled` "by <who>"; a
    `-32602` "already terminal" means the work finished first, and the real result is fetched
    and delivered (the answer says so);
  - not linked: the row ends `cancelled` at once ("cancelled by <who>; the server was not
    connected, and is told when it is") and an owed cancel stays (`state = 'cancel_owed'`,
    `thread_id` cleared).
- **The thread deleted** (by hand, by a folder's delete, by the sweep's purge): in the delete's own
  transaction, every `open` row of the thread becomes `cancel_owed` and every `ended` row is
  deleted (`store::mcp_tasks::thread_gone`). The follower sends the owed cancels.
- **An owed cancel** is sent at the server's next connection and then deleted, whatever it
  answers.
- **`notifications/cancelled` is never used for a task** (the spec's rule); it stays the cancel of
  an in-flight request (client-apps §5.4).

**From the server:** a status `cancelled` (the client's `job_cancel`) ends the row as in §1.4.

**A turn's own cancel or barge-in** before the `CreateTaskResult` cancels the request as today;
after it, the task is a job and runs on.

### 1.6 Link drops, restarts, removed rows

- **A device's link drops, is taken over, or lmgw restarts:** nothing ends (T7). The row waits;
  the thread shows "waiting for device '<name>' (last seen …)".
- **A disabled device key** closes the link (client-apps §1.6); its tasks wait, since the key can
  be enabled again. A deleted key, or a cleared grant, removes the row, which ends its tasks.
- **A registered server's row removed or its prefix changed:** removal ends its tasks
  (`abandoned`, §1.4) in the removal's own transaction (`store::mcp_tasks::server_gone`); a
  rename keeps them (the row's `server_label` is the label at the call).
- **A stdio server restarted** (container recreated, lmgw restarted) answers `-32602`: abandoned.

### 1.7 The bridge (T4)

For a call with no stored thread: the same task-augmented call, then the follower's loop for that
one task, inline. The caller is answered with the result as a normal `CallToolResult` (a JSON-RPC
error stays an error). At the row's `timeout_ms` from the call, or when the caller stops waiting,
lmgw sends `tasks/cancel` and reports the timeout naming the setting, as a normal call's. Nothing
is stored and no feed record is written; the request rows are T20's. On a device link,
`_meta["lmgw/task"]` is `{"delivery": "wait", "thread_id": null}`.

### 1.8 `/mcp` and discovery

`/mcp` lists every tool without its `execution` member (T18), and `/v1/mcp/servers` shows none
either. A `/mcp` client's call of a `required` tool is bridged. `/mcp` keeps declaring no `tasks`
capability; serving Tasks on `/mcp` is Later.

### 1.9 The move to 2026-07-28 (Later), mapped

| 2025-11-25 (this design) | 2026-07-28 + `io.modelcontextprotocol/tasks` |
|---|---|
| receiver's `capabilities.tasks.requests.tools.call` in `initialize` | the extension in `server/discover`'s capabilities; the client declares it on each request |
| `execution.taskSupport: required` decides | gone: the server decides per call; every caller handles a task answer (the bridge already does) |
| `task: {ttl}` on `tools/call` | none (ignored if sent) |
| `CreateTaskResult {task}` | `resultType: "task"` and the `Task` fields flat |
| `tasks/get` → `Task` | `tasks/get` → `DetailedTask`, with `result` or `error` when terminal |
| `tasks/result` | removed (`-32601`) |
| `notifications/tasks/status` | `notifications/tasks` on `subscriptions/listen {taskIds}` |
| `ttl`, `pollInterval` | `ttlMs`, `pollIntervalMs` |
| `failed` includes `isError` results | `failed` only for JSON-RPC errors; `isError` is `completed` |
| `input_required` via `tasks/result` | `inputRequests` in the task, answered by `tasks/update` |
| `tasks/cancel` → the cancelled `Task` | empty acknowledgement, eventually consistent |

lmgw's `status` values and every surface above the wire stay as they are.

## 2. Storage

### 2.1 `mcp_tasks` (`migrations/0073_mcp_tasks.sql`; the next free number at build time)

```sql
CREATE TABLE mcp_tasks (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id        INTEGER NOT NULL,  -- mcp_servers.id; no FK: server_gone ends its rows first
  server_label     TEXT NOT NULL,     -- the label the tool was offered under, at the call
  task_id          TEXT NOT NULL,     -- the receiver's id
  thread_id        INTEGER NULL REFERENCES chat_threads(id) ON DELETE SET NULL,
  tool             TEXT NOT NULL,     -- the exposed name the model called
  call_id          TEXT NOT NULL,     -- the model's id of the call that started it
  started_by       TEXT NULL,         -- the starting principal's key name (feed `by`, request row)
  state            TEXT NOT NULL,     -- 'open' | 'ended' | 'cancel_owed'
  status           TEXT NOT NULL,     -- working | input_required | completed | failed | cancelled | abandoned
  status_message   TEXT NULL,
  poll_interval_ms INTEGER NULL,      -- the receiver's, when it gave one
  ttl_ms           INTEGER NULL,      -- the receiver's stated ttl (NULL: unlimited or not given)
  ended_by         TEXT NULL,         -- who cancelled, for a cancel lmgw sent
  result           TEXT NULL,         -- §1.4's result as IR tool-result blocks (JSON), once ended
  created_at       TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX mcp_tasks_live ON mcp_tasks(server_id, task_id) WHERE state <> 'ended';
CREATE INDEX mcp_tasks_thread ON mcp_tasks(thread_id);
CREATE INDEX mcp_tasks_state ON mcp_tasks(state);
```

- `open` rows are followed; `ended` rows wait for delivery (§3.1) and are deleted by it;
  `cancel_owed` rows wait for their server. Nothing else removes a row (T9).
- `(server_id, task_id)` is unique among the rows not yet `ended`: receivers make ids unique
  among their own tasks only, and may reuse one (as built, WP1: a new task whose id a live row
  holds ends that row first, see the WP1 notes).
- `store/mcp_tasks.rs` holds every query; `thread_gone` and `server_gone` take the caller's
  transaction.

### 2.2 Result rows (`migrations/0074_chat_message_task.sql`; built as **0075**)

`ALTER TABLE chat_messages ADD COLUMN task TEXT NULL;`, JSON `{task_id, server_label, tool,
status, ended_by}`. A result row is:
- `role = 'tool'` (the column has no check; `0009_chat.sql:20`'s comment gains the value);
- `content`: §1.4's text, flattened (images as their placeholder text), for every reader that
  does not take IR: exports, search, the feed's `message.*` renderings, text-only clients;
- `ir_messages`: the synthetic pair of §3.2, the bytes every later turn replays;
- `task`: the facts above.

### 2.3 Writes

Every write is one `begin_write` transaction with its feed record inside: the insert with
`task.started` (§1.2), each status move, the end (§1.4), delivery with `task.done` (§3.1),
`thread_gone` inside the thread delete's transaction, `server_gone` inside the row removal's. A
write that fails records nothing (client-apps §2.3).

## 3. A late result in a thread

### 3.1 Delivery

**Rule:** an `ended` row is written into its thread as a result row **only while no turn of that
thread runs**, under the thread's history lock (`LiveTurns`, `web/chat_live.rs:193-260`). Three
moments deliver:
1. **The row ends while the thread is idle:** the follower delivers at once.
2. **A turn ends** (after its reply is saved or dropped): it delivers what waited for its thread.
3. **A turn starts:** after `begin`, under its own `save_lock` and before it reads the history,
   it delivers what waits. So a result that ended a moment before a send is in that send's
   history, and a bound continuation (§3.4) always finds it. *Amended (WP2):* only a fresh turn
   (a send, an answer) delivers at its start; a continue or a resumed turn extends the last
   reply, which must stay last, so what waits enters when it ends. A reply waiting for
   approvals, or for the turn that runs its decided calls, holds results off too.
   *Amended (2026-10-09, after T11):* **a send delivers before it stores its message.** A result
   that waits when a send arrives (the previous turn just ended, a reply's approvals held it,
   idle delivery had not run yet) used to enter at the start of the send's turn, after the
   send's user message, so the request ended `… U2 [call R] R` and the model answered the result
   instead of the user, the case T11 was decided on. Now the message's own write delivers it:
   in the message's transaction, under the history write it takes (the live turn cancelled, its
   reply no longer savable), after the waiting calls the message declines and before the
   message (`store::send_user_message_by`, from `ChatRepo::append_user_message`). The send's
   decline comes first, so a reply whose approvals held the result off holds nothing once the
   message declines its calls; while the calls still wait, nothing enters, as before. A rolled
   back send (an attachment no longer a draft) lets nothing in. A bound session's spoken turn
   writes its user message the same way (`chat_voice::bound::write_user`), so it gets the same
   order. **From before that write until the message's turn has begun**, the thread counts as
   running (`LiveTurns::sending`, held as `chat_tasks::deliver::Sent` and handed to the turn as
   `TurnOpts::sent`): a result that ends after the message waits for the turn's end, as one that
   ends while the turn runs, and a turn that starts only on an idle thread (`answer`) is
   refused `turn_running`. The send's turn delivers nothing at its start. If the turn is
   refused before it begins, the hold drops and what waits enters as on an idle thread. An
   edit or a regenerate answers a message already stored, whose place is fixed, so its start
   still delivers, after that message. A heard turn's spoken row is written after its turn
   began, so its start delivers and the spoken row follows the result (§3.2's last row).
   *Amended (2026-10-09, review):* **a bound session's transcript retry follows the edit and
   regenerate rule.** A heard response whose audio attempt began and was refused goes again as
   its transcript (voice-audio-input §3.5): a second turn, `Fresh {user_message_id: None}`, over
   the history that now holds the user's row. The send's hold went to the first attempt
   (`Starter::sent` is taken once, `realtime/thread/turn/audio.rs`), so the retry starts with
   `sent: None` and its start delivers what waits, after the message already stored, as an edit's
   does: a result that ended between the two attempts is rendered `U [call R] R → reply`.

Delivery is one transaction: insert the result row, record `task.done`, delete the `mcp_tasks`
row. Then, after the commit, the feed's wake and the bound session's `lmgw.task.done` (§4).

*Why the gate:* a write while a turn runs moves the thread's generation, so that turn's reply
could not be saved; and a row slipped in during the turn would sit before a reply that never saw
it (T10).

### 3.2 What the model sees

The result row's `ir_messages` is a pair, written once:

```json
[{"role": "assistant", "content": [{"type": "tool_use", "id": "lmgw_task_41",
   "name": "lmgw__job_result", "input": {"job": "7f3a", "tool": "desktop__run_command"}}]},
 {"role": "tool", "content": [{"type": "tool_result", "tool_use_id": "lmgw_task_41",
   "name": "lmgw__job_result", "content": [{"type": "text",
   "text": "job 7f3a (desktop__run_command) completed\n…"}]}]}]
```

(IR shapes as `ir::ContentPart::ToolUse` / `ToolResult`; the JSON is illustrative.)
- **The name `lmgw__job_result`** is in lmgw's reserved namespace (no server may take the prefix
  `lmgw`), so it never collides with a real tool. A model that calls it gets the existing answer
  for a tool the thread does not have. The arguments name the job and the tool the model called.
- **The id** is `lmgw_task_<mcp_tasks.id>`; the egress's call-id handling applies as to any id.

**Placement** (`web/chat_tasks/render.rs`, called from `build_messages`; T11 as the owner decided
it, replacing the draft's "immediately before the first assistant reply stored after it; with
none, at the end"): a result row is rendered **where it is stored among the thread's messages**,
chronologically. The pair's call **joins a directly preceding assistant message** (one message:
its text, then the call), else stands as an assistant message of its own; its result follows;
whatever was stored after the row — a user message, a reply, another result — follows that. A
heard turn's spoken words (not stored yet, chat-voice) come after every stored row. A result is
**answered by the first reply stored after it**, whether or not a user message came between
(`render::unanswered`, the owed set's read, §3.3–§3.4).

The cases, `A` an assistant reply, `U` a user message, `R` a result row, in stored order:

| Stored | Rendered | Check |
|---|---|---|
| `U1 A1 R` + a dashboard send `U2` (R entered the idle thread first) | `U1 [A1 + call R] R U2 → reply` | the reply answers `U2`, `R` as context; on Anthropic and Gemini `R` and `U2` are one user turn (tool_result / functionResponse first, then the text) |
| `U1 A1 R` + a send `U2` that found `R` waiting (§3.1's third moment, as amended: `R` enters in `U2`'s write, before it) | `U1 [A1 + call R] R U2 → reply` | the same bytes as the first row |
| `U1 A1 U2 R`: `R` let in at the start of the turn of an edit or a regenerate of `U2` (its place fixed) | `U1 A1 U2 [call R] R → reply` | stored after the message, so rendered after it; generation follows a tool |
| `U1 A1 R` + a continuation (`answer`, a bound session's bare `response.create`) | `U1 [A1 + call R] R → reply` | no two assistants; the tool result follows its call |
| later replays of any of them | the same bytes: `R` stays where it is stored | prefix stable |
| `U1 A1 R1 R2 U2` | `U1 [A1 + call R1] R1 [call R2] R2 U2 → reply` | a two-step tool sequence, then the user |
| `U1 A1 R U2 A2`, then A2 regenerated | `U1 [A1 + call R] R U2 → new reply` | T13: the result is answered again |
| `U1 R` (A1 regenerated or deleted) | `U1 [call R] R → reply` | the call stands alone after the user |
| a heard turn after `U1 A1 R` | `U1 [A1 + call R] R U:spoken → reply` | the spoken row is stored after `R` too |

A tool result always directly follows its call and two assistant messages never meet (a reply
followed by a result takes the call into itself; a result's call never meets a reply, its result
is between). A user message may follow a tool result, as the OpenAI, Anthropic and Gemini APIs
take it; a chat template that refuses it is fixed, or the model is not the one for the task, and
lmgw builds no workaround for it (T11).

**A result first** (the messages before it deleted, `R`, `R A2` or `R U2` stored): every other
request opens with a user turn, so such a pair follows a minimal user turn, `(the messages
before this job's result were deleted)`, rather than opening the request with the call, which
Anthropic (the user's turn first) and Gemini (a call follows a user turn or a function
response) refuse. The words are lmgw's, never the user's; the row is not stored.

**The original call keeps its `started, job 7f3a` result.** The record of the turn that started
the job is never rewritten (Rejected).

### 3.3 Chat turns and the dashboard

- **The next turn** of the thread renders every result row before it as §3.2 places them; a
  dashboard send needs nothing new.
- **`POST /chat/api/threads/{id}/answer`** (T12) runs a continuation: `TurnMode::Fresh
  {user_message_id: None}` (`web/chat_turn.rs:60-71`), streaming the same frames as `send`.
  *Amended (WP2 review fixes):* the continuation is its own `TurnMode::Answer`, which takes the
  thread only while no turn is live and refuses `nothing_to_answer` after delivering what waits
  when nothing is left unanswered.
  - It requires an unanswered result: a result row with no assistant reply stored after it.
    Otherwise `409 nothing_to_answer`, asked again once the turn took the thread.
  - While a turn of the thread runs it is `409 turn_running`: it would cancel that turn, whose
    end lets the results in and whose reply may answer them.
  - It runs on the thread's alias with its configured fallbacks, as every turn does; a `gpu_hold`
    with no fallback fails visibly and the result stays unanswered.
- **A task-augmented call in a tool loop** counts against `responses_max_tool_calls` as any call
  does; the synthetic pair is no call and counts nothing.

### 3.4 Bound realtime sessions

- **`lmgw.task.done`** goes to the thread's bound session after each delivery (§4.2). A bound
  session already carries `lmgw.*` events; the binding is the opt-in (chat-voice §8.6).
- **The session keeps an owed set**: the result rows its thread holds with no reply after them.
  It is read from the thread at bind, filled by each `lmgw.task.done` it sends, and cleared by a
  response whose turn saved a reply. *Amended (WP3):* it is never kept by counting; it is read
  from the thread at the bind and at every wake of the thread (a delivery, a turn's end, a
  history write), through one indexed statement.
- **A `response.create` with no new words** and a non-empty owed set runs a continuation instead
  of `empty_turn` (`realtime/lifecycle/bound.rs`): the journal gets an entry with a reply slot and
  no user turn, and the turn is `Fresh {user_message_id: None}`. With an empty owed set the
  refusal stays `empty_turn`. *Amended (WP3):* the turn is `TurnMode::Answer`, as the route's; a
  turn of the thread running fails the response `turn_running`; only the client's own
  `response.create` continues (never turn detection's automatic response, nor one after a commit
  of silence).
- **A result that ends during a response** waits for that response's turn to end (§3.1), then
  `lmgw.task.done` goes out; the client picks its quiet moment and sends `response.create`.
- **Ruling 22:** a client that speaks a result with no session bound binds the thread
  (`takeover=never`, chat-voice §8.1) and sends `response.create`; the owed set read at bind makes
  it a continuation. lmgw needs nothing more.
- **Unbound sessions** bridge (T4), within the row's `timeout_ms`, as their MCP calls end the
  response anyway (server-tools decision 1).

### 3.5 History edits

- **Edit and regenerate** truncate by id (`store/chat_messages.rs:180`, `:273-283`); they keep
  rows of role `tool` (`AND role <> 'tool'`), so a result outlives the reply that answered it and
  is answered again (T13).
- **Deleting a message** by id deletes a result row like any message; the owner chose it.
- **The turn that started a job** may be cut by an edit; the job runs on and its result arrives
  (T6). Its text names the job and the tool, so it reads on its own.

### 3.6 Temporary threads, Admin Chat, devices

- **Temporary threads** bridge (T4): they are not stored and go away with their window.
- **Admin Chat threads** take the late path as any stored thread; for a device they do not exist
  (client-apps L3), so their `task.*` records and `lmgw.task.done` never reach a device, and their
  id is `null` in `_meta["lmgw/task"]`.
- **A device cancels** only tasks of threads it reaches (the route's 404 otherwise). Reaching
  is enough: a `read_only` device that sees the thread cancels its tasks, as it declines an
  approval there (T16; client-apps §6.6) — a cancel stops work, it starts none.

## 4. Feed and events

### 4.1 Feed (`store/feed.rs`, `web/chat_feed/*`, `lmgw-api-types::chat_feed`)

| `event` | `data` | Kind |
|---|---|---|
| `task.started` | `{thread_id, id, task_id, server_label, tool, by}` | stored |
| `task.done` | `{thread_id, message_id, id, task_id, server_label, tool, status, by}` | stored |
| `device.revoked` | `{device: {kind: "device", name}, by}` (*added 2026-10-09*) | stored |

- `id` is lmgw's task id (the route's `{task}`); `task_id` is the receiver's.
- The facts are in the record's `detail`, so a record renders after its task row is gone (as
  `profile.*` keep id and name, personality-profiles §3.2). A deleted thread's records render as
  its other records do.
- `by`: the starting principal for `task.started`; for `task.done` the canceller of a cancel lmgw
  sent, otherwise `null` (the gateway).
- The thread's own `thread.updated` is not recorded for either; a result row's `message.added`
  is recorded wherever message writes record (client-apps WP10).
- Status moves (`working` ↔ `input_required`, a new `statusMessage`) are not in the feed; the
  thread's `tasks` list (§5.1) shows them (client-apps L7: progress is not feed material).
- *Added 2026-10-09 (the client's WP18):* **`device.revoked`**, a paired device's key deleted, so a
  host may cancel the jobs that device started (the caller its `lmgw/caller` named). The host of
  a task the deleted device started that was still open (`open`, or `cancel_owed`: cancelled, the
  cancel not yet sent to its server, so the host may still be running it) receives it whatever
  the thread's level;
  otherwise it follows client-apps §2.2's rule (the owner, and a device shown the deleted
  device's changes). Only a delete: a disabled key's tasks wait (§1.6), so a disable, a rotate,
  an expiry and a cleared hosting grant record nothing. lmgw ends none of the deleted device's
  tasks for it; their results enter their threads. Client-apps §2.2 has the full entry.

### 4.2 Bound session

`lmgw.task.done {thread_id, message_id, id, task_id, server_label, tool, status, by}` (*amended,
WP3:* `by` too, the feed's `task.done` field for field), typed in `lmgw-client`'s realtime events
beside `lmgw.chat.*`.

### 4.3 The Chat's tool frame

The `tool` frame of `event: "result"` (`web/agentchat.rs:493-506`) gains `task: {id, task_id,
server_label}` for a call that started a task, so a client never parses `started, job …`. The
bound session's `lmgw.chat.frame` relays it as every frame.

## 5. API, settings, documentation

### 5.1 Routes (`web/chat_tasks/routes.rs`, `Cap::Chat`)

| Route | Body | Answer |
|---|---|---|
| `POST /chat/api/threads/{id}/answer` | `{}`, or `{speak}` as `send`'s | the `send` frame stream; `409 nothing_to_answer`, `409 turn_running` |
| `POST /chat/api/threads/{id}/tasks/{task}/cancel` | `{}` (ignored) | `{task: …, delivered: bool, note}`; `404 task_not_found` for a task not of the thread or one whose result entered it, `409 task_ended` for one that ended and waits to enter, `409 task_cancel_unsupported`, `502 task_cancel_refused` (*as built, WP2*) |
| `GET /chat/api/threads/{id}/tasks` (*added in WP4*) | — | the thread's `tasks` without its history, `Vec<ThreadTask>`; the strip's poll |

- **`GET /chat/api/threads/{id}`** gains `tasks`: the thread's rows in state `open` or `ended`,
  `[{id, task_id, server_label, tool, status, status_message, ttl_ms, started_at, by,
  waiting_for}]`, `waiting_for` naming an offline device or a running turn.
- **Messages** gain role `tool` and the `task` facts in every message rendering (`GET` thread,
  exports, `message.*`).
- **Device reach:** the routes resolve the thread as every Chat route does (L3).

### 5.2 Settings

`mcp.task_poll_interval_s` (integer ≥ 1, default 5), in `HostSettings`
(`lmgw-api-types/src/mcp_host.rs`), saved with Settings → MCP like `mcp.host_*`, labelled "Task
poll interval" with the note "used when a server suggests none; a server's status notifications
act at once". It applies from each task's next poll. There is no other new setting (T9).

### 5.3 The host link's facts (`lmgw-api-types::mcp_host`)

- `CallMeta` gains `task: Option<TaskMeta {delivery, thread_id}>`, serialized as `"lmgw/task"`
  only on a task-augmented call.
- The module doc states the link's Tasks: revision 2025-11-25; what a device declares
  (`capabilities.tasks.requests.tools.call`, `cancel`; `execution.taskSupport: "required"`); what
  lmgw sends (`task: {}`, `tasks/get`, `tasks/result`, `tasks/cancel`); that it reads
  `notifications/tasks/status` and `pollInterval`; that a link drop ends nothing and a restarted
  device answers `-32602`.

### 5.4 API-docs entries

- A `DocRoute` for each of §5.1's routes (three as built) in a new `openapi/planes/chat_tasks.rs`, merged by
  `planes/chat.rs`; two `CAPABILITY_TABLE` rows (`server.rs`); `route_walk.rs` covers them.
- The feed's `oneOf` gains two branches (`openapi_coverage.rs`'s one-branch test).
- The thread schema gains `tasks`; the message schema gains role `tool` and `task`; the settings
  schema gains `mcp.task_poll_interval_s`.
- The realtime description's `lmgw.*` list (`openapi/planes/inference.rs:555-575`) gains
  `lmgw.task.done`.
- **No op** (nothing in `op_names.rs` / `OpDoc`) and **no `x-lmgw` header** (`LMGW_HEADERS`).

## 6. UI (lmgw-ui)

- **A result row** renders as a tool card in the transcript: "Job 7f3a · desktop__run_command ·
  completed", the result collapsed like a tool frame's output, its time; failed, cancelled and
  abandoned in their colours.
- **Open tasks** show as a strip above the composer: tool, label, status, status message, since
  when, what it waits for; **Cancel** with a confirmation naming the tool, then really cancels
  (confirm-then-do).
- **Answer** appears beside the composer while the thread has an unanswered result; it calls §5.1.
- **The MCP page** shows "N open tasks" on a server's row.
- **Settings → MCP** gets the poll interval field.
- *Added 2026-10-09 (review):* **results let in before the page's own turn are followed in
  place.** A send lets what waits in before its message (§3.1), and an answer, edit or regenerate
  at its start before its reply, so after the page's own turn the stored rows hold results the
  page lacks in front of a message it shows (`[R, U, A]` where it shows `[U, A]`; the user turn
  known by the `turn` frame's id, or still a bubble). The Chat's follower
  (`chat_sync::plan`) inserts those results (rows of role `tool`) in front of the message they
  were stored before — or, when the turn's messages are still bubbles without ids, in front of
  its user turn, the bubbles adopting the rows after them — instead of loading the transcript
  afresh. Any other row the page lacks in front of one it shows still reloads.
  `scripts/chat-tasks-drive.sh`'s last phase drives it: a build ends while a gated reply holds
  its result off, the next send lets it in before its message, and the page shows the result in
  front of its own message with every id known, without a reload.

## 7. Tests

New integration tests are modules of the one `tests/it` binary. WP1 creates every new module file
below as a stub with its `mod` line.

- **`mcp_tasks/wire.rs`** (a fake device over the host link, `tests/it/mcp_host`'s helpers):
  - a `required` tool on a device declaring the capability is called with `task` and `_meta`'s
    `lmgw/task`; `optional`, absent and `forbidden` are called normally;
  - `required` without the capability is the named error;
  - `model-immediate-response` reaches the tool result;
  - a `CallToolResult` to an augmented call is a normal result; a `CreateTaskResult` to a normal
    call is an error.
- **`mcp_tasks/follow.rs`:**
  - a status notification ends the task at once; without notifications `tasks/get` runs at the
    task's `pollInterval`, then at the setting (paused time);
  - `input_required` holds one `tasks/result`;
  - every row of §1.4's table;
  - the end's request row.
- **`mcp_tasks/link.rs`:**
  - a link drop and a takeover end nothing, and the next link is polled at once;
  - a device that answers `-32602` after reconnecting ends the task abandoned;
  - a deleted key and a cleared grant end its tasks abandoned in that write;
  - a disabled key's tasks wait;
  - lmgw restarted: open rows resume.
- **`mcp_tasks/thread.rs`** (mock upstream with scripted tool calls):
  - the turn's tool result is `started, job …` and the turn answers;
  - a task completed **before** its turn ended is delivered after the save, never in between;
  - one completed **after** with the thread idle is delivered at once;
  - one completed during a later turn waits for it;
  - a send renders the pair after the user message; a later send replays the same bytes
    (*amended, T11 decided:* a send after an entered result follows the pair, joined to the
    reply before it; a result let in at the send's start follows the message) (*amended
    2026-10-09, §3.1:* a send that finds the result waiting lets it in before its message, the
    same bytes as a send after it entered; a result a reply's waiting call held off enters
    before the message of the send that declines the call, and a bound session's spoken turn
    does the same; one that ends after the message waits for the turn's end);
  - `answer` renders the pair joined to the preceding reply; `409 nothing_to_answer`;
  - edit and regenerate keep result rows, and the regenerated reply answers again;
  - each rendered history passes a strict-template check (no user after tool, every result
    directly after its call, no adjacent assistants) (*amended, T11 decided:* "no user after
    tool" dropped; the other two stay);
  - goldens of the request sent to an OpenAI-shaped, an Anthropic-shaped and a llama-server
    mock.
- **`mcp_tasks/cancel.rs`:**
  - the owner's cancel sends `tasks/cancel` and delivers `cancelled by …`;
  - a cancel that loses to completion delivers the real result;
  - a cancel while offline is owed and sent at the next link;
  - a thread delete and a sweep purge owe cancels for open tasks and drop ended ones;
  - a device's own cancel arrives as `cancelled`;
  - a device cannot cancel a task of a thread it does not reach.
- **`mcp_tasks/bridge.rs`:**
  - `/mcp`, `/v1/responses`, an unbound realtime session, an agent run and a temporary thread get
    the result inline;
  - a timeout sends `tasks/cancel` and names the setting;
  - `/mcp`'s `tools/list` carries no `execution`.
- **`realtime_chat_thread/tasks.rs`:**
  - `lmgw.task.done` after delivery;
  - `response.create` with no words runs the continuation, and with nothing owed is
    `empty_turn`;
  - a result delivered before the bind is owed at bind;
  - a result ending during a response waits for it;
  - no session bound: the result is stored and the feed has `task.done`.
- **`chat_feed/tasks.rs`:** `task.started` and `task.done` in commit order, their rendering after
  the task row is gone, filtered for a device on Admin Chat.
- **Existing suites:** `migrations.rs` (0073, 0074 on a populated DB), `mcp_reconcile.rs` or the
  reaper's tests (a server with open tasks is not reaped), `route_walk.rs`,
  `openapi_coverage.rs`, `store_begin_scan.rs` pass.

**Live checks** (dev instance via `scripts/dev-instance.sh`, M7's tool-calling mock, a scripted
fake device with a slow `required` tool):
1. A dashboard send starts a job; the reply comes at once; the result card appears when the job
   ends; the next send and Answer both answer it.
2. A bound voice session: `lmgw.task.done`, then `response.create` speaks the answer.
3. A link dropped while the job runs: the result still arrives after the reconnect.
4. The owner's cancel from the strip.

*Done 2026-10-09 (WP5), all four pass:* `scripts/tasks-live-check.sh [--ui]`; results and timings
in WP5's as-built notes (§8).

The client's WP18 live check (a real announce with speech) is in its own repository.

## 8. Work packages (build order)

Each WP ends green on `bash ci/check.sh`, commits with explicit paths, and lands by rebase and
fast-forward.

| WP | After | Owns | Delivers |
|---|---|---|---|
| **WP1 Wire, follower, bridge** | M1 (merged); beside M2 and M3 | `mcp/tasks.rs`, `mcp/tasks/{wire, follow, bridge, meta}.rs` (new); `store/mcp_tasks.rs` (new), `store.rs` (mod line); `migrations/0073_mcp_tasks.sql`; hook lines in `mcp/mod.rs` (the call's branch to tasks, the reaper's skip), `mcp/host/calls.rs` (the augmented forward, `lmgw/task`), `mcp/handler.rs` (`on_task_status`), `mcp/ingress.rs` (the `execution` strip, one call into `mcp/tasks/meta.rs`), `mcp/exec.rs` (`McpExecutor::with_late`, inert until WP2); `config` and `lmgw-api-types/src/mcp_host.rs` (`HostSettings.task_poll_interval_s`, `CallMeta.task`, module doc); `ops/mcp_host_settings.rs` (the save); every new `tests/it` stub; `tests/it/mcp_tasks/{wire, follow, link, bridge}.rs` | §1, §2.1, §5.2, §5.3; the late path stores `ended` rows that nothing delivers yet |
| **WP2 Thread delivery and the Chat** | WP1, **M3** | `web/chat_tasks.rs`, `web/chat_tasks/{deliver, render, routes}.rs` (new); `migrations/0074_chat_message_task.sql`; hook lines in `web/chat_turn.rs` (`build_messages` → `render`, the start's delivery), `web/agentchat.rs` (`with_late`, the frame's `task`, the end's delivery), `store/chat_messages.rs` (truncation keeps `tool`), `store/chat.rs` and the sweep (`thread_gone`), `ops/mcp_server.rs` and `ops/keys.rs`'s grant write (`server_gone`), `web/mod.rs`, `server.rs`; `store/feed.rs`, `web/chat_feed/*`; `web/chat_export.rs`; `openapi/planes/chat_tasks.rs` + `planes/chat.rs`; `lmgw-api-types/src/{chat, chat_feed}.rs`; `lmgw-client/src/{feed, requests}.rs`; `tests/it/mcp_tasks/{thread, cancel}.rs`, `chat_feed/tasks.rs` | §2.2, §3.1–§3.3, §3.5–§3.6, §4.1, §4.3, §5.1, §5.4 |
| **WP3 Bound sessions** ∥ WP4 | WP2, **M3** | `realtime/lifecycle/bound.rs` (owed set, continuation), `realtime/thread/bind.rs` (owed at bind), `realtime/thread/hooks.rs` (`lmgw.task.done`), `realtime/thread/journal/*` (an entry with no user turn), `realtime/protocol/server.rs` (the event); `openapi/planes/inference.rs` (the event's line); `lmgw-client/src/realtime.rs`; `tests/it/realtime_chat_thread/tasks.rs` | §3.4, §4.2 |
| **WP4 UI** ∥ WP3 | WP2 (and M5, if M5 is in flight) | `lmgw-ui/src/pages/chat*.rs` (result card, strip, Answer), the Chat's CSS, `pages/settings` (MCP field), `pages/mcp*.rs` (the count) | §6 |
| **WP5 Acceptance** | all | this record's "as built" notes; release notes | §7's live checks 1–4 |

**Against the work in flight:**
- **M2** (resources) owns `mcp/ingress.rs`, `mcp/resources.rs` and the tool frame's fields in
  `web/agentchat.rs`. WP1 touches `mcp/ingress.rs` and `mcp/mod.rs` with one hook line each, and
  may build beside M2: whichever lands second rebases those lines.
- **M3** (approvals) owns the gated turn in `web/agentchat.rs`, `store/chat.rs`'s `ThreadMcp`,
  `realtime/thread/owned.rs`, `realtime/conversation/mcp.rs` and `lmgw.approval.decided` in
  `realtime/protocol/server.rs`. WP2 and WP3 touch `web/agentchat.rs`, `store/chat.rs` and
  `realtime/protocol/server.rs`, so they start **after M3 lands**. An approved `required` call
  then simply becomes a task (`_meta` carries both `lmgw/approval` and `lmgw/task`).
- **WP1 needs neither**, so M4 starts the day M1 is merged and its riskiest part (the wire and
  the follower) is proven before the shared files are free.
- The client's WP18 builds against WP3 (bound continuation) and the feed of WP2.

*As built (WP1), 2026-10-09:* `mcp/tasks.rs` and its children (`wire`, `follow`, `bridge`,
`meta`), `store/mcp_tasks.rs`, migration 0073; the late path stores `ended` rows that nothing
delivers yet. Where this record was silent:
- **The late context rides on `CallFrom`**: `late: Option<tasks::Late {thread_id,
  device_sees_thread, call_id}>` and `logged_as {client_key, proto}` (for T20).
  `McpExecutor::with_late(thread_id, device_sees_thread)` sets it per call; the call id is
  `agent::current_call_id()`, a task-local the loop's `execute` sets around each call (empty for
  a call made outside a loop). Without `with_late` every caller is bridged.
- **The follower is one tokio task per followed row**, registered by `(server id, task id)`; a
  bridged task registers too, for its notifications. The reaper skips a server with an `open`
  row's follower (T17); an owed cancel does not hold a server.
- **The pinned revision is exact:** a `required` tool on a session that negotiated anything but
  2025-11-25 is refused by name, as one without the capability is.
- **Bounds:** `tasks/get`, a terminal task's `tasks/result` and `tasks/cancel` are each bounded by
  the row's `timeout_ms`, and a request that did not answer is asked again at the next poll; the
  `input_required` one is held unbounded. A registered server is polled through the lazy connect
  (it honours the backoff) while its row is enabled; a device row only while `Ready`, and its
  `link_ready` nudges its followers (one line in `mcp/host/conn.rs`, whose `device_peer` is now
  visible to `mcp::tasks`).
- **`-32602`:** to `tasks/result` after a `failed` status it reads as the task's own JSON-RPC
  error; otherwise abandoned. To `tasks/cancel` it means already terminal: the real result is
  fetched (`CancelOutcome::FinishedFirst`).
- **A cancel with no live session** (`McpManager::cancel_task(thread_id, id, by)`, for WP2's
  route; it refuses a row of another thread as `NotFound` and one no longer `open` as `Ended`, so
  a route cannot skip the check) ends
  the row in state `cancel_owed` with its result and `thread_id` kept. WP2's delivery calls
  `store::mcp_tasks::delivered_in` (an `ended` row goes; a `cancel_owed` one loses its result and
  thread and keeps waiting); the cancel sent (`owed_cancel_sent`) deletes the row, or leaves it
  `ended` while its result still waits. A `tasks/cancel` answered with a non-terminal task leaves
  it followed (`CancelOutcome::Requested`).
- **`server_gone(conn, server_id, result_of)`** takes the per-row result
  (`tasks::removed_result`); `thread_gone(conn, thread_id)` returns the ids that now owe a cancel.
  After either commits, WP2 calls `McpManager::resume_tasks()` (idempotent; also run at start
  from `set_state`), which starts owed-cancel followers and makes the others read their row.
- **The end's request row (T20)** is written with the existing tool-row writers: a late task's
  under `chat-tool` (only Chat turns take the late path) and the row's `started_by`, a bridged
  one under its caller's proto and key; completed 200, failed and abandoned 502 `tool_error`,
  cancelled 200 `canceled`, `total_ms` from the row's `created_at`.
- **A late row that cannot be inserted** has its task cancelled, and the call fails saying so.
- **The feed's `task.started`** is WP2's (`store/feed.rs`): WP1 inserts with
  `store::mcp_tasks::insert`; `insert_in` is there for WP2 to put both in one transaction.
- **`mcp.task_poll_interval_s`:** a save refuses 0; a stored 0 reads as the default (5) at load,
  logged. Its Settings → MCP field is WP4's (§6); `GET /api/settings-full` and the settings patch
  carry it now. `CallError::Timeout` now names its setting ("… (its timeout_ms)"), so the
  bridge's timeout does. `/mcp/host`'s API-docs description states the link's Tasks;
  `lmgw-client::mcp_host` re-exports `TaskMeta`, `TaskDelivery` and `META_TASK`.

*As built (WP1 review fixes), 2026-10-09:* the follower's registry is `mcp/tasks/registry.rs`
and the cancel `mcp/tasks/cancel.rs` (both out of `follow.rs`).
- **`pollInterval: 0`** reads as not given, so the setting's interval applies; and while there is
  no session to ask (a device offline, a registered server that does not connect, a row gone)
  the next look is `mcp.task_poll_interval_s` away, whatever the task's own. A positive
  `pollInterval` is honoured as given, with no floor. *Taken in this draft, open to the owner's
  veto.* *Why:* 0 is no interval, and polling it means a busy loop; a server's interval is how
  often to ask it, and with nobody to ask it only spun a core.
- **A refused cancel goes on being followed.** `cancel_task` returns `CancelRefusal::Server(msg)`
  when the server answers `tasks/cancel` with a JSON-RPC error, and
  `CancelRefusal::Unsupported(why)` without sending anything when the session's `initialize`
  result does not declare `capabilities.tasks.cancel`; only a cancel that got no answer (no
  session, or none within `timeout_ms`) is owed. An owed cancel whose server, once connected,
  does not declare `tasks.cancel` is dropped unsent (the row then goes as if sent), and the
  bridge sends none to such a server either. *Taken in this draft, open to the owner's veto.*
  *Why:* a server that said no, or cannot cancel, was reachable: booking its task "not
  connected, owed" told the thread a falsehood and ended a task that still runs; and the spec
  gives `tasks/cancel` only to receivers that declare it. A row cancelled while offline still
  says "is told when it is" for such a server, since lmgw cannot know its capabilities offline.
- **A reused task id.** A server that answers a new task with the id of one lmgw still follows
  ended the older one as far as lmgw can tell: the older `open` row ends `abandoned` with "server
  '<name>' reused its task id for a new job: this one was abandoned; it may or may not have
  finished" (delivered like any end, its end's request row written), an owed cancel of it is
  dropped (it would cancel the new task), a bridged wait on it ends with that error; the new
  task is followed under a fresh row and never cancelled for it. The table's `UNIQUE (server_id,
  task_id)` became a unique index over the rows not yet `ended` (migration 0073 edited in place,
  as it has not landed), `store::mcp_tasks::insert_reusing` ends and inserts in one transaction,
  `reused` does the same for a bridged task. *Taken in this draft, open to the owner's veto.*
  *Why:* the old constraint made such an insert fail, and the K26 rule then cancelled the new
  task; and before that, two followers of one id would have read each other's results.
- **Status notifications coalesce** per followed task (latest wins), so a server that floods
  them queues nothing. *Taken in this draft, open to the owner's veto.* *Why:* only the newest
  status means anything; polling reads the same.
- **A bridged result loses `_meta["io.modelcontextprotocol/related-task"]`** (other `_meta` keys
  stay): `/mcp` and `/v1/responses` callers made a normal call. *Taken in this draft, open to the
  owner's veto.*
- **Fixes without a decision:** a follower or bridged wait that goes stops its held
  `tasks/result` (`Drop` for `Follow`); the bridge claims its key like any new task, so it no
  longer ended at once ("the gateway is shutting down") when the key was taken, and a late row
  is always followed (its registration is per-claim, with a ticket, so an older follower that
  stops never forgets the newer one); realtime's responder runs each call under its call id
  (`agent::with_call_id`), so a bound session's task (WP3) records it.

*As built (WP2), 2026-10-09:* `web/chat_tasks.rs` and its children (`deliver`, `render`,
`routes`), `store/mcp_tasks/delivery.rs`, `store/feed/tasks.rs`, `web/chat_feed/tasks.rs`,
`openapi/planes/chat_tasks.rs`, migration **0075** (`chat_messages.task`; 0073 and 0074 were
taken), `lmgw-api-types::chat`'s task types (`chat/tasks.rs`), `lmgw-client::requests::tasks`.
`McpExecutor::with_late` is on for every stored thread's turn. Where this record was silent:
- **Delivery writes under `LiveTurns::hold`** (the thread's lock, no generation move) after
  checking under it that no turn is live; a turn's start delivers under its own save lock
  (*since 2026-10-09, §3.1:* a send and a bound session's spoken turn deliver in their user
  message's own write instead, before the message, and their turn's start delivers nothing).
  Only a fresh turn delivers at its start: a continue and a resumed turn extend the last reply, which
  must stay last, so what waits enters when they end. The end's delivery runs after the turn's
  worker let its ticket go, for the plain and the tool path alike. *Taken in this draft, open to
  the owner's veto.*
- **A reply waiting for approvals holds results off** (the thread's task list says
  `waiting_for` the decision): a result written after it would make the decision `409
  approval_moved_on`, declining the calls for the user. They enter once the decided turn ended,
  or a new message declined the calls. *Taken in this draft, open to the owner's veto.* (The
  review fixes below widen "waiting" to the turn that runs the decided calls.)
- **Thread deletes**: `ON DELETE SET NULL` clears a deleted thread's task rows during the
  `DELETE`, so `store::mcp_tasks::threads_gone(conn)` runs after it in the same transaction, on
  every delete path (by hand, a folder's, the sweep's purge): an `open` row with no thread owes
  its cancel, an `ended` one goes, an owed cancel loses its result (`thread_gone`'s rule; a row
  without a thread is otherwise only ever an owed cancel without a result). The followers send
  the owed cancels after the commit (`resume_tasks`).
- **Server removals**: `store::delete_mcp_server` ends the row's tasks in its own (now
  `begin_write`) transaction, "server '<name>' was removed (it was deleted)"; a device row, in
  the key write that removes it: "(its device's key was deleted)" or "(its device's hosting
  grant was cleared)". After the commit the followers stop and the results enter every idle
  thread (`chat_tasks::servers_gone`, from the MCP server delete op, an agent's row, the key
  delete route and a grant cleared by `key_set`).
- **The synthetic result's `is_error`** is set for every status but `completed`.
- **The frame's `task`** is read from the row the call stored, by thread and the model's call
  id among the rows stored since the model turn began (call ids repeat across turns).
- **`lmgw/task`'s `thread_id`** is the thread's only for a thread without lmgw's admin tools:
  whether one with them is in the hosting device's reach depends on its level, which the call
  does not judge; Admin Chat's is `null` as designed.
- **`GET /chat/api/threads/{id}`'s `tasks`** sits beside `messages` (the route is not in the
  API document; the answer route's description states the list and the result row's fields).
  It lists `open` rows and ended ones whose result waits; `by` is the starter as the feed names
  it (a device key's `device '<name>'`, anything else the dashboard).
- **The cancel route** answers `{task, delivered, note}` and ignores its body. A server without
  `tasks.cancel` is `409 task_cancel_unsupported`, one that answers the cancel with an error
  `502 task_cancel_refused`; a task of another thread, or one whose result entered the thread
  (it is gone then), is `404 task_not_found`, apart from the thread's own `404 not_found`; `409
  task_ended` is a task that ended and waits to enter.
- **`answer`** takes an optional `{speak}` body; a result counts as unanswered when no reply is
  stored after it, or when one waits to enter and nothing holds it.
- **Regenerate** accepts a reply right after a result (a continuation), not only after a user
  message; **auto knowledge retrieval** still runs for a user message the turn's start put
  results after.
- **The Markdown export** heads a result row "Job <task id> · <tool> · <status>" and leaves its
  synthetic record out; the JSON export carries `task`. The dashboard's `chat` frame on
  `/api/events` names a `task.*` record's thread.
- **Tests**: WP1's suites keep a turn running in their thread (`chat_turn_held_for_tests`), so
  an ended row stays readable; the request goldens leave the system prompt out (it names the
  day).

*As built (WP2 review fixes), 2026-10-09:* where the branch review found gaps.
- **The approval gate holds until the decided calls' turn** (M1): a reply holds results off
  while its approvals are open, *or* its calls were decided and its record still ends in them,
  the condition a new message declines them on (`store::decline_waiting`). A result that ended
  between the decision's commit and the resumed turn's start used to land after the reply: the
  resume was refused as moved on, the approved calls closed as not run, and the record's open
  call could join the result's call into one message with a call nothing answered (a 400 on
  OpenAI and Anthropic). The same holds for a bound session's decision. Such a result enters when
  the resumed turn ends, when its calls are closed as not run (`chat_approvals::unrun` delivers
  then), or when a new message declines them; `waiting_for` names "their decision, and the turn
  that runs them".
- **A turn refused after it took the thread delivers** (L1): the end-of-turn guard is taken
  before the turn's ticket and dropped after it, so a refusal after `begin_as` (the device reach
  re-check, a resumed or continued reply gone, an answer with nothing left to answer) lets in
  what waited, as a worker's end does. `deliver::soon` spawns nothing while a turn runs (that
  turn delivers when it ends).
- **A server removed while a task's call was in flight** (L2): `server_gone` ran in the delete,
  before the row existed. The late path's insert now checks the server row in its own write;
  gone, the row ends `abandoned` at once, "server '<name>' was removed (while the call that
  started the job was answered)", and nothing follows it; the result enters the thread when the
  turn ends. And a follower whose server is in neither the snapshot nor the store ends its open
  row the same way ("(while lmgw followed the job)") instead of waiting for a server that is
  gone; one whose row is only missing from a snapshot not yet reloaded still waits.
- **`answer` never cancels a turn, nor runs on an answered history** (L3): `409 turn_running`
  while a turn of the thread runs, and its start (`TurnMode::Answer`) takes the thread only while
  no turn is live (`LiveTurns::begin_idle_as`, decided under the start's lock, so it cancels
  nothing) and, after delivering what waits, refuses `nothing_to_answer` when no result is
  unanswered any more (a turn that ended in between answered it). *Taken in this draft, open to
  the owner's veto:* the new code `turn_running`.
- **Gemini's synthetic call** carries `thoughtSignature: "skip_thought_signature_validator"`,
  the value Google documents for a `functionCall` the model did not generate (Gemini API
  "Thought signatures", read 2026-10-09: Gemini 3 refuses with a 400 a current-turn call whose
  first `functionCall` lacks its signature; the current turn starts at the newest user message
  with text, so the pair after a send and the pair joined to the reply are both in it; *since
  T11's chronological order* a pair before the new message is in an earlier turn, which the
  2026-10-09 change below covers). Sent to
  every Gemini model; `ir::SYNTHETIC_CALL_ID_PREFIX` (`lmgw_task_`) marks such calls. A Gemini
  request golden pins the pair beside the other three. *Since 2026-10-09 (gateway design
  §7.1):* the skip value is no longer keyed on the prefix. It goes to the first call of every
  step that has no captured signature, older turns included, which covers the synthetic call
  and the call that started the job when its record has no signature.
- **Smaller:** `ChatMessageRow::is_task_result` keys on role `tool` alone, so a row whose `task`
  JSON does not read still places as a result; the cancel route's reach is the thread's, so a
  `read_only` device cancels (§3.6). `AppState::chat_turn_held_for_tests` stays a
  `#[doc(hidden)] pub` method: `tests/it` is a separate binary that cannot see `cfg(test)`
  items, and the crate has no test feature; its 25 siblings (`*_for_tests`) are the same.

*As built (WP3), 2026-10-09:* `realtime/thread/tasks.rs` (the owed set, `lmgw.task.done`),
`web/chat_live/results.rs` (the result wake), `web/chat_voice/bound/answer.rs` (a refused
continuation's code), the `lmgw.task.done` variant in `realtime/protocol/server.rs`,
`lmgw-api-types::chat::TASK_DONE_EVENT`, `lmgw-client::realtime::ServerEvent::TaskDone`. Where
this record was silent:
- **The continuation is `TurnMode::Answer`, not `Fresh {user_message_id: None}`** (§3.4 was
  written before WP2's review fixes made `Answer` the continuation's mode). It never cancels a
  turn of the thread: while one runs the response fails `turn_running` (a new realtime code,
  `response.done {failed}`); a start that finds every result answered after delivering what
  waits (a turn of another window answered them since the session last read the thread) fails
  `empty_turn`, after `response.created`. Every other refusal of a bound turn's start stays
  `internal`. *Taken in this draft, open to the owner's veto.* *Why:* the route and the session
  then run one continuation with one set of refusals, and a voice continuation cannot cut a
  text turn that is answering.
- **Only the client's own `response.create` continues.** The automatic response turn
  detection creates for a committed turn answers that turn; one without words (a cough) stays
  `empty_turn` whatever is owed. Words before the create make it an ordinary turn, whose reply
  answers the words and the results alike. *Taken in this draft, open to the owner's veto.*
  *Why:* K26, the client picks the moment; a cough must not speak a job's result.
- **Whether a response may continue is decided when the client's create comes**
  (`Create::continuation`, review fix), not from the turns it answers by the time it starts: a
  create carried over a cough the user started, or held again after a cough cut the
  continuation before anything of it was heard (`pending`, `interrupt`), picks up the cough's
  turn and still continues — a cough that cut a reply nobody heard still answers what it cut.
  *Push-to-talk and manual commits (decided 2026-10-09):* the continuation applies
  only when the client committed no turn since its last `response.create` or the last response
  (`Bound::committed`: an `input_audio_buffer.commit`, or turn detection's commit with
  `create_response` off). A commit of silence followed by `response.create` is a cough's equal:
  `empty_turn`, never a result spoken (K26); the next bare create continues.
- **A cough the ASR gives words** ("Hm.") — or a turn the chat model hears as audio — makes the
  automatic response an ordinary turn, and its reply answers trailing results like any reply
  does. The cough rule holds where the transcript is empty; lmgw does not judge whether words
  are filler.
- **The owed set is read from the thread**, never kept by counting: at the bind once the
  binding is registered (`Owed::at_bind`), again once the session's loop listens, and at every
  wake for its thread — the new result wake (`LiveTurns::results_moved`: a delivery, and every
  stored thread's turn end through `deliver::AfterTurn`) and the approval wake (every history
  write: an edited or deleted reply, the journal's delete of a reply nobody heard, which makes
  its results owed again). The approval wake reads only while the last read found a result row
  in the thread: only a delivery writes one, and it always sends the result wake. A wake missed
  in a broadcast lag is read as a wake.
- **The read** (`store::mcp_tasks::results_read`, review fix) is one narrow statement, not the
  thread's messages: the newest reply's id, the result rows past the smaller of it and the
  watermark (id, call pair, task facts only), whether the thread holds any result row, and
  `chat_messages`' `AUTOINCREMENT` sequence as the watermark. Each part is a range of the new
  index `idx_chat_messages_thread_role (thread_id, role, id)` (**migration 0077**). It runs
  off the session's loop (a spawned task reporting to the loop like the journal does), one at
  a time, a wake during it reading once more after it.
- **The watermark** is the largest message id handed out at the last read — deleted rows'
  included, so a read that found a reply deleted covers that reply's id. A saved reply's `done`
  frame cuts the results stored before it unless a read covered the reply (that read saw what
  became of it and stands); a read taken before the reply was saved does not undo the cut.
  Until a read succeeded the watermark is unknown: a bind whose read failed has the first read
  that succeeds set it silently, so the thread's history of results is not announced.
- **No event at the bind.** Results already in the thread are owed, not said again (the feed
  said them); `session.created` carries no count. A client that needs it reads the thread
  (`GET /chat/api/threads/{id}`: result rows with no reply after them). *Taken in this draft.*
- **`lmgw.task.done` carries `by`** too: it is the feed's `task.done` shape, field for field
  (the contract table's "same fields"; §4.2's list left it out). `lmgw-client` types it with
  `lmgw_api_types::chat_feed::TaskDone`.
- **A result that enters at the start of the session's own turn** (§3.1's moment 3; *since
  2026-10-09* with the response's user message, before it, when the response wrote one) is said
  while that response runs: it is in that response's request, and its reply answers it — the
  client sends no `response.create` for a `lmgw.task.done` that comes during a response (the
  protocol docs say so).
  `lmgw.task.done` and `response.done` are not ordered against each other; the result enters
  only after the turn saved, so it is never in front of a reply that did not see it.
- **The journal has no new entry type:** a continuation queues the usual user entry and reply
  slot; the user entry has no words, writes nothing and answers the turn's barrier with no id
  in its order, so the continuation reads the history after the reply before it was finalized.
- **Cost (review fix):** no read in the session's loop; input audio never waits for one. Per
  voice turn of a thread that holds no result row: **one** read (the turn end's result wake;
  the history-write wakes — its user row, a reply cut or deleted — are skipped). For a thread
  that holds results: one more per such history write, coalesced while one runs. Each read is the indexed statement above, its cost independent of the
  thread's length. Unbound sessions subscribe to no wake and read nothing; temporary threads
  read nothing.

*As built (WP4), 2026-10-09:* `lmgw-ui/src/pages/chat_tasks.rs` (the result card, the jobs strip,
Answer), hook lines in `pages/chat.rs`, `chat_rows.rs`, `chat_sync.rs` and `chat_actions.rs`, the
Chat's CSS (`job-*` classes: the Labs' task picker already owns `.task-head` and `.task-dot`),
Settings → MCP's "Task poll interval", the MCP page's count; `scripts/chat-tasks-drive.sh` with
`scripts/drive/chat-tasks.{json,js}` and `scripts/mock-mcp.py --tasks` (2025-11-25 Tasks: three
`required` tools that complete, fail, or run for an hour). Where this record was silent:
- **The count needed a source.** `GET /api/mcp-servers` (and so `lmgw__mcp_servers`) gained
  `open_tasks` per server: its rows in state `open` (`store::mcp_tasks::open_counts`); an owed
  cancel is not counted. The page shows "N open tasks" beside the status, nothing at 0, and
  reads the list again on every `chat` frame of `/api/events` (a job that starts or ends names
  its thread). Outside WP4's listed files (one store query, one field). *Taken in this draft.*
- **The result card** names job, tool and status from the row's `task`, else from the text's
  first line; its body is the result without that line ("(no output)" when nothing follows);
  its time is local, with the day when not today. Colours: completed green, failed red,
  abandoned amber, cancelled grey. A result no stored reply follows says so under its card.
  Its actions are Copy and Delete only: edit, answer again, continue and read aloud are a
  turn's. Its stored record (the synthetic call) makes no tool cards.
- **The strip** lists the running jobs and the ended ones still on their way in, with a head
  that counts both. A row shows tool, label, status (`input_required` as "needs input"), the
  status message (wrapped, never cut), "since <local time> · <age>", `waiting_for`, and in its
  title the job id, the starter and the stated ttl. Cancel is on running rows only: the
  two-click confirm "Cancel <tool>?" (it disarms by itself after 6 s), then the route; the
  answer's `note` is a toast, a refusal is worded by code (unsupported, refused, ended, not
  found: a warning, the job goes on). With many jobs (no cap, T9) the strip scrolls inside
  35vh, so the transcript keeps its room; the head still counts all. *Taken in this draft.*
- **Freshness.** A status move is not in the feed (§4.1), so while a job runs the strip reads
  the thread's tasks again every `mcp.task_poll_interval_s` (read from `/api/settings-full`
  once a job runs), only while the window is visible, and its head says "as of <time> · read
  again every N s (Settings → MCP → Task poll interval)", or that the interval could not be
  read and it reads only on the feed. A failed read of the interval is tried again with the
  next read of the tasks and when a job runs again. The read is the thin `GET
  /chat/api/threads/{id}/tasks` (a `Vec<ThreadTask>`, the `tasks` of `GET …/threads/{id}`
  without the history; the same reach rules, a thread out of reach is `404 not_found`;
  `lmgw-client::thread_tasks`, a DocRoute in the chat_tasks plane), added after review: the
  first build read the whole thread each interval. *Taken in this draft.*
- **Rows keep their identity.** The strip's rows are keyed by the task's id and read their
  task from the list by it, so a poll that changes the status, the message or `waiting_for`
  changes the text and keeps the row, and an armed Cancel in it (the drive arms Cancel and
  waits for a poll that moved the job's message).
- **Answer** sits beside Send while a stored result has no reply after it, or an ended job
  nothing holds off waits to enter (the route's own test); it waits while a reply streams (Cancel does not: it is a request to the job's server). It
  streams into a new reply; a refusal before any frame takes the reply back and is worded
  above the composer (amber, dismissable, gone when another thread opens or no result waits for an answer any
  more, except "nothing to answer", which says why nothing happened): `turn_running`
  ("being written elsewhere … Answer once it is done, if it did not") and `nothing_to_answer`
  have their own words. In voice mode it is hidden with the composer: the bound session's
  bare `response.create` is that continuation (WP3).
- **Not built:** the live card of the call that started a job does not point to the job (the
  `tool` frame's `task` is not read by the page); the card's "started, job …" says it.
- **Checked:** `scripts/chat-tasks-drive.sh --webkit` (dev instance, mock model, `builder` mock
  server): a running strip, a completed result answered, a failed result, a confirmed cancel
  and its cancelled result, an Answer refused `turn_running` while another window answers,
  Settings → MCP's field, the MCP page's "1 open task", then a light-theme pass at a 2 s poll
  (strip, result card) in which an armed Cancel survives a poll that moved the job's message;
  Answer is as tall as Send, and a running row has the neutral border with a 3px left accent
  like the result cards (not a focus ring); Chrome at 125% plus a WebKitGTK pass of
  /chat and /mcp-servers on Broadway.

*As built (WP5, acceptance), 2026-10-09:* the live checks of §7 and an as-built pass of this
record against `main` (be1e996c).
- **The rig.** `scripts/tasks-live-check.sh` starts a dev instance (`scripts/dev-instance.sh`,
  scratch data dir, 127.0.0.1:8917), `scripts/mock-openai.py` as the chat model (`TOOL_SCRIPT`: one
  job-starting turn per check, every later request answered by echoing its trailing tool result)
  and as the voice (an OpenAI-shaped TTS alias on its `/audio/speech`), and a **fake paired
  device**: `scripts/mock-mcp.py --tasks --device ws://…/mcp/host` with a device key hosting the
  label `builder`, offering the three `required` tools of the stdio mock over the host link
  (2025-11-25 Tasks, `pollInterval` 1 s, status notifications); SIGUSR1 shuts its socket and it
  links again after `DROP_SECONDS`, keeping its tasks. `scripts/tasks-live-check.py` drives the
  checks through the dashboard's routes and a bound `/v1/realtime?chat_thread=` session and writes
  the facts to `target/tasks-live-check.json`. `--ui` then runs `scripts/chat-tasks-drive.sh
  --device`: WP4's dashboard drive against the same kind of device. No GPU, model or cloud call.
- **Results** (all pass; seconds from the send unless named):
  1. *Dashboard send.* The send's stream ended 0.11 s after it began, with `started, job t1`;
     the job (6 s) entered the thread as a result row at 6.08 s; Answer streamed its reply in
     0.14 s ("job t1 (builder__build) completed …"); the stored roles `user, assistant, tool,
     assistant`; a second Answer `409 nothing_to_answer`. A second job (4 s) entered at 4.16 s
     and the next send answered it (0.15 s; a reply stored after the result, Answer then `409
     nothing_to_answer`). The check records where the request put the result and passes with
     either order: on this build the pair came after the new message (the reply was the
     result's echo), which the owner's T11 ruling changes to chronological order. Re-run once T11
     was built (2026-10-09, all four pass): `placement: chronological (the new message last)`,
     the next send's reply the echo of the user's message ("Echo: what came of it?", 0.07 s),
     the result before it as context. In the browser (`--ui`, Chrome at 125%): the strip,
     the completed card, Answer, the failed card, a refused Answer and the MCP page's count, all
     with the device's tools (0 FAIL).
  2. *Bound voice session* (audio out, manual turns, bound while the job ran). `lmgw.task.done`
     came 5.07 s after the send (a 5 s job) with `{id, task_id, server_label: "builder", tool:
     "builder__build", status: "completed"}`; a bare `response.create` gave its first audio
     delta after 0.08 s and `response.done {completed}` after 1.08 s, the spoken transcript the
     result ("job t3, builder__build, completed. built 42 files in 5 s."), no `lmgw.chat.user`;
     the reply stored with `voice.via: realtime`.
  3. *Link dropped.* The device's link was cut 0.1 s into a 6 s job and opened again 10.06 s
     later. Meanwhile the row read `stopped`, the thread's task said `waiting_for` "device
     'builder-pc', which is not connected (it is asked again when it links)", its status stayed
     `working`, and no result entered although the job had ended on the device. The result
     entered 0.06 s after the row read ready again (the link's first poll).
  4. *The owner's cancel.* The cancel route (the strip's Cancel) answered in 0.02 s, `delivered:
     true`, "the server cancelled the job; its result is in the thread"; the result row (`job t5
     (builder__index) cancelled`, "cancelled by the dashboard") was there at once, the strip
     empty, and Answer answered it. In the browser: the two-click confirm naming the tool, then
     the cancelled card, and an armed Cancel that survives a poll (0 FAIL).
- **No bug found.** One rough edge in the rig, not in lmgw: the device's status reads
  `stopped` while its link is down (the MCP page's word for a device row with no link).
- **As-built pass.** The design text carries *Amended* notes where the build differs (T15's
  `thread_id`, the 0075 migration, `TurnMode::Answer` for both continuations, the owed set read
  from the thread, `lmgw.task.done`'s `by`, the routes' answers and the thin task read, the
  start-of-turn delivery); T9 and T11's placement are the owner's decisions (the latter built
  since, §3.2), so no question is open.
  The `ir::SYNTHETIC_CALL_ID_PREFIX` comment no longer says the Gemini skip value is keyed on
  it (gateway design §7.1).

## Open questions (for the owner)

None open.

### Answered

- **Where a result renders when the user wrote after it (T11, §3.2).** Raised by the client's
  WP18 live check and seen in WP5's check 1: with the pair placed after a new user message
  (`U1 A1 U2 [call R] R`), the request ends in the tool result and the model answers the job
  instead of the message. *Decided by the owner, 2026-10-09:* chronological order (T11). *Built 2026-10-09:* T11 and §3.2 as decided, the
  request goldens and the strict-template checks with it.
- **Gemini thought signatures on real calls.** *Resolved 2026-10-09, gateway design §7.1.* The
  Gemini egress captures a model's `thoughtSignature` into the id of the call it signed. Every
  client shape and the Chat's stored records echo that id, so the signature goes back on its
  part. Any step's first call without one gets the skip value. Still not verified live: that a
  model before Gemini 3 takes the skip value.
- **A task followed forever (T9).** A task whose receiver states no `ttl`, or that stays in
  `input_required` because nothing ever answers it, is polled until the receiver ends it or its
  server row is removed; lmgw ends nothing by its own clock (T7). *Decided by the owner, 2026-10-09:* kept as designed.

- **The client-contract changes (M4 table).** Points (1) to (5) and the `_meta["lmgw/task"]` /
  bridge row. *Accepted by the owner 2026-10-09,* as built.

## Later

- **The southbound move to 2026-07-28** and its Tasks extension. The owner decided 2026-10-09 it
  is a **near-future item**. rmcp 3.5.1 speaks the stateless revision, so what remains is the
  rmcp 2 → 3 upgrade (lmgw and Kai), a stateless redesign of the device host link (today built on
  `initialize`: its capabilities and `lmgw/host_limits`), and speaking both revisions while
  third-party servers stay on 2025-11-25 (§1.9 is the map; its own design, for every row and the
  host link at once).
- **Tasks on `/mcp`**: lmgw as a receiver, so a `/mcp` client gets a task instead of a bridged
  wait.
- **Late results for `/v1/responses`** (its `background` mode) and unbound realtime sessions.
- **Elicitation** for `input_required` tasks (client-apps Later).
- A self-admin tool to list and cancel open tasks.

## Rejected

- **Pinning 2026-07-28's extension now.** It needs the stateless revision (no `initialize`), which
  the host link does not speak and lmgw's rmcp 2.0.0 does not offer (rmcp 3.5.1 does; the move is
  Later, a near-future item), and the extension is undefined under 2025-11-25.
- **A namespaced lmgw-only task dialect** (`lmgw/tasks/*` on 2025-11-25 sessions). The revision
  both ends negotiate defines Tasks; a dialect would be lmgw-only where the protocol has the
  feature, and no registered server could speak it.
- **Rewriting the `started, job …` result in place.** It changes what earlier turns saw, breaks
  every prompt-cache prefix from there, and leaves later replies reading as if they ignored a
  result they never had.
- **A late tool message with the original call id.** Strict templates and OpenAI-shaped upstreams
  require a tool result directly after its call.
- **The result as a user message.** The model would take it as the user's words, and the contract
  asks for a tool result.
- **lmgw starting a turn on a result.** Contract (5); the client picks the moment.
- **Ending a task on a link drop.** T7.
- **Holding `tasks/result` open from the start instead of polling.** An HTTP server would hold a
  stream for hours; polling plus notifications is what the spec asks. Held only for
  `input_required` (T19).
- **Calling `optional` tools as tasks.** T3.
- **A cap on open tasks, a requested `ttl`, a retention setting for results.** T9.
