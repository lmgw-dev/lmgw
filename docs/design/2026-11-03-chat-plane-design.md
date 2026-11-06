# The Chat on the Chat plane

*2026-11-03. Built. WP2 of the Kai for Firefox design (the kai-firefox repository,
`docs/design/2026-11-03-kai-firefox-design.md`, §8.2 item 2 and §12). The calls marked D1…D17 are
draft calls. A review round revised D1, D8, D11 and D12 and added D13–D17. WP3a (§8,
the same design's §8.2 items 1, 4, 5 and 6) adds the transport seam and D18–D27; its review round
revised six of them and added D28–D30. WP3b (§9, item 3) moves the Chat into its own crate,
`lmgw-chat-ui`, with the host's seams and a narrow layout, and adds D31–D42. §9.10 adds the
narrow head's New chat, New chat in the open thread's folder, the way back to an ongoing
conversation and a message's origin, with D45–D55.*

The Chat will be reused in a Firefox sidebar, which reaches lmgw as a paired device: Kai's device
certificate, through Kai's proxy. A device holds `Chat`, `Inference` and `Transfer`, never `Admin`,
and the device listener does not serve `/api` at all (device link design §4.6). Before this work
the dashboard's Chat read the owner's admin plane in fourteen places. Now its code reads only
`/chat/api/*` and `/v1/*`, so the owner's dashboard and every device client use one set of routes.
Its live updates come from a source its host hands it (D1): the dashboard hands it the shell's own
stream, and a host without one gets the Chat's feed. The owner sees no change beyond the two noted
in D9 and D16.

What a device sees is lmgw's existing rule (client-apps design L3, L4, L5, §5.6), which decision 10
of the Kai for Firefox design accepts for the browser. Every new read follows the same rule: a
device learns nothing through it that it could not learn before.

## 1. The call sites

The Chat's code is `crates/lmgw-ui/src/pages/chat.rs`, `pages/chat/*`, `pages/chat_*.rs` with their
child modules, the widgets the Chat renders (`widgets/tool_picker.rs`'s `McpPicker`,
`pages/knowledge_source.rs`'s source viewer), the kit pieces it uses (`lmgw-ui-kit`'s model catalog,
voice picker and profile API), and the live stream (`live.rs`). Before WP2 they named 86 route call
sites over 60 routes:

### 1.1 The admin plane: 14 call sites, 9 routes

| Route | Call sites | What the Chat read | Now |
|---|---|---|---|
| `GET /api/events` (SSE) | `live.rs` (the shell's one stream), read by `chat_sync.rs` (`chat` frames) and `chat_voice/page.rs` (`vram` frames) | which threads, folders and messages changed; the GPU hold and a benchmark's lease | the source the host hands the Chat (`pages/chat_live.rs`): the dashboard's is this stream; a host without it gets `GET /chat/api/feed` (§2.5, D1, D6, D11) |
| `GET /api/settings-full` | `chat_knowledge.rs` (KB budget), `chat_folders/ongoing.rs` (archive and purge days), `chat_voice/section.rs` (TTS aliases), `chat_voice/dictation/upload.rs` (`max_body_mb`), `chat_tasks.rs` (task poll interval), `chat_voice/realtime/live.rs` (pre-roll, ping interval) | nine settings | `GET /chat/api/settings` (§2.1, D3) |
| `GET /api/knowledge/bases` | `chat_knowledge.rs` | the bases for the Knowledge section, `#` picker and chips | `GET /chat/api/knowledge/bases` (§2.3, D7) |
| `GET /api/knowledge/files/{id}/text` | `knowledge_source.rs` (the citation viewer) | a file's text and chunk spans | `GET /chat/api/knowledge/files/{id}/text` (§2.3, D8, D15) |
| `GET /api/knowledge/files/{id}/original` | `knowledge_source.rs` | the original's download | `GET /chat/api/knowledge/files/{id}/original`, the owner's (§2.3, D14) |
| `GET /api/mcp-servers` | `widgets/tool_picker.rs` | enabled servers, label, status, tool count (the answer also carries every server's env and headers, redacted) | `GET /chat/api/tools` (§2.2, D4) |
| `GET /api/tools` | `widgets/tool_picker.rs` | the built-in toolsets and their tools (the answer is the whole inventory and connects every enabled server) | `GET /chat/api/tools` (§2.2, D4) |
| `GET /api/mcp-servers/{id}/tools` | `widgets/tool_picker.rs` | an attached server's tools | `GET /chat/api/tools/{label}` (§2.2, D16) |
| `GET /chat/api/threads/rows` | `chat_sync/rows.rs` | the rows of threads whose messages changed | the same route, `Cap::Chat` (§2.4, D2) |

The design's survey counted `/api/settings-full` in seven places. The seventh,
`gpu_download.rs`'s `GpuPulls`, is the dashboard's titlebar and is not rendered by the Chat (the
Chat uses only `Dl` and `DlBar` from that module, which read no route).

### 1.2 The inference plane: 3 call sites, 3 routes

`GET /v1/models` (the kit's model catalog: the model pickers, a model's capabilities),
`GET /v1/audio/voices?model=` (the kit's voice picker), and the WebSocket
`GET /v1/realtime?chat_thread=<id>` (`chat_voice/realtime/socket.rs`, its URL from
`lmgw_client::requests::realtime_page_url`). A device reaches all three.

### 1.3 The Chat plane: 69 call sites, 48 routes

Threads (list, create, read, settings, delete, pin, title, archive, persist, move, export), turns
(send, continue, approvals, answer, the message actions delete, edit, regenerate and speak, speech
stop, transcribe, voice warm-up), tasks (list, cancel), media (cancel, an attachment's action, the
defaults), attachments (upload, read, delete, text, transcribe, mode), folders (create, change,
delete, current, export), the all-threads export, search, and the kit's eight profile routes. Every
one was `Cap::Chat` already; none changed.

### 1.4 Beyond the survey

- **The citation viewer** read `/api/knowledge/files/{id}/text` and `…/original`.
- **`/chat/api/threads/rows`** sits on the Chat path but was `Cap::Admin`, so it is not on the
  device listener's face.
- **`/api/events` carried two things for the Chat**, not one: the `chat` frames, and the `vram`
  frame, whose hold and benchmark lease the voice controls show as the amber chip.
- **Links to dashboard pages** are not routes, but the sidebar will meet them: "Add a model"
  (`/models`) and "Add a cloud provider" (`/upstreams`) in `chat.rs`, "Open the Audio lab"
  (`/audio-lab`) and "Open Engines" (`/backends`) in `chat_voice/state.rs`, and the empty-state
  texts "create one on the Knowledge page", "register one on the MCP page".
- **Desktop shell calls**: `crate::ui_scale::tauri_invoke` (audio devices, window), which a browser
  sidebar does not have.

After WP2 the Chat's code names no `/api` route. Its live updates come from the source its host
hands it (`pages/chat_live.rs`, D1): the dashboard hands it the shell's `/api/events`, and a host
that hands none gets `/chat/api/feed`.

## 2. The Chat plane's reads

Every route is `Cap::Chat`, documented in the API description (`openapi/planes/chat_reads.rs`),
typed (`lmgw_api_types::chat_reads`), and served by `crates/lmgw-core/src/web/chat_reads/*`. Each
handler takes the Chat's `Caller`, which fails closed: no principal is a 500, an anonymous one a
401. Every refusal is decided before anything is read, so an id cannot be probed.

### 2.1 Settings: `GET /chat/api/settings`

`ChatSettings`: `kb_budget_tokens`, `archive_days`, `purge_days`, `tts_alias`, `realtime_tts_alias`,
`max_body_mb`, `task_poll_interval_s`, `realtime_prefix_padding_ms`, `realtime_ping_interval_s`.
Exactly the nine values the Chat reads, none of them a secret, the same for every caller (D3).

### 2.2 Tools: `GET /chat/api/tools`, `GET /chat/api/tools/{label}`

The list is the caller's tool scope applied to lmgw's own label listing (`mcp::exec::labels`,
what `GET /v1/mcp/servers` answers): the built-in toolsets first (lmgw, docs, kb, image, audio),
each carrying its tools as `mcp::exec::list_label` resolves them in-process, then the enabled
servers and service agents by label, each with its connection's status and the number of its tools
the caller may tick. It connects nothing. The detail is `list_label_unstarted` for one label, the
resolution a turn runs without the step that starts a service agent: it connects that server when it
is not up, and an agent whose app is not running is the `502` saying so (D16). Tool names are the
exposed names, the spelling a thread's `allowed_tools` and `require_approval` already store.

Scoping (D4, D5):

| Caller | Built-in toolsets | Servers |
|---|---|---|
| the owner (cookie or owner key; the gateway's scope) | all five; one that offers nothing has `status: unavailable` and the reason | every enabled server |
| a device | those that offer it tools under its scope; `lmgw` only while its capped admin level is `full` | those `/v1/mcp/servers` lists it: enabled, reached under its scope and its device-tools grant (L16) |

A tool the owner switched off is never listed or counted. A label the caller may not attach is the
detail's `404 not_found`; a server that cannot be listed its `502 mcp_list_tools_failed`.

### 2.3 Knowledge: `GET /chat/api/knowledge/bases`, `…/files/{id}/text`, `…/files/{id}/original`

`ChatKnowledgeBases`: `{id, name, files, ready, embed_resolvable}` per base. These are the fields the
Chat shows, without the pin, the models, the upstream or the sizes the Knowledge page's answer
carries. The text route answers `ChatKnowledgeSource` (D15): the text, the chunk spans, the
highlight and its notice as the Knowledge page's read has them, the file as the viewer shows it
(`id`, `name`, `kind`, `status`, `pages`), and `original`, whether the caller may download the
original. The original route answers the owner what the Knowledge page's does, through the same
handler.

A device reads the bases only with `kb__search` within its tool scope (D7, D13), and a file's text
only with `kb__read` too, switched on by the owner (D8). Otherwise the answer is
`403 tool_label_out_of_scope`, naming the tool and never a base, which is what a device's write of a
base already gets. A device's original is `403 forbidden` whatever its scope (D14), and its viewer
shows no download. Each refusal is said before the file is looked up. The owner is never refused.

### 2.4 Thread rows: `GET /chat/api/threads/rows`

Unchanged but for its capability (D2). It answered through `ChatRepo::stored_rows`, which applies the
caller's reach, so a device's answer leaves out what L3 hides, as its list does.

### 2.5 The live updates: the host's source

The Chat takes its live updates from a source its host hands it in context
(`crates/lmgw-ui/src/pages/chat_live.rs`, `ChatLive`, D1). Either source gives the page the same two
things (`ChatUpdates`): what changed, merged in one inbox (`live/chat.rs`) that the follower
(`chat_sync`) takes and reads again, and the GPU hold with a benchmark's lease for the voice
controls.

- **`ChatLive::Shell`**, the dashboard's: the shell's one stream, `/api/events`. Its `chat` frames
  go into the inbox (D11), and its `vram` frame's `hold_active` and `benchmark` are the hold. The
  page opens no connection of its own.
- **`ChatLive::Feed`**, for a host that hands none (the sidebar, WP3): the Chat's change feed,
  `GET /chat/api/feed`. Each event maps to the names it reads again (`chat_live/said.rs`): thread
  events and task events name a thread, folder events (`folder.current` included) a folder, and
  message events the thread whose messages changed. `hello` and `resync` mean read everything
  again, and an event that does not read is a resync too. The feed's hold (`hello.hold`, `hold`,
  every `state`) gains `benchmark` (D6), published when a benchmark run takes or gives back the
  graphics card (`AppState::set_gpu_lease`). The stream opens when the page mounts and the session
  is good, and closes at once when the page goes or the session locks (a `oneshot` whose sender the
  cleanup drops). A login opens it again. Since WP3a it is read off the page's transport, not an
  `EventSource`, and carries its cursor itself (§8.3): the next connection sends the last `id:` as
  `Last-Event-ID`, so the catch-up after a reconnect names what changed while it was down. The
  feed's L3 rules (reach moves, the catch-up's narrower reach) apply to whoever follows it.

A hold not known yet reads differently by source (D17). The shell's stream said it before the page
mounted, so not knowing it means that stream is down, and the voice controls say what the hold
would do, as before. The feed says it in its first event, so not knowing it is the moment before
that, and nothing is said.

## 3. What a device sees, and where it is enforced

| Rule (Kai for Firefox decision 10; client-apps L3–L5) | Enforced | New here |
|---|---|---|
| Admin Chat and folders a device deleted are hidden | every route by id (404), the lists, counts, search, exports, the feed (`Caller::sees`, `AdminThreads`) | the rows route for devices, through the same reach |
| Threads with the self-admin toolset appear at the device's admin level | the reach level everywhere above; attaching `lmgw` needs `full` (L5's note) | the picker offers `lmgw` only at `full` (D5) |
| Its alias scope decides the models it may pick | writes (`check_aliases`: a thread's and a folder default's model, ASR and TTS), every call (`policy_checked_call`), the media defaults' choices | not the model picker's list (D10) |
| Writing a folder's retention is refused | `retention_is_owners` (`403 forbidden`), and showing a folder to devices again | nothing |
| Knowledge bases need `kb__search` in its tool scope | writes (`check_kbs`), a turn's retrieval (`kb_reach`) | the list (D7, D13) and the viewer (D8, with `kb__read` and its switch; D15, without the ingest record); the original is the owner's (D14) |

## 4. Tests

- `tests/it/device_chat/plane.rs`: the settings are the same for the owner and a device and are
  exactly the nine; the bases need `kb__search` (refused under `docs__*`, listed under `kb__search`,
  `all` and for the owner, a base hidden from `/mcp` included, no admin-only field); a file's text
  needs `kb__read` too and a device's original is refused, a file that exists and one that does not
  getting the same refusal; a device with both tools reads an ingested file's text and its cited
  chunk without the ingest error or notes and with `original: false`, the owner the same text with
  `original: true` and the original's bytes; `kb__read` switched off refuses a device's text (the
  owner still reads) while the bases stay listed with `kb__search` off too; a device's rows leave
  out an Admin Chat thread and a self-admin-toolset thread that the owner's include; a row in a
  folder whose defaults take the toolset comes without its folder for a plain device, and a device
  at `read_only` reads that folder and the toolset thread's row, never Admin Chat's.
- `tests/it/device_chat/plane_tools.rs`: the owner's list (all five toolsets in order, an
  unavailable one with its reason, both servers); a device's list equals the labels
  `/v1/mcp/servers` gives it, unscoped and under an allow list, with out-of-scope labels a 404 on the
  detail; `lmgw` at `read_only` neither listed nor detailed, at `full` both; a switched-off tool is
  neither listed nor counted; a connected server's count is 3 for the owner and 2 under an allow
  list naming two of its tools, one less each once the owner switches one off (D9); another
  device's hosted label is listed and detailed to a device with its device-tools grant, to the
  hosting device and to the owner, and is a 404 without the grant (L16).
- `tests/it/agents_service/picker.rs`: the picker's list shows a sleeping service agent as stopped and its
  detail says it is not running, starting no container; once a turn's resolution started it, the
  detail lists its tools and starts nothing more (D16).
- `tests/it/chat_feed/live.rs`: a benchmark's lease is a `hold` event for the owner and a device and
  is in a new feed's `hello`; the existing hold assertions carry `benchmark`.
- `tests/it/chat_live_reads.rs`: a device reads the rows route. `tests/it/device_tls.rs`: the rows
  route is no longer off the device face.
- Unit tests: `web/chat_reads/settings.rs` (each field is its setting), lmgw-api-types'
  `chat_reads.rs` (the Chat's source keeps no ingest record), lmgw-ui's `chat_live/said.rs` (the
  feed's events to names and hold), `chat_live.rs` (the shell's `vram` frame to the hold),
  `chat_voice/state.rs` (a hold not known is said for the shell and not for the feed, D17),
  `knowledge_source.rs` (both planes' paths).
- The CAPABILITY_TABLE walks (`route_walk`, `sandbox_walk`, `device_face`) and the OpenAPI coverage
  tests pick the new routes up from the table.

## 5. Draft calls

- **D1. The Chat takes its live updates from the source its host hands it.** The dashboard hands it
  the shell's `/api/events` (`ChatLive::Shell`): its `chat` frames and its `vram` frame's hold, as
  the page read them before WP2. lmgw serves plain HTTP/1.1 on loopback, where a browser keeps at
  most six connections to one host, so a stream of the page's own beside the shell's would let three
  Chat tabs take them all and leave a fourth tab's reads hanging. A host that hands nothing gets
  the Chat's feed, `/chat/api/feed` (`ChatLive::Feed`), which already scopes every event per
  reader, reach moves and the catch-up's narrower reach included, so a second chat-plane stream of
  ids only would have had to repeat that. The sidebar will be such a host (WP3). The source is an
  enum in context, not yet the transport seam WP3 builds.
- **D2. `/chat/api/threads/rows` becomes `Cap::Chat`.** It already answered through the caller's
  reach. It moves into the developer API document.
- **D3. `/chat/api/settings` is the nine values the Chat reads, the same for every caller.** No
  per-principal variant: none of them is a secret, and a device that writes outside them is refused
  where it writes.
- **D4. The tool list is `/v1/mcp/servers`'s listing, shaped for the picker.** A device is listed
  what it may use now. The owner is also listed a toolset that offers nothing, with the reason, as
  the inventory listed it before.
- **D5. `lmgw` is offered to a device only at `full`.** Below it the device uses the threads that
  carry the toolset but attaches it to none (L5's note), so the picker leaves it out and the detail
  answers 404. `/v1/mcp/servers` still lists it at `read_only`, because a realtime session may use
  it there.
- **D6. The feed's hold carries `benchmark`.** It is the second thing that holds a local voice
  model, and the voice controls read it from `/api/events`' `vram` frame, which a host on the feed
  does not have. A device learns nothing new from it: a call it makes during a benchmark is refused
  with `gpu_benchmark`.
- **D7. A device with `kb__search` sees every base.** A thread's base selection ignores a base's
  `mcp_visible` switch (chat-complete design §9.4), and a device may already name any base id there.
- **D8. The citation viewer needs `kb__read` besides `kb__search`, and `kb__read` switched on.**
  With `kb__search` alone a device receives excerpts, never a whole file; `kb__read` is the tool
  that reads one, and the only way a device read a whole file before. A tool the owner switched off
  is dropped from every resolution (`resolve_builtin`), so a device whose `kb__read` is off reads no
  whole file through the viewer either. The refusal is the scope's `403`, said before the file is
  looked up.
- **D9. A server's tool count is what the caller may tick.** It used to be the connection's full
  count. The owner sees a lower badge for a server with switched-off tools.
- **D10. The model picker keeps `/v1/models`.** That list is every exposed model for every key, as
  before. The alias scope is enforced at the write and the call, so a device that picks a model
  outside it is refused on save (`403 key_scope`). The pickers are the kit's, and the catalog's
  source moves with WP3's transport seam, so a scoped list belongs there.
- **D11. `/api/events` keeps its `chat` frame.** The dashboard hands it to its Chat as the live
  updates (D1), and the MCP page refreshes its servers' open-task counts on it.
- **D12. The source viewer takes its plane as a prop.** The Knowledge page keeps `/api/knowledge`
  and the Chat passes `/chat/api/knowledge` (`knowledge_source::SourcePlane::Chat`). The component
  is shared, so one viewer serves both. It shows either answer in the Chat's shape (D15), the
  Knowledge page's with the original always offered.
- **D13. The bases list does not read the owner's switch on `kb__search`.** A thread's base is
  searched by the Chat's own retrieval, not by the tool: neither the check of a device's base write
  (`check_kbs`) nor a turn's retrieval (`kb_reach`) reads the switch, and the list is their picker.
  If the switch should bind the Chat's retrieval, all three change together.
- **D14. A device gets no original download yet; a separate grant is the owner's decision.** The
  original hands out the uploaded bytes (a PDF's images, a document's metadata and revisions),
  more than any `kb` tool does, and a device reaches the bases as the `kb` toolset, never around it
  (client-apps design L5, review W2-4). A device's `…/original` is `403 forbidden`, said before the
  file is looked up, and the viewer offers it no download.
- **D15. The viewer's answer is Chat-shaped.** `ChatKnowledgeSource` carries the text, the spans,
  the highlight and the file's name, kind, status and pages, for every caller. The Knowledge page's
  answer also carries the ingest record (`error`, `notes`, size, digest), which can name upstreams
  and models. Shaping it for everyone rather than blanking it for devices keeps one answer per
  route, as `ChatKnowledgeBase` does.
- **D16. The picker's detail starts no service agent.** Opening a thread's tool settings is not the
  ask that runs an agent's container (container-runtime §3.3); a turn that attaches the label is.
  The detail lists a running agent's tools and answers one whose app is not running with
  `502 mcp_list_tools_failed`, saying it is not running and that a turn starts it, where
  `/api/mcp-servers/{id}/tools` answered no tools. `/v1/mcp/servers/{label}` keeps starting it.
- **D17. A hold not known is said only for the shell's source.** The shell's stream has said the
  hold before the Chat mounts, so not knowing it means that stream is down, and the voice controls
  say what the hold would do, as before WP2. The feed says it in its first event, so not knowing it
  is the moment before that, when the note would be wrong more often than right; nothing is said
  then.

## 6. For WP3

- **No `/api` in the Chat's code.** The crate extraction can admit `/chat/api/*` plus `/v1/models`,
  `/v1/audio/voices` and `/v1/realtime`, and nothing else. The shell's `/api/events` stays the
  dashboard's: it reaches the Chat only as the source the dashboard hands it (`ChatLive::Shell`,
  D1), which the crate does not name.
- **The live source is the seam's first piece.** `ChatLive` is an enum in context with two
  sources; the crate needs it as a seam its host fills (the dashboard with the shell's stream, the
  sidebar with the feed through Kai). Each source gives an inbox of `ChatChanged` and the hold, and
  says whether a hold not yet known is said (D17).
- **The feed through the transport seam.** Built in WP3a (§8.3): the feed source reads
  `/chat/api/feed` off the page's transport and carries the cursor itself. Closing is immediate: the
  page drops a `oneshot` sender in its cleanup, which aborts the open request. The open/close is
  gated on the dashboard's session signal (`crate::session::locked_signal`); the sidebar's
  equivalent is its link to Kai. One stream per sidebar page is one connection per page, which the
  sidebar's host has to budget for as the dashboard's tabs had to (D1).
- **Kai's own feed and the sidebar's.** Both are Kai's device key. The feed counts each as a link of
  the device and neither ends the other. Kai's voice marks the sidebar's turns by `by.own`
  (kai-firefox §8.3).
- **What the crate takes along:** `chat_live` (the source, the feed and its event mapping) with the
  inbox (`live/chat.rs`), the six settings readers (all on `ChatSettings`), `chat_knowledge` with
  `ChatKnowledgeBase`, the source viewer with `SourcePlane::Chat` and `ChatKnowledgeSource`, and
  `McpPicker` with its `ChatToolList`/`ChatToolDetail` reads. The key editor's `ScopeToolPicker`
  stays in the dashboard on `/api/tools`.
- **Dashboard-only pieces the crate must replace or drop:** the links listed in §1.4,
  `ui_scale::tauri_invoke`, `crate::session`, the toasts, the dirty guard, and `ChatLive::Shell`
  with the `LiveBus` it carries.
- **The model list.** The pickers still read `/v1/models` unscoped (D10). If the sidebar should hide
  models outside Kai's alias scope, the kit's catalog needs a scope, or a chat-plane model read.
- **Duplicated reads** (feed source): the feed already carries the thread row (`thread.*`) and the
  messages (`message.*`) the follower then fetches. Taking them from the event would save a read
  per change; the follower's "newest read wins" rules (`OwnEdits`, `Latest`) would have to cover
  event-applied state too.

## 7. Not built yet

- A scoped model list for the pickers (D10).
- Removing the duplicate reads by applying the feed's rendered rows and messages (§6).
- What §9.8 lists for WP8.
- A device grant for knowledge originals (D14): the owner's decision.
- A way to start a sleeping service agent from the picker on purpose (D16): the picker says a turn
  starts it.

## 8. WP3a: the transport seam

*Built. The Kai for Firefox design's §8.2 items 1, 4, 5 and 6. The crate move (`lmgw-chat-ui`, no
router, the narrow layout) is WP3b. A review round (no blocker, no high) revised D18, D20, D21, D23,
D24 and D25 and added D28–D30.*

In the sidebar every HTTP request goes as a `req` frame through the extension's background to Kai's
proxy, and an extension page allows only `'self'` scripts plus `'wasm-unsafe-eval'`. WP3a makes the
Chat's I/O host-agnostic: its host hands it a transport, and its pages run without inline script.
The owner's dashboard looks and behaves as before.

### 8.1 The transport

`lmgw-ui-kit`'s HTTP client (`http.rs` with `http/transport.rs`, `http/fetch.rs`, `http/fake.rs`,
`http/sse.rs`, `http/bytes.rs`):

- **`Request`** is plain data: `method`, `path` (relative to lmgw, query included), `headers`,
  `body` and `read`, plus the browser's `AbortSignal` (D19). `Body` is `Empty`, `Json` (text sent
  as `application/json`), `Bytes`, or `Blob` (a picked or dropped file, D19). `Read` says how the
  caller reads the answer: `Whole`, `Stream` (an SSE stream, open as long as lmgw sends) or `Blob`,
  so a relaying host knows what to buffer and what to relay.
- **`Response`** is `status`, `headers` (lower case) and a `Payload`: `Whole(bytes)`,
  `Stream(chunks)` or `Blob`. Every reader (`bytes`, `text`, `stream`, `blob`) takes any payload, so
  a host may answer a `Read::Blob` with whole bytes and a `Read::Stream` with one chunk.
- **`Transport`** is the trait: `send(Request) -> Response` (an `Err` is "no answer", a refusal is a
  `Response`), and `link(path) -> Option<String>`, a URL the browser may load by itself with the
  same credentials (D20). Its cancel contract is written on the trait and on `ByteStream`: a
  transport stops a request when its `AbortSignal` aborts (no answer yet: `send` resolves `Err`;
  streaming: the stream ends with an `Err`), and dropping the `send` future or the answer's stream
  cancels it too (the browser's fetch cancels the body's reader; a relay sends its cancel frame).
  The error's words are the transport's own ("AbortError: …", "cancelled"): a caller tells a stop
  from a failure by `signal.aborted()`, never by the text (D28).
- **`Http`** is a `Copy` handle to a registered transport, handed to the page in context and read
  with `use_http()` where the page is set up (D18). Its helpers are the free functions' (`get`,
  `get_text`, `post`, `post_no_content`, `post_bytes`, plus `post_body`, `blob`, `ok`, `send`),
  refusals and the `401 session_required` hook included. `Http::fetch()` is the browser's `fetch`
  under the configured base: the dashboard's, and what a host that hands none gets. A debug build's
  `use_http()` called without a reactive owner, where it can only answer `fetch`, logs a console
  error and counts the call on `<html data-use-http-orphans>`, which the relay drive checks (D18).
- **`Fake`** (`http/fake.rs`) answers from a function of the request and keeps every request; a
  streamed answer is its chunks as given.
- **`sse::Decoder`** reads SSE records (`event`, `data`, `id`, `retry`) off byte chunks, a record
  split across chunks included; the send stream, the feed and the Audio lab's streams share it.
  Each push scans only the bytes it has not scanned (a terminator split across chunks included)
  and moves the finished records out of its buffer at once, so a frame of megabytes in small
  chunks and a catch-up of thousands of records in one chunk cost what their bytes do.
- **`bytes::use_src` and `bytes::download`** show and save lmgw's bytes: the transport's link when
  it has one, else an object URL of the bytes (D20, D22). `use_src` says `None`, `Loading`,
  `Ready(url)` or `Failed(reason)`. `download` with a link clicks the `<a download>` at once, inside
  the caller's click handler, as the dashboard's own link did; only the bytes path waits for the
  transport, and its failure goes to the caller's `on_error`. `bytes::click_download` is the one
  `<a download>` click (the dashboard's other pages' `docs::download` is it).

The kit's free functions stay the browser's `fetch`: the dashboard's other pages and Kai's settings
window use them as before. `post_raw` (a `File`, a `FormData` or a string as the body) goes through
the same fetch transport as they do; `checked` still takes a `gloo_net` response, because the Audio
lab builds its run's request with `gloo_net` and reads the answer as audio, JSON or a stream, which
is a larger move than this round's. The kit pieces the Chat renders take the handed transport: the model
catalog (captured when `provide_model_catalog` runs), the voice picker and the profile editor
(`profiles::api` takes an `Http`).

### 8.2 The moved call sites

Nineteen places bypassed the kit, all moved:

| Where | What it did | Now |
|---|---|---|
| `chat_stream.rs` `post_sse` | POST + SSE over a fetch body: a turn's send, edit, regenerate, continue and answer (`chat_turn`), an image card's action (`chat_media/actions.rs`), read-aloud (`chat_voice/read_aloud.rs`), dictation's warm-up | `Http::send` with `Read::Stream`, records by `sse::Decoder` |
| `chat_voice/dictation/upload.rs` `transcribe` | POST of the WAV | `Body::Bytes`, the fallback header read off the `Response` |
| `chat.rs` Attach on a produced picture | `api::fetch_file`, a fetch to a blob | `api::file_of` over `Http::blob` (the Audio lab's kept upload too, over `Http::fetch()`; `fetch_file` is gone) |
| `chat.rs` a draft's upload | the kit's `post_raw` with the `File` | `Http::post_body` with `Body::Blob` |
| `chat_live.rs` the feed | an `EventSource` | the transport's stream (§8.3) |
| `chat_attach.rs` a sent and a draft chip's thumbnail, `chat_media/card.rs` a picture and its strip, `chat.rs` the viewer's picture | `<img src="/chat/api/attachments/{id}">` (5) | `chat_bytes::ChatImg` |
| `chat_media/player.rs` a recording | `<audio src>` | `chat_bytes::ChatAudio` (D21) |
| `chat_attach.rs` Download original, `chat_media/player.rs` and `actions.rs` Save, `chat_export.rs` thread, folder and all-chats exports, `knowledge_source.rs` Download original | `<a download>` of the route (6) | `chat_bytes::Saver` over `bytes::download` |
| `knowledge_source.rs` the viewer's text | the kit's free `get` (2) | the handed `Http` |

Beyond them, every kit call in the Chat's modules (47 in 26 files) and in the kit pieces it renders
(11) takes the handed transport instead of the free functions. What surprised: the attachment
upload went through the kit's `post_raw` with a browser `File`; three more streams than the survey's
one ran over `post_sse`; the kit's own catalog, voice picker and profile editor fetched by
themselves; and Trunk's loader is an inline script too (§8.4).

Two tests keep it so: `pages/chat_bytes/contract.rs` scans the Chat's modules for a direct fetch,
`EventSource`, request, free kit call, `src=`/`href=` to an lmgw path, `window.location` or the
dashboard's session (`crate::session`), and the kit's `tests/network_contract.rs` refuses a
free-function call outside the HTTP client. The exceptions are named, each with its reason, by the
use as written:
- `chat_voice/realtime/live.rs`, `.location()`: the realtime WebSocket's URL, since the realtime
  socket is not proxied and the sidebar has no voice mode yet (kai-firefox §8.2).
- `chat_live.rs`, `crate::session::locked_signal()`: the feed opens while the session is good and
  closes when it locks.
- `chat_live.rs`, `crate::session::is_locked()`: a feed whose refusal locked the session ends
  rather than retry; the login opens a new one.
- `chat_voice/dictation/upload.rs`, `crate::session::lock()`: a transcription refused `401
  session_required` locks the session, as the kit's hook does for its helpers.
The three session uses are what WP3b replaces with the host's equivalent (§8.9).

### 8.3 The feed on the transport

`ChatLive::Feed` reads `GET /chat/api/feed` with `Read::Stream` (`chat_live.rs`,
`chat_live/resume.rs`): the same events (`EVENTS`, each to `said`), the same hold. The page carries
the cursor: every record's `id:` is kept (an empty one clears it), and so are `hello.cursor` and
`caught_up.cursor`, where the stream stands, as the lmgw-client reader keeps them; each new
connection sends it as `Last-Event-ID`. What ends a connection, and what follows (D23):
- **It ends, drops or fails to connect**: tried again after a wait, 1 s doubling to 30 s, the
  server's `retry:` as the first wait when it sends one; a connection that delivered a record
  resets it.
- **It goes silent**: once `hello` said `keepalive_s`, two of those intervals without a byte (a
  keep-alive comment counts) end it as dead; the stream is dropped and the request aborted, and the
  reconnect waits as above. Before `hello` no clock runs: the server sends it first. Two intervals
  are what Kai's own link counts.
- **A `400` to a connection that sent a cursor**: the feed could not read the cursor. It is
  dropped and the feed connects again at once from now; that connection's `hello` makes the page
  read everything again.
- **Any other `4xx`** (a `408` and a `429` aside, which wait and retry like a `5xx`): asking again
  gets the same answer, so the feed stops, and the Chat's head says "not live" with the refusal in
  its tooltip (`chat_live::FeedStopped`). A `401 session_required` locks the session instead (the
  kit's hook), and the login opens a new feed.
The page's cleanup or a locked session drops the stop sender, which aborts the open request at once.
The dashboard keeps `ChatLive::Shell`.

### 8.4 Scripts as files

The page holds no inline script. `index.html`'s theme and scale script is `assets/boot.js`, a classic
script in `<head>` (it runs before first paint, as the inline one did). Trunk's inline loader is
gone: `Trunk.toml`'s `pattern_script` writes `<meta name="lmgw-wasm" data-js=… data-wasm=…>` and
`assets/start.js`, a hashed module file, loads the glue, runs the app and dispatches
`TrunkApplicationStarted` for the desktop shell (D24). Only a name with Trunk's content hash is
served `immutable`; Trunk writes the hash as `{:x}` of a 64-bit value, without leading zeros, so
`content_hashed` takes 12 to 16 hex digits (a build's wasm had 15, and was served `no-cache`). `codeblocks.js` was already a module file and
loads its libraries by `<script src>` from `vendor/`. The libraries under `script-src 'self'
'wasm-unsafe-eval'`:

| Library | Code from strings | Under the policy |
|---|---|---|
| highlight.js 11.10.0 | none | works |
| KaTeX 0.18.9 | none (inline `style` attributes, which `script-src` does not govern) | works |
| Mermaid 12.0.0 | `Function("return this")()` in five bundled lodash copies, reached only when neither `global` nor `self` is an object, never in a page; `(0,eval)(t)` in chevrotain's `toFastProperties`, after an unconditional `return`; `new Worker` only for the ELK layout, which the Chat does not use. Its `<style>` in the SVG is `style-src`'s | works, no fallback needed (D27) |
| DOMPurify 3.4.16 | none | works |
| wasm-bindgen glue | none; `inline_js` snippets are emitted as files under `snippets/`; compiling WebAssembly needs `'wasm-unsafe-eval'` | works |

No element carries an inline handler. `tests/it/ui_scripts.rs` checks the served page: every script
a same-origin file that answers, no `on…` attribute, the loader and both names on the `<meta>`.
Without a bundle it cannot check anything: under `ci/check.sh`, which builds the bundle first and
exports `LMGW_UI_BUNDLE_REQUIRED=1`, that is a failure; elsewhere (a plain `cargo test`, the
GitHub job without Trunk) it prints `SKIP` and passes.

`scripts/csp-check.sh` proves it in a browser: a fresh dev instance (the echo mock as chat model and
as TTS) behind a proxy (`scripts/csp-check/proxy.py`) that records every violation the browser
reports. A canary page with an inline script must be reported, so the check can see one. Two drives
run through it, each under its host's policy (the drive switches the proxy's mode):
- `scripts/drive/chat-csp.json`, the dashboard host, under the gateway's own policy plus
  `script-src 'self' 'wasm-unsafe-eval'`: the Chat mounts, a reply renders highlighted code, KaTeX
  math and a Mermaid diagram, the preview is the `srcdoc` frame, a thumbnail and an export are the
  routes themselves.
- `scripts/drive/chat-csp-relay.json`, the test host (§8.6), under the sidebar's extension page
  policy alone (§8.9; nothing of the gateway's header): the proxy also checks that every
  `/chat/api` and `/v1` request carried the test host's tag (D29).
It passes when both drives pass, the canary's is the only violation, and no request went untagged.
Run after the review round: both drives passed, the canary's was the only violation, and every Chat
request of the relay drive went through the transport (about 100 s on a warm build). The extension
policy is the least that runs the Chat: with `style-src 'self'` alone, 20 violations came from
inline style attributes (Leptos, Mermaid, DOMPurify's checks, `codeblocks.js`) and `<style>`
elements (Mermaid's, the preview page's).

### 8.5 The frame seam for the HTML preview

`chat_preview.rs`: the preview takes its frame from the host as `PreviewFrame` in context.
`Srcdoc`, the default, is the dashboard's sandboxed `srcdoc` frame, unchanged. `Posted { src }` is a
page of the host's, loaded as the frame's `src` without a `sandbox` attribute; once it has loaded,
and again when the open preview's code changes, it is posted `{type: "lmgw-preview", code, lang}`
with target origin `*`, but only while that first load is the frame's document: the frame counts its
loads, and after a second (the previewed page navigated it, or replaced the host's page) nothing is
posted (D25). The receiving page takes a message only from the page that framed it (`ev.source ===
parent`): the previewed page, which it frames itself, can post to it too. The test host's page is
`scripts/csp-check/preview-host.html`: it shows the code in a `srcdoc` frame of its own, and the relay
drive's previewed code loads a script (`preview-intruder.js`) that posts code of its own to it, which
it ignores. The sidebar's page, listed in `sandbox.pages` with its own policy, is WP8.

### 8.6 The test host

A debug build of the dashboard runs its Chat as a relaying host when `localStorage["lmgw-test-host"]`
is `relay` (`crates/lmgw-ui/src/test_host.rs`, D26): a transport with no link over the browser's
`fetch`, a blob body sent as its bytes (read as the sidebar's transport reads it), a blob answer
handed back as whole bytes and a stream as its chunks, and `x-lmgw-test-relay: 1` on every request
it sends (D29); no `ChatLive`, so the Chat follows its feed through it; and `PreviewFrame::Posted`
with the test page. A release build has none of it. The relay drive checks there, under the
extension policy alone:
- through the transport, as the proxy's log of tagged requests shows: the feed, the send stream,
  read-aloud (its speech reaches the player: samples pushed), the picked file's upload;
- code, math and a diagram; the preview page fed by `postMessage`, and the previewed page's own
  message to it ignored;
- a draft's and a sent message's thumbnails and the viewer's picture as object URLs of bytes, a
  thumbnail's URL revoked once its thread is left; an export saved from a blob under the name the
  answer gave;
- a thread made elsewhere showing through the feed; the proxy then cuts the feed mid-stream, the
  reconnect goes through the relay with `Last-Event-ID`, and a thread made after the cut shows;
- at the end: no Chat request untagged, no `use_http()` without an owner.

The owner's dashboard was checked with the existing rigs when WP3a was built. Of
`scripts/chat-drives.sh`'s 17 drives, 14 passed (render, actions, sampling, search, knowledge,
attachments, export, security, leaving mid-stream, the key-scope editor, voice render, profiles).
Three failed for reasons of their own, the same with or without WP3a:
- `chat-folders` clicks the row menu's second entry for "Move to…", which is the third since Pin
  came first.
- `chat-ongoing` expects its folder to be folder 1, which holds only when it runs alone; alone it
  passed.
- `chat-live` makes a device key, which the gateway refuses while the device listener is off
  (`device_listener_off`).

`webkit-check.py` (WebKitGTK, the desktop window's engine) loaded `/chat`, `/knowledge`,
`/usage/keys` and `/settings/chat` with the new loader and passed.

### 8.7 Tests

- Kit: `http/fake.rs` (the JSON helpers send plain data and read answers and refusals, a stream in
  chunks, bytes with their type), `http/sse.rs` (records split across chunks, ids, `retry`,
  comments, an event without data; a 3 MiB frame in 256-byte chunks and a catch-up of 20 000 records
  in one chunk, each scanning no more than its bytes; a terminator split at every point),
  `http/bytes.rs` (each object URL revoked exactly once whatever lands when; only the newest load's
  failure shows; the download's name from `content-disposition`), `tests/network_contract.rs` (no
  free call past the handed transport).
- `lmgw-ui`: `chat_live` (two connections over the fake, the second resuming from the first's last
  id, a record the feed does not send skipped and counted; a stream silent after `hello` ended as
  dead after two keep-alives and resumed from `hello`'s cursor; a cursor refused with a `400`
  dropped and the next connection made without it; the wait doubling while connecting fails, a
  refusal not resetting it; a record that does not read is a resync), `chat_live/resume.rs` (the
  cursor from ids, `hello` and `caught_up`; the waits, the server's `retry:`; the dead-link timing;
  what each refusal means), `chat_turn.rs` (a stop is the signal's: a fake transport answering
  "cancelled" is a stop when the signal aborted and a failure when it did not),
  `chat_preview.rs` (the posted message; only the first load is posted to), `chat_bytes/contract.rs`
  (with the session uses and their exceptions).
- `lmgw-core`: `tests/it/ui_scripts.rs`; `tests/it/chat_page_gone_scan.rs` counts a call on the
  handed `Http` as a request, as it counted the free functions; `web::ui`'s `content_hashed` takes
  Trunk's hash at every length it writes down to twelve digits.
- In a browser: `scripts/csp-check.sh`.

### 8.8 Draft calls

- **D18. The transport is a handle in context, captured where the page is set up.** A task spawned
  from a handler has no reactive owner to read context from, so the Chat reads `use_http()` while it
  is set up and carries the `Copy` handle into its structs (`TurnEnv`, `OpenEnv`, `ListRead`,
  `FolderEnv`, `KbUi`, `MediaCtx`, the voice parts) and closures. The kit's free functions stay the
  browser's `fetch` for every other page; the contract tests keep the Chat off them. A call without
  an owner falls back to `fetch` silently in a release build; a debug build says so loudly (a
  console error and `<html data-use-http-orphans>`), and the relay drive fails on it.
- **D19. A request is plain data but for two browser objects.** The `AbortSignal` is the browser's
  cancel token, which every host in a page can listen to (a relay sends its cancel frame). A body
  may be a `Blob`: on the dashboard a picked file goes as itself, never copied through the wasm
  module's memory, as before; a relaying host reads it as bytes.
- **D20. A transport may have a link, and the dashboard's has.** Pictures, recordings and downloads
  load the route itself when `link` answers, so the dashboard keeps the browser's cache, an
  `<audio>`'s ranges and `preload="none"`, and the desktop shell's Save As with the server's name,
  and an export is never read into memory. Bytes and object URLs are for a transport without one.
  With a link, a download's `<a download>` is clicked inside the click handler that asked for it,
  as the dashboard's own link was, not a task later.
- **D21. Without a link, a recording is fetched at its first Play.** A thread with many recordings
  fetches none of them until one is played, as on the dashboard; the player shows a Play button
  until then and plays once the bytes are in. When they cannot be read (a relay's size bound, a
  file gone) the Play button is back and a toast says why; a picture shows a marked box with the
  reason in its place.
- **D22. An object URL lives as long as its element; a download's for a minute.** An element's URL
  is revoked when it goes or shows another path, and a load that lands after either is revoked at
  once. A download's is revoked a minute after the click: the browser reads it after the click, and
  revoking at once can cut it off.
- **D23. The feed waits 1 s, doubling to 30 s, between tries.** The server's `retry:` is the first
  wait (and the cap when longer); a connection that delivered a record starts over. The 30 s bound
  is when the page asks again, not how long it follows. A connection silent for two of `hello`'s
  `keepalive_s` intervals is dead (the value the server sends, and the count Kai's link uses). A
  `400` to a cursor drops it and connects from now; any other `4xx` but `408` and `429` stops the
  feed with "not live" in the Chat's head, since asking again gets the same answer.
- **D24. The loader is a hashed module file reading a `<meta>`.** Trunk's `pattern_script` keeps it
  inside the Trunk build, with the hash and integrity every other script has, where a post-build
  rewrite would be a second tool. The dashboard's own policy has no `script-src` yet, although the
  page passes under the strict one (§8.4): the dashboard's HTML preview is a `srcdoc` frame, which
  inherits its parent's policy, so the previewed page's inline scripts would be refused (checked in
  headless Chrome: a sandboxed `srcdoc` frame's inline script runs under today's policy and is
  refused once `script-src 'self' 'wasm-unsafe-eval'` is added). Before the dashboard can carry it,
  its preview has to move to a page of its own whose policy allows the previewed page's scripts
  (`PreviewFrame::Posted`, as the sidebar's will). Then the desktop window is the next check: its
  shell injects initialization scripts and evaluates script in the page (the palette, IPC answers),
  which was not measured, since the preview already rules the change out.
- **D25. The preview's page gets the code on its load and on every change, to any origin, while it
  is the frame's first load.** A page under a sandboxing policy has an opaque origin no target
  origin can name; what is posted is the code block the page shows anyway. A frame that has loaded
  a second document holds something else, which gets nothing. The receiving page checks that a
  message comes from its parent (`ev.source === parent`), so the previewed page it frames cannot
  hand it code.
- **D26. The test host is the debug dashboard behind a `localStorage` key.** It runs every
  non-dashboard path in a real browser without a second bundle; a release build has none of it.
- **D27. Mermaid needs no fallback for hosts with a strict policy.** Its code-from-strings paths are
  unreachable in a page (§8.4), and the check renders a diagram under the policy. If a later version
  needs one, the fallback is the diagram's source, which the code view already shows.
- **D28. A stop is told by the request's signal, never by the error's words.** Every transport words
  an abort its own way (the browser's fetch "AbortError: …", a relay "cancelled"), so a caller asks
  `signal.aborted()`: the Chat's turns, dictation, read-aloud and an image card's action do. The
  cancel contract (the signal, and dropping the future or the stream) is written on `Transport`
  and `ByteStream`.
- **D29. The relay check is a header the test host's transport adds.** The proxy cannot see which
  code sent a request; a tag every relayed request carries can be checked, and a request to
  `/chat/api` or `/v1` without it is one that went past the transport. The drive's own requests
  carry a tag of their own. The tag exists only in the debug test host.
- **D30. The relay drive runs under the extension policy alone.** What the sidebar's manifest will
  carry (§8.9) is what the test host runs under, nothing of the dashboard's header beside it, so a
  directive the Chat needs and the manifest lacks shows as a violation here.
  - *As built (kai-firefox WP8b):* the sidebar carries §8.9's policy as its page's `<meta>`, not
    as the manifest's (§8.9's note says why); the drive's policy is still that one, word for word.

### 8.9 For WP3b and WP8

- The crate takes `Http` from context. The sidebar provides `Http::new(its transport)` and
  `PreviewFrame::Posted` before it mounts the Chat and before `provide_model_catalog`, and hands no
  `ChatLive`, so the Chat follows the feed through that transport.
- The frame transport: `Request` maps to a `req` frame; `Read::Stream` is relayed unbuffered (its
  timeout covers the response head), `Whole` and `Blob` are buffered under `proxy_max_mb`;
  `Body::Blob` is read with `arrayBuffer()` (`lmgw_ui_kit::http::blob_bytes`); `link` answers
  `None`. It keeps the cancel contract (D28): the `AbortSignal`'s `abort` sends the cancel frame,
  and so does dropping the `send` future or the answer's `ByteStream`; the error it answers then
  may say anything. A refusal it makes itself (a body over `proxy_max_mb`) is a `Response` with
  its status and the house `{code, message}`, which the Chat shows (a picture's failed box, a
  recording's toast). The test host's `Relay` is the shape.
- **The sidebar's extension page policy**, the manifest's
  `content_security_policy.extension_pages`, exactly as the relay drive runs under it (D30):

  ```text
  default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; media-src 'self' blob: data:; font-src 'self' data:; connect-src 'self'; object-src 'none'
  ```

  `'wasm-unsafe-eval'` compiles the module; `style-src 'unsafe-inline'` is for inline style
  attributes and `<style>` elements (Leptos, KaTeX, Mermaid, DOMPurify, `codeblocks.js`), without
  which the check reports 20 violations; `blob:` is for pictures and recordings shown from bytes;
  `connect-src 'self'` is the loader's fetch of the wasm (the Chat's requests go as frames).
  `default-src 'self'` covers the preview page's frame. The preview page in `sandbox.pages` carries
  its own policy, which allows the previewed page's inline scripts.
  - *As built (kai-firefox WP8b and its review round):* the sidebar runs under this policy word for
    word, but as a `<meta http-equiv="Content-Security-Policy">` in its page, not as the manifest's
    `extension_pages`. The manifest's policy governs every page of the extension, its background
    too, and the background's own fetches (a tab's PDF, a local PDF by `file:`, a screenshot's
    `data:` URL) are refused by `connect-src 'self'`. So the manifest carries these directives with
    the background's `connect-src` (`'self' http: https: file: data: blob:`) and Firefox's
    `upgrade-insecure-requests`, and the page's `<meta>` narrows `connect-src` to the one above
    (both apply; a page that lost its `<meta>` keeps the rest). kai-firefox's design §8.2 and its
    §14 item 62.
- The sidebar's preview page takes a message only when `ev.source === parent` (D25), as the test
  host's `preview-host.js` does.
- The feed and dictation read the dashboard's session (`crate::session`: the locked signal,
  `is_locked`, `lock`; the three exceptions of the contract scan, §8.2); the crate needs the host's
  equivalent (the sidebar's link to Kai). *Built in WP3b: `ChatHost::locked` and the kit's hook
  (D33).*
- The sidebar's page needs no inline script either: its own `Trunk.toml` takes the same
  `pattern_script` and `start.js`, and `boot.js`'s theme becomes `prefers-color-scheme` (kai-firefox
  §8.5).
- Bundle assets loaded by URL go along: `codeblocks.js` resolves `vendor/` next to itself; the voice
  worklets and the visualisation module are loaded from `/voice/…`. *WP3b: under the host's asset
  base (§9.4, D35).*
- Not proxied, and so not in the sidebar yet: the realtime WebSocket (its URL from
  `window.location`), dictation (the microphone). The links to dashboard pages of §1.4 remain.
  *WP3b: the URL and the links are the host's, and a host without voice shows none (D32, D34).*
- The feed's dead-link timeout is built (D23): a relayed stream that stalls ends after two
  keep-alive intervals and reconnects with its cursor. The relay need not time a stream's body
  itself.
- `lmgw_ui_kit::profiles::api`'s functions take an `Http` now; a client that calls them directly
  passes `Http::fetch()` (Kai uses only `ProfilesPanel`, which reads it from context).
- The dashboard's own policy gains `script-src` only after its preview has a page of its own (D24).

## 9. WP3b: the lmgw-chat-ui crate

*Built. The Kai for Firefox design's §8.2 item 3 and §8.4–§8.5: the Chat as a library a second
host mounts, its seams, and a layout for a sidebar's width. D31–D44 are draft calls.
Rebased onto the machine check's WP8b, whose Chat side moved into the crate (§9.2), and revised
after its review round (§9.9).*

The Chat's code left the dashboard: `crates/lmgw-chat-ui` holds every module that was
`pages/chat*` with its children, the source viewer (`knowledge_source`) and the Chat's inbox of
live changes (`live`). The dashboard (`lmgw-ui`) depends on it and mounts it at `/chat`; it looks
and behaves as before (§9.6). The crate names no router, no dashboard module, no address bar and no
session; what it needs from a host is a struct of seams and the kit's transport.

### 9.1 The public API

- **`Chat`**, the mount component. It takes no props: it reads the host from context and fills its
  parent (a `height: 100%` column; the dashboard's is its content pane).
- **`provide_chat_host(ChatHost)`**, called once before the Chat mounts. `ChatHost` (`host.rs`):
  - `location: Arc<dyn ChatLocation>`: where the Chat is, in the host's terms (D32);
  - `locked: Signal<bool>`: the host's session is locked (D33);
  - `live: ChatLive`: `Host { changes, hold }` for a stream of the host's own, `Feed` (the
    default) for the Chat's own `/chat/api/feed` (D1, D31);
  - `preview: PreviewFrame`: `Srcdoc` (the default) or `Posted { src }` (§8.5);
  - `voice: Option<ChatVoice>`: `ChatVoice { realtime_url }`, the WebSocket URL of
    `/v1/realtime?chat_thread=<id>`; `None` (the default) shows no voice controls (D34);
  - `assets: String`: the base the Chat's assets are served under, `"/"` by default (D35);
  - `machine: Option<ChatMachine>`: the machine check's surfaces in the Chat (machine-check design
    D177): the host's banner over the conversation, its card above the empty state and the fix
    under a `gpu_setup` card, built with `ChatMachine::new(banner, empty, fix, here)`; `None` (the
    default) shows no banner and no card, and a `gpu_setup` card is the 503's sentence alone
    (machine-check D178; D31, D44);
  - `origin: Option<ChatOrigin>`: where the user writes from, one sentence the host words, asked
    at each send and each edit of a user message and sent as its `origin` (§9.10, D53); `None`
    (the default) sends none.
  `ChatHost::new(location, locked)` gives the defaults.
- **`ChatLocation`**, the trait the host implements: `opened_at() -> ChatAddress` (the thread,
  the message to reveal, the folder whose settings open; read once at mount), `show_thread(id)`
  (called on every switch, mid-stream too), `dashboard_href(path) -> Option<String>` (a link to a
  dashboard page; `None` leaves the link out).
- **`live::{inbox, ChatInbox, ChatPost}`**: a host with a stream of its own makes the inbox,
  posts each `ChatChanged` into it and hands the read end as `ChatLive::Host`.
- For the dashboard's other pages: `md_to_html` (the docs playground), `knowledge_source::{SourceModal,
  SourceRef, SourcePlane}` (the Knowledge page), `media_defaults::{use_view, MediaModelBox,
  MediaVoiceBox}` (Settings → Chat → Media).
- Besides the host: the kit's `Http` in context before `lmgw_ui_kit::catalog::provide_model_catalog`
  (which captures it), the kit's `ToastHost` rendered before the Chat, and its dirty guard
  (`widgets::dirty_guard::provide_dirty_guard`, with `DirtyGuardHost` for the question; the
  thread settings ask before a link leaves them unsaved).

The dashboard's host is `lmgw-ui`'s `chat_host.rs`: the `/chat` route reads its router's query into
the `ChatAddress`, keeps `/chat?t=<id>` with `replaceState`, links its own routes, hands the shell's
`/api/events` (its `chat` frames into the inbox, its `vram` frame's hold) as `ChatLive::Host`, builds
the realtime URL from the page's own origin (`lmgw_client::requests::realtime_page_url`) and serves
the assets from the root, and fills `machine` with `widgets::machine_check::chat_slot()` (the
banner, `MachineCard`, the compact `MachineCheck`, and whether the page is on the computer lmgw runs
on). The debug test host (§8.6) hands `Feed`, `PreviewFrame::Posted` and no `machine` there
instead; its transport stays an `App`-level context, as the catalog needs it.

### 9.2 What moved, and where the dashboard finds it

| What | From | To | The dashboard's old path |
|---|---|---|---|
| The Chat (`chat*`: 28 modules with their children) | `lmgw-ui` `pages/` | `lmgw-chat-ui` `src/` | `pages::Chat` gone; `/chat` is `chat_host::ChatRoute` |
| `knowledge_source` | `lmgw-ui` `pages/` | `lmgw-chat-ui` | the Knowledge pages import `lmgw_chat_ui::knowledge_source` |
| The Chat's inbox (`live/chat.rs`) | `lmgw-ui` | `lmgw_chat_ui::live` | `crate::live::ChatInbox` (`pub use`) |
| The tool picker, `approval_rule` | `lmgw-ui` `widgets/` | `lmgw_ui_kit::widgets` | `crate::widgets::tool_picker::…`, `crate::widgets::approval_rule::…` (the glob `pub use`) |
| `RowMenu`, `MenuItem` | `widgets/table.rs` | `lmgw_ui_kit::widgets::row_menu` | `crate::widgets::{RowMenu, MenuItem}`, `crate::widgets::table::{…}` (`pub use`) |
| `use_slash_focus` | `widgets/filter_bar.rs` | `lmgw_ui_kit::widgets::slash` | `crate::widgets::use_slash_focus`, `filter_bar::use_slash_focus` (`pub use`) |
| `Dl`, `DlBar`, `DlLine`, `DlChip`, `SENTENCE`, `PULL_KIND` | `gpu_download.rs` | `lmgw_ui_kit::download` | `crate::gpu_download::…` (`pub use`); the live sources and who a download is for stay |
| `pcm_to_wav` | `pages/audio_stream.rs` | `lmgw_ui_kit::wav` | `audio_stream::pcm_to_wav` (`pub use`) |
| `api::file_of` | `lmgw-ui` `api.rs` | `lmgw_ui_kit::http::file_of` | `crate::api::file_of` (the glob `pub use`) |
| The machine check's Chat side (WP8b: `ChatMachine`, `BannerSlot`, `EmptySlot`, `GpuSetupCard`, `Parts`) | `lmgw-ui` `pages/chat_machine.rs` | `lmgw-chat-ui` `src/chat_machine.rs` | `pages::ChatMachine` gone: `lmgw_chat_ui::ChatMachine`, in `ChatHost::machine`; `widgets/machine_check/{surfaces,nudge,involved}.rs` stay the dashboard's |
| The Chat's CSS | `app.css` (its chat sections, the source viewer's, the search and knowledge rules, the feed badge, WP8b's `.chat-empty-machine`, `.chat-gpu-setup*` and the transcript's `--mc-stick-gap`, the thread list's and a code block's `user-select`) | `assets/chat.css` | linked after `app.css` (D39) |
| The tool picker's CSS (`.mcp-pick`, `.mcp-tools`, `.mcp-approval`, `.tool-pick`, `.ask-tick`, `.seg-mini`) | `app.css`, then `chat.css` | `lmgw-ui-kit` `assets/kit.css` | the key editor on `/usage/keys` uses the picker too (D38) |
| `voice.css`, `codeblocks.js`, `vendor/`, `voice/` | `lmgw-ui/assets/` | `lmgw-chat-ui/assets/` | `index.html` links them from there; served at the same URLs |

Toasts, `SplitPane`, `Modal`, `Select`, `Popover` and the pickers were the kit's already. The
profile editor page (`/chat/profiles`) stays the dashboard's (D40). The parts of the dashboard
the Chat used and did not take: `crate::session` (D33), `ui_scale::tauri_invoke` (D36), the router
(D32), `ChatLive::Shell`'s `LiveBus` (D31).

### 9.3 The narrow layout

Below 520 px of the Chat's own width (`chat/narrow.rs`, measured on its root, not the window) the
thread list and the thread's settings have no rails: each is a drawer (the kit's
`SplitPane` with a `Drawer`), opened by a button in the Chat's head (the list's with its count,
the settings' at the head's end) and in the empty state, and laid over the conversation from its
edge, as wide as its automatic share held between its floor and the column (the list 220 px, the
settings 280 px, so at 280 px the settings fill the column). Esc, the fold button, a click back in
the conversation, a pick, and opening a thread put a drawer away; opening one puts the other away.
The conversation takes the whole column with 10 px sides, a user's bubble 90 % of it. It works from
280 px up. Wider than 520 px nothing changed: the panels dock, and below their floors fold to rails
that lay the panel over the conversation, as the dashboard did. Colours and sizes are the kit's
tokens, light and dark by `[data-theme]`.

In the dashboard the Chat is the content pane beside the navigation, and the navigation is its
52 px rail whenever the window is under 1100 CSS px (`shell.rs`, `RAIL_BELOW`). So at 125 % the drawers come below about 716 px of window (572 CSS px less the rail), not below the
920 px the 216 px navigation would give: at 880 px (704 CSS px) the Chat has 651 px, and its
panels fold to their rails as before (D37).

The machine check's surfaces keep to the narrow layout (D43): the Chat sets `--chat-side`, its
conversation's side gutter (22 px, 10 px narrow), which the dashboard's banner takes for its sides,
and the narrow transcript and empty state set `--mc-stick-gap` to their 12 px top padding, so the
head of a setup's run under a `gpu_setup` card sticks flush there too.

### 9.4 Assets a host bundles

- **Stylesheets, in this order:** the kit's `kit.css` and `profiles.css`, the base components
  (today the dashboard's `app.css`: buttons, fields, chips, notices, the type badges, the progress
  bar, the dense table; D39), the crate's `chat.css`, then its `voice.css`.
- **`assets/codeblocks.js`**, a module script the host loads itself (it sets
  `window.lmgwDecorateCode` and fires `lmgw-preview`), with **`assets/vendor/`** beside it: it
  imports highlight.js, KaTeX, Mermaid and DOMPurify from `vendor/` relative to its own URL.
  Trunk: `<link data-trunk rel="copy-dir" href="…/lmgw-chat-ui/assets/vendor">` and `<script
  data-trunk src="…/lmgw-chat-ui/assets/codeblocks.js" type="module" data-no-minify>`.
- **`assets/voice/`** under the asset base (`ChatHost::assets`): the capture and player
  worklets and the visualisations (`voice/viz/*.js`), loaded by URL when voice runs, never
  hashed. A host without voice may leave it out.
- The fonts are the host's (`--sans`, `--mono`).

### 9.5 Tests

- `chat_bytes/contract.rs` now scans the whole crate and refuses, beside WP3a's bypasses,
  `.location()`, `.history()`, `leptos_router`, `crate::session` and a literal `"/voice/`; its list
  of exceptions is empty (the realtime URL and the session went to the host).
- Unit tests: `host.rs` (an asset under the host's base), `chat/narrow.rs` (the width that is
  narrow), the kit's `split.rs` (a drawer's width), the dashboard's `chat_host.rs` (the query as an
  address; the shell's hold, moved from `chat_live`), the kit's `download.rs` and `wav.rs` (the tests
  that moved with the code). The kit's network contract admits `/chat/api/tools*` (the picker).
- `lmgw-core`'s `tests/it/chat_page_gone_scan.rs` reads `lmgw-chat-ui/src/chat_sync/gone*`.
- `ci/changed.py` and `ci/it-map.toml` know the crate (a change runs trunk; the worklets are
  under its `assets/voice/`).
- `scripts/drive/chat-narrow.json`, the narrow window in WebKit at 125 %, run by
  `scripts/chat-drives.sh --webkit` (`webkit-check.py --backend broadway --zoom 1.25 --drive`):
  at 880 px the rail and no drawers; at 700 px (a 507 px Chat) the head's two buttons on the first
  line, each drawer laid over the conversation from its edge at its width (220 and 280 px) and on
  top, one putting the other away, the fold button, Esc, a press in the conversation and a pick
  putting them away, and the dirty guard asking over an open settings drawer (on top, inside the
  window; Stay keeps the page, the drawer and the edit; Leave and discard leaves and stores
  nothing).
- `crates/lmgw-ui/Trunk.toml` watches the crate and the kit beside its own directory, so
  `trunk watch` rebuilds on an edit in either.

### 9.6 The dashboard as before

- **The same styles.** Every element of the Chat page (a thread with markdown, a table, code,
  math and a second turn; the thread list with a pin) was read with `getComputedStyle` (65
  properties each) in headless Chrome before and after the move, at 1440, 1100 and 900 px, dark
  and light, and at 1440 with the settings open: the same 478 to 728 elements, and no property
  differs but the composer hint's opacity, which is mid-animation.
- **The drives.** `scripts/chat-drives.sh --webkit`: 14 of 17 pass, the same 14 as in §8.6, and
  the same three fail for their own reasons (`chat-folders`' menu entry, `chat-ongoing`'s folder
  number, `chat-live`'s `device_listener_off`); `webkit-check.py --backend broadway` loads `/chat`,
  `/knowledge`, `/usage/keys` and `/settings/chat` and passes.
- **The policy.** `scripts/csp-check.sh`: both drives pass, the dashboard's and the test host's
  under the extension policy alone, the canary's is the only violation, and every Chat request of
  the relay drive went through the transport.
- **The narrow layout**, in headless Chrome with the dashboard's own navigation hidden so the Chat
  has the viewport, at 338 and 280 px, dark and light: no rail, the two head buttons, nothing
  wider than the column, the list's drawer 220 px, the settings' 280 px (279 at 280 px, the
  column), the settings' button putting the list away, the fold button closing; at 1440 px the
  panels dock again and the buttons are gone.

### 9.7 Draft calls

- **D31. One struct of seams, in context, besides the transport.** `ChatLive` and `PreviewFrame`,
  separate contexts in WP3a, are fields of `ChatHost`, so a host fills one value and cannot miss a
  piece; `ChatLive::Shell(LiveBus)` became `ChatLive::Host { changes, hold }`, which names no
  dashboard type. The machine check's slot (WP8b's `ChatMachine`, a context of its own on `main`)
  became the field `machine: Option<ChatMachine>` the same way, `None` by default. The transport
  stays the kit's own context: the kit's pickers outside the Chat read it too, and the catalog
  captures it at `App` level. (Revised in the review round: the `machine` field.)
- **D32. The location is a trait the host implements, not props.** The Chat reads the address once
  and writes it on every switch from deep inside (the opener, the delete-elsewhere path), so a
  prop would have to be threaded through both; the page carries it as a `Copy` handle owned by the
  page, so a read that lands after the page was left writes no address. The dashboard's
  `ChatRoute` reads the router's query; a host with no address keeps the thread in memory.
  Links to dashboard pages go through `dashboard_href`, and a host without the dashboard leaves
  them out ("Add a model", "Open the Audio lab", "Edit profiles…").
- **D33. The session is one signal; a refusal locks through the kit.** The feed opens while
  `locked` is false. A `401 session_required` the Chat reads itself (dictation's upload, whose
  answer carries a header the helpers drop) runs the kit's configured hook
  (`lmgw_ui_kit::http::session_required`), as every helper's refusal does, so a host sets its
  lock once, in `http::Config`.
- **D34. Voice is all or nothing per host.** `voice: None` hides the microphone, voice mode, the
  voice menu with read-aloud and the devices, every reply's speaker button and the dictation key,
  and no turn asks to be read aloud; the composer's status line says nothing of dictation or
  read-aloud (it still says what the turn's model is doing), and the page installs no device
  listeners. The thread's Voice settings stay: they are the thread's, and the dashboard and the
  phone read them. A host that relays realtime later hands `ChatVoice`. (Revised in the review
  round: the status line's hold lines and the listeners.)
- **D35. The asset base is one value per page.** The audio code loads a worklet long after any
  reactive owner could be asked, so `provide_chat_host` also keeps the base beside the context;
  two Chats on one page with different bases are not supported.
- **D36. The desktop window's audio routing stays a probe, not a seam.** The crate looks for
  `__TAURI_INTERNALS__` itself (the kit may not name Tauri); only lmgw's window has the
  `audio_outputs` commands, and a browser or the sidebar lists the browser's outputs, as a plain
  tab did.
- **D37. The narrow layout is measured on the Chat, at 520 px, with drawers.** A sidebar has no
  room for a 28 px rail beside 310 px of conversation, and the dashboard in a narrow window gets
  the same drawers. Between 520 px and the panels' floors the dashboard's rails stay as they
  were. In the dashboard at 125 % that is a window under about 716 px, as its navigation is a
  rail below 1100 CSS px (§9.3). (Revised in the review round: the dashboard's threshold,
  measured.)
- **D38. Shared widgets go to the kit, the Chat's own to the crate.** The tool picker (the key
  editor uses it too), the row menu, the `/` key, the download state and the WAV writer are the
  kit's; the dashboard keeps every old path by re-export. The kit's network contract gains
  `/chat/api/tools*`.
- **D39. The crate carries its own rules, not the base components.** `chat.css` is the Chat's
  rules, moved as they were and in the same order; the buttons, fields, chips and notices it uses
  stay in the dashboard's `app.css`, where every page uses them. Moving those into the kit is a
  dashboard-wide change of its own (WP8 needs it, §9.8).
- **D40. The profile editor stays a dashboard page.** It is a routed page around the kit's
  `ProfilesPanel`; the Chat links to it through `dashboard_href`, and a host can frame the panel
  itself.
- **D41. The source viewer is the Chat's.** The Knowledge page imports it from the crate, as it
  took it from `pages/` before (D12).
- **D42. `chat.rs` split by concern, the component kept whole.** The module root holds the
  page's types (threads, messages, tool cards, the IR reader); `chat/page.rs` the `Chat`
  component; `chat/list.rs` the thread list; `chat/drawer.rs` the settings draft and drawer;
  `chat/msg_view.rs` a message and the stats row; `chat/narrow.rs` the narrow layout. The
  component is still one function of about 1600 lines: its closures share the page's signals
  (`chat/page.rs` makes 57 of them), and cutting it would be a rewrite, not a move.
- **D43. A host's surface in the Chat's column keeps to the Chat's gutter.** The Chat names its
  conversation's side gutter `--chat-side` on its root (22 px, 10 px in the narrow layout), and
  the dashboard's machine banner reads it with 22 px as its fallback, rather than the Chat's
  stylesheet styling the host's class or wrapping the host's banner in an element of its own
  (WP8b's drive checks that the banner follows the head directly). WP8b's `--mc-stick-gap` on the
  transcript and the empty state follows the narrow padding the same way.
- **D44. The dashboard builds the Chat's machine slot as the route mounts.** `ChatRoute` calls
  `chat_slot()` beside the rest of its host, where `App` provided it before, so the slot's own
  counters (the newest `gpu_setup` card, the one that holds the fix) start afresh with each Chat
  and the test host's `None` is decided where its other seams are. The machine state it reads is
  still `App`'s one (machine-check D160).

### 9.8 For WP8

What the Firefox sidebar provides, before it mounts `lmgw_chat_ui::Chat`:

1. `http::configure` with the sidebar's base and its `401` hook, then `Http::new(its frame
   transport)` in context (§8.9's contract), then `provide_model_catalog(locked)`.
2. `provide_chat_host` with: a `ChatLocation` that keeps the open thread in memory (or the
   extension's storage), `opened_at` from it, `dashboard_href` answering `None` (or an absolute URL
   of lmgw's dashboard in a tab, a later choice); `locked` from its link to Kai; `live:
   ChatLive::Feed`; `preview: PreviewFrame::Posted { src }`, its page in `sandbox.pages`; `voice:
   None`; `assets` where it copies `voice/` (or anything, without voice); `machine: None` (the
   default: a `gpu_setup` turn is its sentence alone, which ends "Open lmgw there to see what to
   do.").
3. `ToastHost` before the Chat, `provide_dirty_guard` with a path function (the sidebar has one
   page: any constant path) and `DirtyGuardHost` with a `navigate` callback, which "Leave and
   discard" calls with the link it held (with `dashboard_href` answering `None` the Chat shows
   few links); `data-theme` from `prefers-color-scheme`.
4. Bundled: `kit.css` (which carries the tool picker's rules now), `profiles.css`, the base
   components (below), `chat.css`, `voice.css` (the voice controls are absent, but the status
   line's rules are there), `codeblocks.js` with `vendor/`, its own `start.js` loader (§8.4),
   fonts of its own or the system's. Not `machine.css`: without a slot nothing of the machine
   check is drawn, and the `gpu_setup` card's rules are `chat.css`'s.
5. The manifest's `content_security_policy.extension_pages` exactly as §8.9 gives it.
   *As built:* the page's `<meta>` carries it, the manifest a wider `connect-src` for the
   extension's background (§8.9's note).

Not built yet:
- The base components in the kit. Until then the sidebar links lmgw-ui's `app.css`, which carries
  them with the dashboard's own rules and refers to its fonts by `fonts/…` URLs.
- A host-provided realtime socket (voice through Kai's proxy) and dictation in the sidebar.
- `dashboard_href` to lmgw's dashboard in a browser tab.
- A way for the host to hand the Chat an address after it mounted: `opened_at` is read once, so
  "open this thread in the sidebar" (kai-firefox WP8b) has no seam yet; today the host would
  mount the Chat again.
- The scoped model list (D10).
- Splitting the `Chat` component itself (D42).
- Seams the sidebar needs, found while kai-firefox WP8b was built (its design §8.1, §8.2, §8.5,
  §13); the sidebar works around each meanwhile:
  - **A composer seam for a host's action:** a slot in `.composer-tools` for a host's button, and a
    public way to hand the open thread's composer a `web_sys::File` (the upload and its chip, as a
    picked file gets). Attach this page sits in the sidebar's strip and goes in by a synthetic
    drop on `.chat-main` until then.
  - **Mermaid in parts:** its chunked ESM build loaded by `codeblocks.js`, each file under the
    5 MB that AMO's validator parses (Mermaid is one 5.6 MB script, `FILE_TOO_LARGE`, an error
    that stops a signing). The sidebar ships a stand-in, and a diagram shows its source with a
    sentence saying it is not drawn there yet.
  - **`ChatLive::Feed` keeping its cursor across `locked`:** the Chat closes its feed while locked
    and opens a new one without a cursor, which reads everything again. The sidebar's transport
    keeps the cursor and adds it to the next feed.
  - **A narrow `.chat-head` that does not wrap:** at 280 px "Export" wraps onto a second row of the
    head. The sidebar's stylesheet keeps Export on the first line as an icon.
  - **The fonts' licence:** lmgw-ui's IBM Plex fonts carry no licence text beside them (the SIL
    Open Font License asks that it go with them); kai-firefox ships its own copy as
    `fonts/OFL.txt`.

### 9.9 The rebase onto WP8b and the review round

The review found no blocker and no high item. What changed:

- **WP8b's Chat side moved with the Chat** (§9.2): `chat_machine.rs`, its edits to `Msg`, the
  message view, the head, the empty state and the turn's `gpu_setup` branch, and its rules. Its
  slot is the field `ChatHost::machine` (D31), which `ChatRoute` fills (D44).
- **M1.** `Trunk.toml`'s `[watch]` lists `.`, `../lmgw-chat-ui` and `../lmgw-ui-kit`: with
  trunk 0.21.14 an edit in `chat.css` and one in `kit.css` each rebuilt `dist/` under
  `trunk watch`.
- **M2.** The narrow window's drive (§9.5). The review's 920 px came from the 216 px navigation,
  which a window under 1100 CSS px never has; the drive checks 880 px (the rail, no drawers) and
  covers the drawers at 700 px (§9.3, D37).
- **L1.** Without voice the status line has no dictation or read-aloud line and the page installs
  no device listeners (D34).
- **L2.** The tool picker's rules are `kit.css`'s (D38).
- **L3.** The thread list's and a code block's `user-select` rules are `chat.css`'s; the crate no
  longer asks web-sys for `History`; the archive strip's comment is back on `archive_info`; the
  kit's `MenuItem` has no `#[allow(dead_code)]`.
- **L4.** The feed reads the host's `locked` with `try_get_untracked`, a gone signal reading as
  locked.
- The machine check's surfaces in the narrow layout (§9.3, D43).

Checked after the rebase, on a fresh dev instance each, against `main` 68ca8038's numbers:
`scripts/chat-drives.sh --webkit` 14 of 17, the same three failing for their own reasons
(`chat-folders`' menu entry, `chat-ongoing`'s folder number, `chat-live`'s
`device_listener_off`), `webkit-check` and the narrow drive passing; `scripts/csp-check.sh` both
drives passing, the canary's the only violation; WP8b's `machine-check.json` through
`ui-drive.py --init-script scripts/drive/machine-fake.js` at 1536x864 and 1.25, 0 FAIL in 204
checks, and `ui-matrix.py --tauri --dsf 1.25` with the same fake over `/`, `/chat`,
`/chat?t=<id>` and `/settings/gpu#machine`, 0 FAIL in 28 measurements, as on `main`. The
machine check's banner, the card above the empty state and a `gpu_setup` card in the narrow
layout, in headless Chrome with the fake: the banner 10 px from each side, nothing wider than
the column, the transcript's `--mc-stick-gap` 12 px (a check of this pass, not a drive in the
repository). `ci/check.sh` OK (9144 tests).


### 9.10 The sidebar's New chat, the way back and the message origin

*Built 2026-11-04, from testing Kai for Firefox. D45–D55 are draft calls.*

Four changes, each in both hosts (the dashboard and the sidebar mount the same crate):

1. **New chat in the narrow head.** Both side panels are drawers in the narrow layout, so New chat
   was two presses away. The head's first line has a New chat button (a plus) beside the list's
   button; its title and label say where the chat goes: "New chat" or "New chat in <folder>". The
   first line is the list's button, New chat, the model and the settings' button, from 280 px.
2. **New chat follows the open thread's folder** (owner, 2026-11-04: everywhere, the dashboard
   too). The list's New chat, the head's and the empty state's start the chat in the open
   thread's folder, as its menu's "New chat here" does (`FolderEnv::new_chat_in`: the folder's
   defaults, the folder expanded, the Opener's order rules). In an **ongoing** folder the new chat
   becomes the folder's current conversation (ruled 2026-11-04), as its menu's "New
   conversation" makes it (`FolderEnv::new_conversation`, `POST /chat/api/folders/{id}/current`
   with `new`), so Kai's voice continues there. With nothing open, an unfiled thread open or a
   temporary chat open, it stays unfiled as before. A temporary chat has no folder; an Admin chat
   does not go into a folder yet (`chat_folders/goes.rs`, `new_chat_goes`).
3. **The way back to the ongoing conversation** (ruled 2026-11-04: mark Kai's active thread, for returning to Kai after switching to another folder). An ongoing folder's current thread has an accent bar
   inside its row's left edge beside the word "current", and its folder's header the same bar
   beside the word "ongoing". While the open thread is no ongoing folder's current thread and one
   has a current thread, the head shows "Back to <folder>", which opens that thread and expands
   its folder (`back_to`). Narrow, it is the folder's name after an arrow at the start of the
   head's second line; wide, "Back to <folder>" after the badges.
4. **Where a message was written, told to the model** (owner, 2026-11-04). A send
   (`SendRequest.origin`) and a user message's edit (`MessageEdit.origin`) carry one sentence the
   client words about where the user writes from; the sidebar will send "Typed in Kai's sidebar in
   Firefox, beside the tab “<title>” (<URL>).". lmgw knows nothing of the clients. The sentence is
   stored on the user message (migration 0099, `chat_messages.origin`), not in the system prompt:
   a system prompt that changed with every tab would change the start of every request and make
   llama.cpp read the whole conversation again each turn, while a stored line per message keeps
   the history as it was and tells the model where each earlier message came from. Every request
   that replays the message puts it first among its parts (`web/chat_origin.rs`), marked as the
   client's words, not the user's. An edit keeps the message's origin unless it sends one. The
   thread's read (`Message.origin`, the feed's messages too) and both exports carry it, and the
   Chat shows it as one line under the user's bubble. A host hands the sentence through
   `ChatHost::origin` (`ChatOrigin`, asked at each send and each edit); the dashboard hands none.

Draft calls:

- **D45. New chat goes through the folder's own paths.** The page routes its New chat to
  `new_chat_in` or `new_conversation`, which already take the Opener's choice
  (`starting`, `follow`, `failed`), so the order rules (CF-1, CF-4, CL-20) hold for the folder path
  without a copy of the request code. Any thread of an ongoing folder, its current one or an older
  one, starts the folder's new current conversation; with the current thread still empty the
  server keeps it and the toast says it continues there, as the menu's does. A folder the list
  does not know (deleted meanwhile) counts as none.
- **D46. The list's and the empty state's New chat keep their words.** The list's button says
  where the chat goes in its tooltip, the head's in its label and tooltip; the empty state has no
  open thread, so its chat is unfiled.
- **D47. With several ongoing folders, the way back goes to the one whose current thread was
  active last**: the latest `updated_at` among the current threads in the list as read. A current
  thread the list does not carry (the archived view is shown) counts as the oldest, and a tie goes
  to the folder listed first. None shows while the open thread is any ongoing folder's current
  thread.
- **D48. The way back shows in both layouts.** Wide, a small accent "Back to <folder>" after the
  badges (it ellipsizes from 16rem); narrow, the arrow and the folder's name (from 10rem) lead the
  head's second line, so the first line keeps its four controls at 280 px. Its title and label
  say "Back to <folder>, its current conversation".
- **D49. The current thread's mark is a bar and a word, in the accent.** A 2 px accent bar inside
  the row's left edge and the existing "current" in the accent at 600, and on the ongoing folder's
  header the same bar and the word "ongoing": the palette's blue, never the only cue.
- **D50. The origin is its own leading text part**, `<origin note="where the user wrote this
  message, as their app says; not part of the message">…</origin>`, before the message's files and
  text: one IR form, the same text part on the OpenAI, Anthropic and Gemini egresses, as a file's
  block is. The note explains it to the model without a line in the system prompt. Its text has
  every `&`, `<` and `>` written as an entity (`&amp;`, `&lt;`, `&gt;`): the sentence is plain
  and never needs markup, and a page title in it is the page's to choose, so a chat template's
  special token (`<|im_end|>`, `<|im_start|>system`, `<start_of_turn>`, `<|eot_id|>`) cannot reach
  the model as one. Only the model's copy is escaped; the stored sentence and the caption are the
  raw text. `origin` also joins the block tags a file, an excerpt or a context cannot open or
  close. Not yet: a file's and an excerpt's text only defuse the block tags, so
  the same special-token class exists there, and Mistral-style `[INST]` markers are escaped
  nowhere. Where two user messages in a row go out as one (chat-voice design §7.4),
  the second's origin follows the first's text after a blank line.
- **D51. Blank is none, and an edit cannot clear an origin yet.** The gateway trims the sentence
  and stores none for a blank one; an edit that sends none or a blank one keeps the message's.
  Only a user message's is rendered; a reply carries none. The sentence has no length cap (no
  hidden limits): a long title goes whole into every turn, and what it costs shows in the turn's
  prompt tokens.
- **D52. The caption is one line under the bubble**, ellipsized, the whole sentence and what it
  is in its tooltip. The composer does not show yet what origin the next send will carry.
- **D53. `ChatHost::origin` is a plain `Arc<dyn Fn() -> Option<String> + Send + Sync>`**
  (`ChatOrigin`), like `ChatVoice::realtime_url`, not a reactive `Callback`: it lives as long as
  the host's value and needs no reactive owner. The Chat asks it once per send and per edit of a
  user message, trims it and leaves the field out when it is blank. A bound voice session's
  spoken turns carry no origin yet.
- **D54. The exports carry it.** The JSON export has `origin` on every message (`null` when
  none); the Markdown export writes it in italics under the message's heading, its Markdown
  metacharacters backslash-escaped so a page title cannot make a link or a heading there.
- **D55. New chat while another open is in flight uses the folder of the thread shown.** An open
  that has not landed is cancelled by the new choice, so the thread currently shown decides where
  the New chat goes. Draft call.

Not yet: realtime voice-session turns (`chat_voice/bound.rs` `write_user`) carry no origin, and
MCP and tool results are not neutralised against block-tag forgery (an existing class, not new
with the origin).

Tests: `chat_folders/goes.rs` (where a New chat goes: a folder, an ongoing folder, unfiled, none
open, an unknown folder, temporary, Admin; the label; the way back and its pick), `host.rs` (a
blank origin), `web/chat_origin.rs` (trimmed, blank is none, the block, a title that tries to close
it), `chat_turn.rs` (the origin leads a user message's parts on every replay, never a reply's, and
in a merged pair), `tests/it/chat_message_origin.rs` (the OpenAI-compatible, Anthropic and Gemini
requests on two turns, the thread's read, an edit keeping and replacing it, a blank one, Keep) and
`tests/it/migrations/chat_message_origin.rs` (0099 on an old row, the store's write, read and
rewrite). The drive `scripts/drive/chat-sidebar-head.json` (headless Chrome at `--dsf 1.25`, the
dashboard's navigation hidden so the Chat has the viewport, on a dev instance with the fake-echo
alias) checks at 338 and 280 px that the head's first line holds the list's button, New chat, the
model and the settings' button, the way back leads the second line, and nothing in the head is
wider than the column; New chat in a folder's thread lands in that folder, in Kai's makes its new
current conversation; the way back opens the current thread and goes; the list's marks; the
origin's one line under its bubble; and, wide, "Back to Kai" in the head and the list's New chat
naming the folder: 0 FAIL. The echo's reply shows the origin block reached the model before the
message's text. Not yet in `scripts/chat-drives.sh`.
