# lmgw for client apps: device keys, the Chat feed, ongoing folders, device-hosted MCP

**Draft v2, 2026-10-06**: v1 plus an adversarial review checked against main at 2baa764
(appendix "Review dispositions"). Paths are relative to the lmgw repository at 2baa764. Companion
records in `docs/design/` are cited by short name: "principals §n" (2026-09-22), "chat-complete
§n" (2026-09-30), "realtime §n" (2026-10-01), "chat-voice §n" (2026-10-03), "server-tools §n"
(2026-10-05) and "egress §n" (2026-10-06).

**Every change here is generic.** lmgw's core never knows a particular client. A client app is:
- a **device key**, holding a new **Chat** capability;
- a subscription to a **change feed**;
- a folder marked as an **ongoing conversation**;
- optionally an **MCP server it hosts** over its own outbound connection.

The first client is a desktop client on Plasma; the next is an Android app.

Each change is tagged with what needs it: **[Desktop 1]**, **[Desktop 2]**, **[Desktop 3]** (the
desktop client's phases) or **[Android]**.

| § | Change | Needed by |
|---|---|---|
| 1 | A. Device keys, the `Chat` capability, binding a voice session from a device | Desktop 1; the hosting grant (§1.5) Desktop 2; the QR code Android |
| 2 | B. The Chat change feed | Desktop 1 (thread, folder, hold and voice events, `resync`); Android (`message.*`, catch-up of them) |
| 3 | C. Ongoing-conversation folders | Desktop 1, Android |
| 4 | D. The `lmgw-client` crate and the documented Chat subset | Desktop 1 (the crate and the types it uses); Android (the full typed and documented subset) |
| 5 | E. Device-hosted MCP | Desktop 2 (overlay tools), Desktop 3 (desktop tools), Android (phone tools) |
| 6 | F. MCP approvals on Chat and bound realtime sessions | Desktop 3 |
| 7 | G. `/mcp` passes resources through; MCP Apps metadata | Desktop 2 |
| 8 | H. `ring.js` takes an optional transparent flag | Desktop 1 (optional: a client-side shim works) |

E is needed from Desktop 2 on, because tool-controlled overlays are tools the agent calls, and a
bound session's tools are the thread's (chat-voice §8.1).

## Decided by the owner (2026-10-06)

1. **Client apps talk to lmgw only through its network API**, the one a future Android client
   (chat + realtime) uses. An "extension" of lmgw is a client with a restricted key.
2. **A.** A key kind `device` and a capability covering:
   - Chat threads and folders;
   - voice binding;
   - `/v1` inference;
   - a small status subset (the hold state; reachability is the stream itself).

   Nothing in the `/api` admin plane. Device keys are:
   - named per device, revocable, with a last-seen time;
   - limited by the existing per-key tool scope, so a thread with the self-admin toolset attached
     cannot escalate a device;
   - paired through a dashboard QR code or link (URL + key, plus a certificate fingerprint if
     lmgw serves TLS);
   - optionally granted the right to host tools under a label (E).

   The key's name identifies the device in voice takeover messages.
3. **B.** One SSE stream per client, carrying:
   - threads created, updated, deleted;
   - messages appended, edited, cut;
   - turns started and done;
   - a voice session bound or taken over;
   - the hold toggled.

   A `since` cursor lets a client catch up.
4. **C.** A folder can be marked as one ongoing conversation, with an idle rollover as a visible
   setting. One route answers "the current thread of this folder", rolling over when idle or on
   demand. The client names its folder. The folder's defaults hold its voice settings and tools,
   and another device continues the same conversation.
5. **D.** The Chat API subset clients use is documented. It stays lmgw-native, not OpenAI
   Conversations.
6. **E.** The device connects out with its key and offers its MCP server over that connection.
   lmgw lists it like a registered MCP server while it is connected. Its tools then work from:
   - the device's own voice;
   - the dashboard chat;
   - `/mcp` clients;
   - agents;
   - other devices whose key names the label.

   Who among them reaches it is §5.6: never an anonymous caller, and never a key by a wildcard
   scope alone.
7. **F.** MCP approvals use OpenAI's `mcp_approval_request` / `mcp_approval_response` items.
8. **G.** `/mcp` passes `resources/list` and `resources/read` through, so a client can fetch the
   `ui://` resources other servers ship.
9. **H.** `ring.js` gets an optional transparent flag.
10. **Configured fallbacks are always used**, with no content-based exceptions. Privacy is chosen
    by picking an alias without a fallback. Only capability keeps content from a route.
11. **OpenAI-shaped routes copy OpenAI's protocol**; lmgw-only behaviour is opt-in.
12. **No hidden limits.** Every limit is a visible setting, a logged note or an explicit error.

## Taken in this draft (open to the owner's veto)

**L1. A device key is stored as a hash only**, like a client key. Its plaintext appears once, in
the pairing link. A lost key is re-paired with Rotate, which for a hash-only row is a new code path
(`key_rotate` refuses non-owner rows today, `web/api_settings.rs:1552-1558`). Owner and agent rows
keep plaintext only because lmgw must hand those keys out again (`config/keys.rs:56-64`).

**L2. One new capability, `Chat`, held by owner keys and device keys.** Every `/chat/api` route
moves from `Admin` to `Chat`, the three exports included: a device reads every thread it may see
through list and get anyway, so keeping exports owner-only would protect nothing. A device's
export omits what L3 hides. The dashboard is unaffected, because an owner holds `Chat`.

**L3. Admin Chat threads do not exist for a device.** One rule, enforced where a thread is
resolved:
- every route that reaches a thread by thread id, message id or attachment id answers a device
  404 for an admin thread, the voice bind included;
- collections (list, search, folder counts, the feed, exports) filter admin threads out;
- a device's folder delete with `threads: "delete"` leaves admin threads in place (out of the
  folder) and says how many it left.

*Why:* those threads drive the self-admin plane a device never holds, and their history is where
provider keys get pasted.

**L4. A turn a device starts runs as that device**, not as the gateway:
- every model call goes through `policy_checked_call` against the device key: disabled or deleted
  key, scope, budget, expiry, rpm and tpm;
- the rows are attributed to the device key;
- the tools are resolved under the device's tool scope.

A turn resumed after an approval runs as the principal that started it, whoever approved (L13).
The owner's own turns are unchanged (`internal:chat`, `ToolScope::gateway()`).

**L5. Tool labels written by a device are checked against its tool scope**, in a thread's
`mcp_tools` and a folder's default `mcp_tools`.
- The check requires `admits` for every tool the label exposes now: a connected server's live
  list, a built-in's namespace, a device-hosted label by L16's rule.
- A server without a tool prefix that is not connected cannot be checked, and is refused with
  that reason. `lmgw` is never allowed.
- **What this does not bound, stated plainly:** a device may write a thread's system prompt and
  any label within its own reach. The owner's later turns in that thread read them and run with
  the gateway's scope. A device's scope bounds the device's own turns, not the content the owner
  later reads.

**L6. The feed persists change records, not renderings.**
- A record is `{seq, at, type, thread_id, folder_id, message_ids, by}`, written by one store
  helper inside the change's own transaction.
- Delivery renders the current state at read time, or a tombstone for something gone.
- Turn, voice and hold events are live-only. `hello` carries the live state.
- The cursor is `"<epoch>:<seq>"` with an epoch per database. Catch-up streams in pages. A cursor
  the feed cannot honour gets an explicit `resync`. Retention and keep-alive are settings.

**L7. The feed carries no token deltas and no temporary threads.** Deltas belong to the send
stream or the bound session that asked for them; temporary threads are in memory only
(chat-complete §7).

**L8. "Current thread" is a `POST`** (it may create one).
- It runs under a per-folder lock, so two clients asking together get one thread.
- An empty current thread is reused, even on `new: true`.
- Idleness is measured from the newest message, not `updated_at`.
- The sweep skips current threads.
- Only `kind: "chat"` threads become current; a hand-created one does.
- A folder whose defaults name no model is a 409 `folder_no_model`: lmgw has no server-side default
  chat model, and two clients each passing their own would disagree on the conversation's model.

**L9. A defaults change on an ongoing folder also reaches its current thread** (opt-out per save).
In an ongoing conversation the current thread is the conversation; a voice change that waited for
the next idle rollover would look broken. Other folders keep "a copy at creation".

**L10. The `lmgw-client` crate lives in lmgw's workspace** and takes over the dashboard's pure
realtime rules (`crates/lmgw-ui/src/pages/chat_voice/realtime/machine.rs`, `protocol.rs`), so the
dashboard and every client run one set. The subset's typed DTOs and `DocRoute`s are built as
clients need them: the desktop client's in Desktop 1, the rest for Android (R25).

**L11. Device-hosted MCP is an lmgw transport binding, not an MCP transport.** The device opens a
WebSocket to `GET /mcp/host`, and lmgw is the MCP client on it.
- The server it reaches is an `mcp_servers` row of transport `device`, owned by the key the way
  agent rows are owned by their agent.
- Its message and frame sizes are settings. A second link from the same key takes over.
- An offline device's label is reported and its tools are skipped; it never refuses a turn by
  itself.
- Every call lmgw stops waiting on is cancelled to the device.
- Sampling is refused on device rows.

**L12. lmgw stamps `_meta["lmgw/caller"]` and `_meta["lmgw/approval"]`** on every call it forwards
to a device-hosted server, and on no other server's calls. The approval says who decided. The
device can then apply its own policy, trusting the callers and approvers it chooses, without asking
twice.

**L13. A gated Chat turn ends with its pending calls stored on the thread**, with the principal
that started it.
- Any `Chat` client resumes it with decisions; the first decision wins.
- The approver is recorded on the call's row, in `approval.decided` and in `_meta`.
- A new message in the thread declines what is still pending.
- A bound voice session sees OpenAI's items and answers with them.

**L14. MCP Apps on `/mcp`.**
- Resource URIs are namespaced by the server's tool prefix, consistently in `resources/list`,
  `resources/read`, tools' `_meta.ui.resourceUri` and resource references in tool results.
- `/mcp` advertises `resources` and the MCP Apps extension capability on the protocol revisions it
  already speaks, and adds no revision.
- Rewriting URIs in results departs from server-tools decision 7 ("results pass verbatim"); that
  is the only departure.

**L15. `last_seen_at` is written when a device's connection opens or closes**, not per request.

**L16. A device-hosted label is reachable only:**
- by the owner (and the gateway's own runs on the owner's behalf);
- by the device that hosts it;
- by principals whose tool scope names that label explicitly.

It is never in a default, `all`, `deny` or anonymous scope, whatever *Require API key* says. §5.6
gives the rule and how a browser page is kept away.

**L17. Expiry is checked on `Chat` routes too** (401 `key_expired`), and the revocation signal fires
at `expires_at`, so a device's open feed, link and sessions end then.

**L18. Revocation is said, not implied.** The feed sends `revoked {reason}` before it closes; the
WebSocket links close with 4003 and a reason.

## What exists today (relied on, not re-derived)

**Principals and keys**
- **`Principal`** is `Anonymous` or `Key {id, name, kind, agent_id}`
  (`crates/lmgw-core/src/principal.rs:26-41`).
- **`Cap`** is `Public | Inference | Ledger | AgentSelf | Admin` (`:45-52`).
- **`holds`** encodes principals §3.2 (`:105-134`). Two arms are written negatively: owner
  `cap != Ledger` (`:122`) and agent `!matches!(cap, Admin)` (`:126`).
- **`describe`** ends in a catch-all that calls any other key "a client API key" (`:159`).
- **A matched disabled row** is refused by name (`:354-369`). An unmatched bearer is `Anonymous`;
  on a non-`Public`, non-`Inference` route that answers 401 `session_required` with the dashboard's
  login text (`:181-188`; `server.rs:1110-1126`). The cookie is honoured only for owner rows
  (`:375-391`) and only same-origin (`:410-436`). The listener speaks `http://` only
  (`:403-406`).
- **`ApiKeyKind`** is `Key | Internal | Agent | Owner` (`config/keys.rs:74-108`). `KeyPolicy` carries
  the alias scope, the tool scope (default mode `all`), the budget, rpm, tpm, concurrency and
  expiry (`:111-131`, `:133-140`).
- **`last_used`** on the Keys page is the newest usage hour (`web/api_usage.rs:886-893`).
- **`key_rotate` refuses non-owner rows** (`web/api_settings.rs:1552-1558`).
- **`CAPABILITY_TABLE`** runs from `server.rs:189` to `:420`. Every `/chat/api` row is `Admin`
  (`:337-384`), and `/v1/realtime` is `Inference` (`:219`).
- **The gate** decides at `:1110-1126`. Expiry, rpm and concurrency (`policy::admit`) run on
  `Inference` routes only (`:1127-1134`).
- **The full per-call check** is `policy_checked_call` (`proxy/recording.rs:411-445`): it re-reads
  the key by id, refuses a disabled or deleted key, runs scope and budget, then `count_call`
  (expiry, rpm, tpm; `policy.rs:220-240`, `usable` at `:334-351`). `check_alias` alone is scope and
  budget.
- **CORS** is `CorsLayer::permissive()` with request headers mirrored since 2baa764
  (`server.rs:120-135`). A browser page on any origin can call `/v1` with whatever credential it
  has, and with none while *Require API key* is off.

**Tool scope**
- **`ToolScope`** (`mcp/scope.rs:10-27`).
  - Self-admin is derived from `Cap::Admin` (`:86`).
  - `principal_list` gives `Anonymous`, owners and client keys of mode `all` the list `All`
    (`:245-267`); agents get their manifest's labels.
  - `may_reach` answers true for any server without a tool prefix (`:140-150`).
- **The gateway's own runs use `ToolScope::gateway()`:** Chat tool turns (`web/agentchat.rs:487-490`)
  and in-process agent batches (`agents/batch.rs:661`).
- **Every plane that hands tools out resolves under the caller's scope:**
  - `/mcp` (`mcp/ingress.rs:248-262`);
  - `/v1/responses` (`responses.rs:718-724`);
  - realtime `mcp` tools (server-tools §1);
  - `/v1/mcp/servers` (server-tools §1.4).

  `/mcp` refuses foreign browser origins (`mcp/ingress.rs:995-1044`); `/v1/responses` checks no
  `Origin`.

**Chat**
- **Chat handlers take no `RequestCtx`** (`web/chat.rs:725-729`, `send`).
- **A turn records under `internal:chat`** (`telemetry.rs:390-392`); recording prefers a key
  reference when one is given (`proxy/recording.rs:266-273`).
- **`TurnOpts`** carries stop, voice, language, heard, began, spoken and the user row, and no caller
  (`web/chat_turn/out.rs:105-140`).
- **A label that fails to resolve is reported. If no tool is left, the whole turn is refused**
  (`web/agentchat.rs:497-530`).
- **lmgw has no server-side default chat model.** Creating a thread takes `model_alias` from the
  request (`web/chat.rs:103-129`); folder defaults override it only when they name one
  (`web/chat_folders.rs:281-301`).
- **Folder defaults are a copy taken at thread creation** (`store/chat_folders.rs:8-11`). A folder
  patch never touches existing threads (`web/chat_folders.rs:160-208`). Folder names are not
  unique (`migrations/0048_chat_folders.sql`).
- **History writes go through `LiveTurns`:** `write`, `begin` and `save_lock` move a per-thread
  generation and cancel the live turn (`web/chat_live.rs:17-27`, `:143-215`). A text send from
  any window cancels the thread's running turn, voice turns included.
- **The bound session.**
  - **One per thread; a second bind takes over** (`web/chat_live/voice.rs:53-76`), and the older
    session is told "voice mode moved to another window" (`realtime/thread.rs:67-73`,
    `realtime/thread/hooks.rs:57-66`).
  - **The bind requires `Cap::Admin`** (`realtime/thread/bind.rs:86-96`), refuses Admin Chat
    (`:113-121`) and skips realtime's policy check (`:19-23`).
  - **Its per-call key checks "pass trivially"** (chat-voice §8.8).
  - **Item creates and deletes are refused `owned_by_thread`** (`realtime/thread/owned.rs:120-130`).
  - **Its `lmgw.*` events** are listed at the end of chat-voice §8.6. "The binding is the
    opt-in".
- **Search covers every thread kind** (`store/chat_search.rs:117-118`). Attachments are reached by
  attachment id (`server.rs:379-384`).
- **The Chat sweep** archives after `chat_archive_days` (14) and purges `chat_purge_days` (30)
  later; pinned threads are exempt (`config/settings.rs:238-250`, `server.rs:687-702`,
  `store/chat.rs:381-414`).
- **No route pushes Chat changes.** The page learns of a turn only through its own `send` stream
  (`crates/lmgw-ui/src/pages/chat_stream.rs:9-46`). `/api/events` is `Admin`, carries no Chat or
  hold frames, and drops frames a slow subscriber lagged on (`web/api.rs:1459-1585`, `:1533`).
- **The hold** is `settings.hold` (`config/settings_classes.rs:71-79`). `active` is written by
  `ops::hold_set` (`ops/hold.rs:27`); `fallback_alias` also by the settings patch
  (`ops/settings_patch.rs:251-259`).
- **The Chat mini-API is excluded from the OpenAPI document** by the owner's 2026-09-28 decision
  (`openapi/exclusions.rs:9-14`).

**MCP**
- **`McpTransport` is `Stdio | Http | Sse`** (`config/mcp.rs:15-19`). An `mcp_servers` row may be
  owned by an agent (`agent_id`, `:82-86`, `store/mcp_servers.rs:34`).
- **Reserved namespaces:** `lmgw`, `docs`, `kb` (`mcp/mod.rs:397`). The lazy-list budget is 10 s
  (`:92`). rmcp is 2.0 (`crates/lmgw-core/Cargo.toml:108`).
- **`/mcp`**:
  - it speaks MCP 2025-11-25, 2025-06-18 and 2025-03-26 (`mcp/ingress.rs:59-61`);
  - it advertises `tools` only (`:758-764`);
  - it handles `initialize`, `ping`, `tools/list` and `tools/call` and answers anything else
    `-32601` (`:766-823`);
  - it passes tools through as rmcp serializes them, `_meta` included (`:248-254`);
  - it refuses batches (`:577-580`).
- **Sampling from a server** runs under `internal:mcp-sampling` (`mcp/handler.rs`,
  `telemetry.rs:403`).
- **Approvals exist in the agent loop:**
  - `ResolvedTool::needs_approval` (`agent.rs:64-98`);
  - `PendingCall`, with a gated call's siblings held (`:100-116`);
  - `DecidedCall {call, approved, denial}`, no identity (`:136-144`);
  - `StopReason::Approval` (`:155-166`);
  - `/v1/responses`' verdict rules (`responses.rs:378-432`).

  The Chat stores no `require_approval` (`store/chat.rs:7-22`). Realtime refuses a client's
  `mcp_approval_*` items (`realtime/conversation/mcp.rs:143`).
- **A Chat `tool` result frame** carries flattened text only (`web/agentchat.rs:363-377`).

**MCP Apps** (not in lmgw today). The extension's specification is `modelcontextprotocol/ext-apps`,
`specification/2026-01-26/apps.mdx` (stable 2026-01-26; SEP-1865), read 2026-10-06:
- hosts advertise the extension `io.modelcontextprotocol/ui` with a `mimeTypes` list;
- a UI resource is `text/html;profile=mcp-app` under `ui://`;
- a tool links to it with `_meta.ui.resourceUri`, and `_meta.ui.visibility` (`["model"]`,
  `["app"]` or both) says who may call it;
- a resource's `_meta.ui` carries CSP domains (`connectDomains`, `resourceDomains`,
  `frameDomains`, `baseUriDomains`) and permissions;
- the view talks to its host in JSON-RPC over postMessage: `ui/initialize`, the `ui/notifications/*`
  set, and proxied `tools/call` and `resources/read`.

The brief named "MCP 2026-07-28"; this draft found MCP Apps only as that extension, so the
spec pins the extension document above. G's first step re-reads it (§7.4).

**Voice and the visualisation**
- **The visualisation contract** is chat-voice §10. `ring.js` asks for `alpha: false` (`:28`) and
  fills its background every frame (`:72`); `engine.js` hands the factory `{palette}` (`:342`).
- **`chat_voice_audio_input`** is `off | on` (`config/settings.rs:302-311`; a stored `local`, its
  name before 2026-10-06, reads as `on`), decided by capability only. A client inherits it
  through its folder; nothing here depends on it.

## 1. Device keys and the Chat capability (A) [Desktop 1]

### 1.1 The kind

- **`ApiKeyKind::Device`**, spelled `device`.
  - Name `device:<name>`; the server adds the prefix idempotently, as it does for `owner:`
    (principals §3.12).
  - Plaintext `lmgw-device-<64 hex>`, so a leaked string says what it is.
  - Hash only (L1).
- **Migration** (the next free number): `api_keys.last_seen_at TEXT NULL` and
  `api_keys.hosts_label TEXT NULL`.
- **`KeyPolicy` applies whole** (L4, §1.3).
  - The pairing form shows the alias scope, budget and tool scope, prefilled `all`, no budget.
    The card marks an unscoped, unbudgeted device in amber (§11 Q1).
  - A device's own scope never reaches self-admin, because it never holds `Admin` (`mcp/scope.rs:86`).
  - It never reaches another device's hosted label unless it names it explicitly (L16).
- **`principal_list`** (`mcp/scope.rs:245-278`) gives `Device` the client key's arm. The match is
  exhaustive, so the new kind cannot slip into "reaches nothing" unnoticed.

### 1.2 The capability

`Cap::Chat` joins `principal.rs:45-52`. **Every arm of `holds` is written positively** (R16), so a
new capability is held by nobody until an arm names it:

| Principal | `Public` | `Inference` | `Chat` | `Ledger` | `AgentSelf` | `Admin` |
|---|---|---|---|---|---|---|
| owner | yes | yes | **yes** | no | yes | yes |
| device | yes | yes | **yes** | no | no | no |
| agent | yes | yes | **no** | yes | yes | no |
| client key | yes | yes | no | no | no | no |
| internal | yes | no | no | no | no | no |
| anonymous | yes | while *Require API key* is off | no | no | no | no |

`describe` gets a device arm: "a device key ('<name>')".

`CAPABILITY_TABLE` (`server.rs:337-384`):
- **To `Chat`:** every `/chat/api` row, the three exports included (L2).
- **New rows:**
  - `GET /chat/api/feed` (§2), `Chat`;
  - `POST /chat/api/folders/{id}/current` (§3), `Chat`;
  - `GET /mcp/host` (§5), `Chat` [Desktop 2];
  - `POST /chat/api/threads/{id}/approvals` (§6), `Chat` [Desktop 3].
- **`/v1/realtime?chat_thread=`** checks `Chat` instead of `Admin` (`bind.rs:86-96`).
- **Unchanged and closed to devices:** `/api`, `/api/op`, `/api/events`, `/audio-lab/api`,
  `/image-lab/api` and `POST /mcp/admin`.
- **On `Chat` routes the gate also runs the key's expiry check** (`policy::usable`), answering 401
  `key_expired` with the date (L17). The gate takes no concurrency slot on `Chat` and counts no
  rate window: a feed or a host link holds no slot, and the model calls inside a turn are checked
  one by one (§1.3).

### 1.3 What a device's turn runs under (L3, L4, L5)

**The caller reaches the turn.**
- The Chat handlers take the request's `RequestCtx`, and `TurnOpts` gains the caller
  (`web/chat_turn/out.rs:105-140`).
- An owner principal keeps today's behaviour exactly.
- A device principal is described in the rest of this section.

**Model calls.**
- Each model call of a device's turn goes through `policy_checked_call` (`proxy/recording.rs:411-445`)
  against the device key and is recorded under it. That covers:
  - the chat model;
  - ASR and TTS;
  - a knowledge base's embedder and reranker.
- A refusal ends the turn with the gateway error it was, as `gpu_hold` does today (chat-voice §8.2).

**Tools.**
- `agentchat` resolves with `ToolScope::of_request` instead of `ToolScope::gateway()`
  (`web/agentchat.rs:487-490`).
- A label the scope keeps out is reported to the turn like a server that could not be reached,
  never dropped silently. Under L16 that covers another device's label the scope does not name.

**Writes** (L5).
- A device's `mcp_tools`, in a thread's settings or a folder's defaults, pass the check in L5;
  otherwise 403 `tool_label_out_of_scope`, naming the label and the tool that failed.

**Admin Chat** (L3).
- One check in `ChatRepo`'s thread resolution, used by every thread-, message- and
  attachment-addressed route: for a device principal, an admin thread is "not found".
- The list, search (`store/chat_search.rs`), folder counts, feed and exports filter admin threads
  for a device.
- Creating `kind: "admin"` is a 403 `forbidden`.

**Binding from a device.**
- Before the 101, the handshake runs realtime §10.2's alias check for the thread's chat, ASR and
  TTS aliases, through `policy_checked_call`. Today a bound session skips it (`bind.rs:19-23`).
- The session's per-call checks run as for an unbound session (realtime §10.3); chat-voice §8.8's
  "pass trivially" is corrected.
- The session takes one slot of the key's `concurrency_limit` for its life.
- A device's bind refusals write a request row, as realtime's own handshake refusals do.
  chat-voice §8.8 NIT 12 exempted them only as "the dashboard's own" requests.

**A resumed turn** (§6.3) runs as the principal stored with its pending calls (L13), not as the
approver.

**Across devices** the last writer wins, as across windows today: a text send cancels the thread's
running voice turn (`web/chat_live.rs:24-27`). Clients see it live (`turn.done {code:
"superseded"}`). A client that acts by itself (automatic listening) should hold back while
another principal's turn runs. The feed gives it what it needs to do that (`turn.started` with
`by`).

### 1.4 Pairing

- **The card.** Usage → Keys gains a **Devices** card (§11 Q6). It lists:
  - the device rows with their state (online with which connections, or last seen);
  - their scopes, budget and hosting label;
  - who reaches each hosted label (§5.6);
  - Disable, Rotate, Delete.
- **Destructive actions confirm, then do it.** Disable, Rotate and Delete each warn what they end
  ("its feed, voice session and tool link close now; the device must be paired again").
- **"Pair a device"** asks for a name, the scopes and budget (shown, §1.1) and an optional hosting
  label (§1.5). It mints the key and shows once:

  ```
  lmgw-pair:?v=1&url=http%3A%2F%2F127.0.0.1%3A8001&name=desktop&key=lmgw-device-…
  ```

  The QR code of the same string is [Android].
- **`url`** is prefilled from `net::primary_base_url(bind_addr)` and editable, because a phone needs
  the tunnel's name. A loopback URL carries the note "reachable from this computer only".
- **`fp=sha256:<hex>`** is reserved for a TLS listener. lmgw serves none today, so no link carries
  it. A client must verify it when present.
- **Guidance for clients:** the link carries a credential. A client must not log it (a URL scheme
  handler receives it on argv), keeps the key only in its platform's secret store, and drops the
  link once stored.
- **Ops** (`/api` only, absent from the `lmgw__*` plane as the other key ops are, principals
  §3.12):
  - `key_create {kind: "device", name, hosts_label?, policy}` returns `{key, link}`;
  - `key_rotate` on a device mints a new hash, returns a new link and ends the old key's
    connections (§1.6);
  - `key_set` takes `hosts_label` and the policy fields;
  - `key_reveal` refuses device rows (L1).

### 1.5 The hosting grant [Desktop 2]

- **`hosts_label`** is the label (tool prefix) the device may host tools under. It is validated like a
  `tool_prefix`: unique among the `mcp_servers` rows' prefixes and names, and not reserved
  (`mcp/mod.rs:397`).
- **Setting it creates the device's server row** (§5.2); clearing it deletes the row.
- **Without a grant,** `GET /mcp/host` is a 403 `host_not_granted`.

### 1.6 Revocation, expiry, last seen (L17, L18)

- **A disabled device** is a 401 `device_disabled`: "device '<name>' is disabled — enable it on
  Usage → Keys". It is a new arm of `principal.rs:354-369`. A rotated or deleted key matches no row,
  so its bearer is `Anonymous`, and `Chat` routes answer 401 `session_required`.
- **Disable, Rotate, Delete and `expires_at` end every open connection** of the device at once:
  - the feed sends `event: revoked` with `{reason}`, naming which of the four happened, then
    closes;
  - `/mcp/host` and realtime sessions close with 4003 and the same reason.

  One revocation signal keyed by key id is watched by those long-lived tasks; expiry raises it
  from a timer.
- **`last_seen_at`** (L15) is written when a feed, link or realtime connection of the device opens or
  closes. The card shows "online (feed, voice, tools)" while any is open. `last_used` stays the
  usage hour.

### 1.7 Naming the binder

- **`LiveTurns::bind_voice`** (`web/chat_live/voice.rs:53-76`) takes the binder's description and
  keeps it with the binding.
- **The session taken over names it,** in the `chat_thread_taken_over` message
  (`realtime/thread/hooks.rs:57-66`) and the close reason (`realtime/thread.rs:73`): "voice mode
  moved to device 'phone'", or "…to the dashboard" for an owner session.
- **The feed's `voice.ended`** carries the same `by`.

### 1.8 Refusals

| HTTP / close | `code` | When |
|---|---|---|
| 401 | `device_disabled` | the matched device row is disabled |
| 401 | `key_expired` | a device key past `expires_at`, on any route |
| 401 | `session_required` | a rotated or deleted device key (it matches no row) |
| 403 | `forbidden` | a device on an `Admin` route, or creating an admin thread |
| 403 | `tool_label_out_of_scope` | a device writes a label L5 refuses |
| 403 | `host_not_granted` | `GET /mcp/host` without a hosting grant, or from a non-device principal |
| 403 | `cross_origin_refused` | `GET /mcp/host` with an `Origin` header (§5.6) |
| 404 | `not_found` / `chat_thread_not_found` | a device reaching an admin thread by any id (L3) |
| 409 | `folder_no_model` | `current` on a folder whose defaults name no model (§3.3) |
| close 4003 | `revoked` | §1.6 |

### 1.9 Documents A updates

- the `/v1/realtime` `DocRoute`'s "`chat_thread` (dashboard only)" and its refusal list;
- the `bind.rs` module-doc table;
- chat-voice §8.1's refusal table, and §8.8's "pass trivially" and NIT 12;
- the labs' doc comment ("no key of their own", `web/mod.rs:147-149`);
- principals §3.2's table, with a pointer to this record.

## 2. The Chat change feed (B) [Desktop 1, Android]

### 2.1 Route

`GET /chat/api/feed`, SSE, `Chat` capability.
- **Resuming:** `?since=<cursor>`, or the standard `Last-Event-ID` header. A cursor is
  `"<epoch>:<seq>"`. With neither, the feed starts at "now".
- **A stored event** is an SSE record with `id: <cursor>`, `event: <type>`, `data: <json>`. Live-only
  events carry no `id`.
- **The first record is `hello`:**

  ```json
  {"epoch": "…", "cursor": "<epoch>:<seq>", "keepalive_s": 15,
   "principal": {"kind": "device", "name": "desktop"}, "hosts_label": "desktop" | null,
   "hold": {"active": false, "fallback_alias": null},
   "voice": [{"thread_id": 812, "by": "device 'phone'"}],
   "turns": [{"thread_id": 812, "by": "the dashboard", "voice": false}]}
  ```

  `hold` is A's status subset; reachability is the stream itself. `voice` and `turns` are the live
  state now.
- **Keep-alive comments** every `chat_feed_keepalive_s` (a setting in Settings → Chat, default 15,
  named in `hello`), so a client derives its dead-link timeout from a stated value.

### 2.2 Events

| `event` | `data` at delivery | Kind | Phase |
|---|---|---|---|
| `thread.created`, `thread.updated` | the thread as the list returns it now | stored | Desktop 1 |
| `thread.deleted` | `{thread_id}` | stored | Desktop 1 |
| `folder.created`, `folder.updated` | the folder as the list returns it now | stored | Desktop 1 |
| `folder.deleted` | `{folder_id}` | stored | Desktop 1 |
| `folder.current` | `{folder_id, thread_id, previous_thread_id, reason}` | stored | Desktop 1 |
| `message.added`, `message.updated` | `{thread_id, messages}` as `GET …/threads/{id}` returns them now | stored | Android |
| `message.deleted` | `{thread_id, message_ids}` | stored | Android |
| `approval.requested`, `approval.decided` | §6.6 | stored | Desktop 3 |
| `turn.started` | `{thread_id, by, voice}` | live | Desktop 1 |
| `turn.done` | `{thread_id, message_id, saved, code}` (`superseded`, `gpu_hold`, … or null) | live | Desktop 1 |
| `voice.bound` | `{thread_id, by}` | live | Desktop 1 |
| `voice.ended` | `{thread_id, by, reason}` (`closed`, `taken_over`, `thread_gone`, `revoked`) | live | Desktop 1 |
| `hold` | `{active, fallback_alias}` | live | Desktop 1 |
| `state` | `hello`'s live part again | live | Desktop 1 |
| `resync` | `{reason}` | live | Desktop 1 |
| `revoked` | `{reason}`, then the stream ends | live | Desktop 1 |

**What writes each event:**
- **`thread.*`** come from create, Keep, rollover, settings, title, pin, archive and restore, move,
  delete, and the sweep. The sweep's archive step emits `thread.updated` for each id it archived
  (`RETURNING id`); its purge emits `thread.deleted`.
- **`folder.*`** come from the folder routes and §3.
- **`message.*`** come from:
  - a user message;
  - a saved reply;
  - edit and continue;
  - a voice reply's heard cut (chat-voice §8.3);
  - delete, and the truncation of an edit or regenerate.
- **`hold`** is emitted whenever a published snapshot's `hold` differs from the one before, so a
  fallback change made through the settings patch is seen too (R17).

`by` is the principal's description (§1.7).

### 2.3 Storage and delivery (L6)

- **Stored events are change records:** `chat_feed (seq INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT,
  type TEXT, thread_id INTEGER NULL, folder_id INTEGER NULL, message_ids TEXT NULL, by TEXT NULL)`.
  - **Written by one helper.** `store::feed::record(&mut tx, …)` runs inside the change's
    transaction. A single-statement write becomes a two-statement transaction, so a failed write
    records nothing and `seq` order is commit order.
  - **The epoch** is a random value stored once per database (a one-row meta table, set by the
    migration).
- **Rendering happens at delivery.** `thread.*` renders the thread as it is now, or `{thread_id,
  deleted: true}`; `message.*` renders the listed messages as they are now, omitting gone ones.
  - Catch-up therefore never replays a stale copy (`purge_at`, `voice_resolved`), and no message is
    stored twice.
  - Two events for one thread may render the same state; clients apply them idempotently.
- **Delivery reads the table.** A `watch` channel carries the newest `seq`. Each subscriber reads
  `seq > cursor` in pages whenever it moves, until it has caught up. The page size bounds memory,
  not delivery: every row is sent. A slow subscriber falls behind and loses nothing.
- **Live-only events** go through a broadcast. A subscriber that lags on it gets a fresh `state`
  frame, never a silent drop. After a lmgw restart nothing claims a turn is still running: live
  state is `hello`'s.
- **Filtering per principal** happens at read: a device sees no admin thread (L3).
- **Retention:** `chat_feed_retention_days` (Settings → Chat, default 7, `0` keeps all), pruned on
  the hourly maintenance tick that runs the Chat sweep (`server.rs:687-702`).

### 2.4 `resync`

A cursor whose epoch is not this database's, whose `seq` is older than the oldest kept row, or
newer than the newest, gets `resync {reason}`, then events from now.
- The reason names the cause, e.g. "the cursor is older than the feed keeps (Settings → Chat →
  feed retention: 7 days)" or "the cursor is from another database".
- The client reloads what it shows. Nothing is skipped silently.

### 2.5 Not in the feed (L7)

- **Token deltas, reasoning, tool progress, speech:** these belong to the `send` stream and the
  bound session.
- **Temporary threads.**
- **Attachments**, beyond what a message row carries.

Live mirroring of another client's turn is §Later.

## 3. Ongoing-conversation folders (C) [Desktop 1, Android]

### 3.1 The folder

- **Migration:**
  - `chat_folders.ongoing_idle_minutes INTEGER NULL`. NULL: not ongoing. 0: ongoing, rolling over
    only on request. N > 0: rolling over after N minutes without a message.
  - `chat_folders.current_thread_id INTEGER NULL`, referencing `chat_threads(id)` `ON DELETE SET
    NULL`.
- **The folder JSON** gains `ongoing: {idle_minutes, current_thread_id} | null`.
- **`POST /chat/api/folders/{id}`** takes `ongoing: {idle_minutes} | null`.
- **Marking a folder ongoing requires its defaults to name a model**; a patch without one is a 400
  naming the field. The form marks the model as required.

### 3.2 The current thread

`POST /chat/api/folders/{id}/current` takes `{new?: bool}` and returns
`{thread, rolled_over, reason}`.
- `thread` is the thread as `GET /chat/api/threads/{id}` returns its `thread` object.
- `reason` is `first`, `gone`, `idle`, `requested` or `null`.
- An unknown folder is a 404, a folder that is not ongoing a 409 `not_ongoing`, a folder whose
  defaults name no model a 409 `folder_no_model` ("set the folder's model in its defaults").
- The route holds a per-folder async lock from read to write (L8).

### 3.3 Rollover rules

1. **No current thread,** or one that was deleted, moved out or archived by hand: a new thread
   (`first` or `gone`).
2. **`new: true`:** a new thread (`requested`), unless the current one has no messages. Then it is
   reused, with `rolled_over: false`, so no pile of empty threads builds up.
3. **`idle_minutes > 0`** and the current thread's newest message is older than that: a new thread
   (`idle`). A thread with no messages is never idle. Idleness is measured from the newest
   message's `created_at`.
4. **Otherwise** the current thread.

- **A new thread** is `create_in_folder` (`web/chat_folders.rs:281-312`) with the folder's model.
  It becomes the current thread and records `thread.created` and `folder.current`.
- **A `kind: "chat"` thread created by `POST /chat/api/threads {folder_id}`** in an ongoing folder
  becomes its current thread (`folder.current`, reason `requested`). An admin thread created there
  does not, and neither does a thread moved in.
- **When the current thread is deleted,** `ON DELETE SET NULL` clears the pointer; the delete records
  `folder.current` with `thread_id: null`, so clients learn of it without asking.
- **A bound voice session on the old thread is left alone.** It keeps its thread; clients follow
  `folder.current`.

### 3.4 Defaults and the current thread (L9)

Folder defaults stay a copy at creation. For an ongoing folder, `POST /chat/api/folders/{id}` with
`defaults` also applies the changed fields to the current thread, through the thread settings
route's own checks (sampling, reasoning, voice aliases, knowledge bases, and L5 for a device),
unless the body says `apply_to_current: false`. The response names what it applied. A field the
thread refuses fails the whole patch, with the thread's message.

### 3.5 Sweep

`sweep_chat_threads` (`store/chat.rs:381-414`) skips every folder's current thread, as it skips
pinned threads. An ongoing folder's past threads archive and purge by the global settings (§11 Q2).

### 3.6 Dashboard

- **The folder form** gains "Ongoing conversation", with "new thread after [N] idle minutes (0:
  only on request)", the model marked as required, and "Also apply to the current thread", checked
  by default.
- **The sidebar** marks the current thread.
- **The folder menu** gains "New conversation", which sends `current {new: true}`.

## 4. The `lmgw-client` crate and the documented Chat subset (D)

### 4.1 The crate [Desktop 1] (L10)

- **Place:** `crates/lmgw-client` in lmgw's workspace.
- **Shape:** sans-IO, depending on `lmgw-api-types` and `serde_json` only. It builds for wasm and
  native, and its public types are owned and FFI-friendly for a later UniFFI wrapper. It reads no
  clock (time is passed in) and performs no I/O.
- **Contents:**
  - the realtime protocol (moved from `crates/lmgw-ui/src/pages/chat_voice/realtime/protocol.rs`);
  - the derived voice state and the truncate-first rule (moved from `machine.rs`);
  - truncate bookkeeping from a playback cursor;
  - feed frames and the cursor;
  - request builders for the routes a client uses.
- **Users:** the dashboard's voice panel uses it. Clients depend on it by path or by a tagged
  release; Android later through UniFFI or as the reference for a port.

### 4.2 The subset

Every route `Chat` holds (§1.2):
- **threads:** list, create, get, settings, delete, pin, archive, move, persist, continue,
  export;
- **turns:** send, voice warm, transcribe, speech stop;
- **messages:** edit, delete, regenerate, speak;
- **attachments:** upload, delete, get, mode, transcribe, text;
- **search;**
- **folders:** list, create, patch, delete, current, export;
- **the feed;**
- **approvals** [Desktop 3].

Beside it, on `/v1`:
- `/v1/realtime?chat_thread=` (chat-voice §8);
- `/v1/embeddings`, `/v1/models`, `/v1/audio/voices`;
- `/v1/mcp/servers` (server-tools §1.4).

### 4.3 Typed and documented [Desktop 1 for what the desktop client uses; Android for the rest]

- **Typed DTOs.** The Chat JSON is built with `json!` today (`web/chat.rs` `thread_json`,
  `thread_row_json`, `get_thread` at `:227-262`). Its types move into `lmgw-api-types` (`chat.rs`),
  so the document and the code cannot drift.
  - **Desktop 1:** `ThreadRow`, `Thread`, `Folder`, `Current`, `FeedEvent` and `ApiError`.
  - **Android:** `Message`, `Attachment`, `SendRequest` and the send stream's frames.
- **Documentation.** Each route gets a `DocRoute` under a "Chat" tag, and the API docs page lists
  them. The Chat block leaves `openapi/exclusions.rs`.
- **This reverses, for the subset, the owner's 2026-09-28 decision** that the dashboard's backend
  is not a contract (`exclusions.rs:9-14`). The owner's 2026-10-06 decision D is the reason.
- **Compatibility.** The routes stay lmgw-native: not OpenAI Conversations, no `/v1` prefix.
  Changes are additive within a release line; a breaking change is a new route or field.

## 5. Device-hosted MCP (E) [Desktop 2, Desktop 3, Android]

### 5.1 The link (L11)

- **`GET /mcp/host`**, a WebSocket upgrade.
  - The layer requires `Chat`.
  - The handler requires a device principal with `hosts_label` (§1.5), otherwise 403
    `host_not_granted`. An `Origin` header is refused 403 `cross_origin_refused`: devices are
    native clients (§5.6).
  - One JSON-RPC message per text frame, no batches (as `/mcp`, `mcp/ingress.rs:577-580`).
- **An lmgw transport binding, not an MCP transport.** MCP defines stdio and Streamable HTTP. Here
  the server dials the client over a WebSocket and the roles reverse at the message level. An MCP
  SDK on a device needs a small custom server transport for it, and an Android SDK will too.
- **lmgw is the MCP client.**
  1. lmgw sends `initialize` with the newest protocol version it speaks.
  2. The device answers with its capabilities (`tools`, and `resources` for its own `ui://`
     resources).
  3. lmgw sends `notifications/initialized` and lists the tools, paging as for any server.
- **One mechanism, where it can be.** The WebSocket is wrapped as an rmcp transport (rmcp 2.0), so
  the aggregate and `GatewayClientHandler` are reused. The manager's connect, lazy-list and reap
  paths gain a `Device` arm that waits for an inbound link instead of dialling.
- **Sizes:** `mcp.host_max_message_mb` and `mcp.host_max_frame_mb` are set explicitly on the
  WebSocket, defaulting to realtime's values (realtime §10.4) and shown in Settings → MCP. An
  overrun closes the link with a reason naming the setting. A full-desktop screenshot is the
  expected large message.
- **Liveness:** a WebSocket ping every `mcp.host_ping_interval_s` (default 20). A missed pong closes
  the link with a reason naming the setting.

### 5.2 The row

- **`McpTransport::Device`**, spelled `device` (`config/mcp.rs:15-19`), with
  `mcp_servers.device_key_id`, owned by the key as agent rows are owned by their agent (`:82-86`).
  - Created when the grant is set, kept in step (prefix = `hosts_label`, name = the device name),
    deleted with the key.
  - The URL and command fields are unused.
- **The row's other fields:**
  - `timeout_ms` defaults to 60 000 and is editable;
  - `idle_seconds` is 0 (never reaped);
  - `allow_sampling` cannot be set on a device row. Sampling would spend under
    `internal:mcp-sampling`, outside the device key (R26). Its request is refused `-32601` with
    that reason.
- **Per-tool hide and rename** and the owner's tool switches apply as for any row. The MCP page shows
  the row with its device and a "device" transport chip.

### 5.3 Status

| Link | Status |
|---|---|
| none | `Stopped`, detail "device offline (last seen …)" |
| `initialize` and listing | `Connecting` |
| linked and listed | `Ready` |
| `initialize` or listing failed | `Error` with the message |

- **A second link from the same key takes over.** The older link is closed 4000 "another
  connection of device '<name>' took over".
- **Offline devices.** Resolving a label of an offline device answers at once, without
  `LAZY_LIST_BUDGET`'s wait (`mcp/mod.rs:92`): "device '<name>' is not connected (last seen …)".
  - **A turn is never refused for it.** The label is reported (an `error` frame naming it) and the
    turn runs without that label's tools. If every label failed only because its device is
    offline, the turn runs as a plain chat turn. The Chat's "no usable tool, so refuse" rule
    (`web/agentchat.rs:497-530`) applies only when some failure is not "device offline".
  - A conversation whose only label is a desktop's therefore still answers from the phone while
    the desktop is off (R6).
- **On disconnect** the device's tools leave the aggregate, and `/mcp` subscribers get
  `tools/list_changed`. The device's own `notifications/tools/list_changed` makes lmgw list again.

### 5.4 Calls

- **Calls route through `McpManager`** like any server's and write their rows as today, with the
  caller and, for an approved call, the approver.
- **lmgw cancels every forwarded call it stops waiting on** (R14). For a timeout, a turn's cancel or
  barge-in (server-tools decision 5), or a link that closes, it sends
  `notifications/cancelled {requestId, reason}` to the device before reporting the call. A device
  must not start a cancelled call, and must stop one it can.
- **A link that drops mid-call** reports the Chat's abandoned wording ("the call was abandoned; it
  may or may not have run", server-tools decision 5), so the model does not retry blindly.

### 5.5 `_meta` on forwarded calls (L12)

Every `tools/call` lmgw sends over a device link carries:

```json
"_meta": {"lmgw/caller": {"kind": "owner" | "gateway" | "device" | "key" | "agent", "name": "…"},
          "lmgw/approval": null | {"decision": "approved", "by": {"kind": "…", "name": "…"}},
          "lmgw/timeout_ms": 60000}
```

- `caller` is the principal the call runs as: the turn's starter for a Chat turn (L4), the
  presenter for `/mcp`.
- `approval` is set when an F approval decided this call, naming the approver.
- `timeout_ms` is the row's `timeout_ms`, the point at which lmgw stops waiting and cancels
  (§5.4). A device that asks its user before running a call can show the time left.
- Other servers' calls get nothing new. The prefix form follows MCP's `_meta` key rules.
- The device decides what it trusts; lmgw only states facts.

### 5.6 Who reaches a device's tools (L16)

`ToolScope::admits` and `may_reach` answer for a device-hosted row's tools by this rule, on every
plane: `/mcp`, `/v1/responses` `mcp` blocks, realtime `mcp` tools, `/v1/mcp/servers`, Chat turns.

| Principal | Reaches the label |
|---|---|
| owner key, owner cookie | yes |
| the gateway's own runs (`ToolScope::gateway()`: the dashboard's threads, in-process agent runs) | yes, as the owner's |
| the device that hosts it | yes (its own tools) |
| a client key or device key with tool scope `allow` | only if a pattern names the label: its literal text before the first wildcard starts with `<label>__` (`desktop__*`, `desktop__see_screen`); `*` or `d*` does not |
| a client key or device key with tool scope `all` or `deny` | no |
| an agent | only if its manifest's `tools[]` names the label |
| anonymous | **no**, whatever *Require API key* says |

**In code.** `principal_list`'s `All` for anonymous and for `all`-mode keys stops at device rows: a
`List::All` carries "except device-hosted namespaces", read from the snapshot's device rows.
`/v1/mcp/servers` lists a device label only to principals that reach it.

**A browser page is kept away** because no route hands it a principal that reaches a device label:
- an anonymous request reaches none, so the permissive CORS layer, which mirrors request headers
  since 2baa764, exposes nothing to a credential-less page on `/v1/responses` or anywhere else;
- the owner's cookie is `SameSite=Strict` and honoured only same-origin (principals §3.3, §3.6), so a
  page on another site or port cannot borrow it;
- a bearer is something a page would have to be given, and device keys live in the device's secret
  store;
- `/mcp` refuses foreign origins (`mcp/ingress.rs:995-1044`), and `/mcp/host` refuses any `Origin`
  (§5.1);
- agent origins are other hosts, carry no cookie, and an agent's token reaches a device label only
  through its manifest.

**Visible:** the Devices card lists, per hosted label, every principal that reaches it (owner, the
device, each named key, device and agent).

## 6. MCP approvals (F) [Desktop 3]

### 6.1 Thread tools gain `require_approval`

- **`ThreadMcp`** (`store/chat.rs:14-22`) gains `require_approval` in OpenAI's shapes: `"never"`,
  `"always"`, or `{always: {tool_names}, never: {tool_names}}`.
  - It is parsed by `mcp::spec`'s `parse_require_approval` (server-tools §1.1).
  - `read_only` is refused (server-tools decision 8).
  - Folder defaults carry it.
- **The comment that says the Chat cannot store it** (`:7-13`) goes.

### 6.2 A gated turn (L13)

1. `agentchat` marks gated tools with `ResolvedTool::gated` (`agent.rs:64-98`).
2. The loop stops with `StopReason::Approval`, holding the gated call's siblings (`:100-116`).
3. The turn saves the assistant message with its pending calls (`ir_messages`) and a
   `pending_approvals` column (migration) that holds the calls and the starting principal's key
   id.
4. It emits `tool {event: "approval", approval_request_id, server_label, name, arguments}` for each
   gated call, then `done {pending_approvals: […]}`. `arguments` is a JSON string, as OpenAI's,
   and `name` is the wire name (server-tools decision 6).

### 6.3 Deciding

`POST /chat/api/threads/{id}/approvals` takes
`{decisions: [{approval_request_id, approve, reason?}]}`.
- **It resumes the turn as a continuation**, streaming the same frames as `send`.
- **The resumed turn runs as the stored starting principal:** its scope, policy and attribution.
  If that key is gone or disabled, the answer is a 409 naming it.
- **`/v1/responses`' rules** (`responses.rs:378-432`):
  - every gated call needs a verdict, or the answer is a 400 naming the missing ones;
  - a declined call reads "The user declined this tool call: <reason>".
- **`DecidedCall` gains `by`.** The approver goes on the call's request row, into
  `approval.decided` and into `_meta["lmgw/approval"]` (§5.5).
- **The first decision wins.** A second is a 409 `approval_decided`, naming who decided.
- **A new user message in the thread** declines every pending call with "the user moved on without
  deciding". Every call is answered, so the transcript stays valid for strict templates.

### 6.4 Bound realtime sessions

- **A bound turn's gated call** becomes OpenAI's `mcp_approval_request` item
  (`conversation.item.added` and `.done`, `{id, type, server_label, name, arguments}`), and the
  response ends (`response.done`).
- **The client answers** with `conversation.item.create` of `{type: "mcp_approval_response",
  approval_request_id, approve, reason?}`, then `response.create`. That resumes the turn through
  §6.3's code with the session's principal as approver.
  - On a bound session this one item type is accepted, though other creates stay
    `owned_by_thread` (`realtime/thread/owned.rs:120-130`).
  - The refusal at `realtime/conversation/mcp.rs:143` narrows accordingly.
- **Unlike server-tools decision 1, the approved call and its answer come in one response.** The
  resumed turn is the Chat's, which runs the call and answers. It stays a bound session's own
  behaviour, opted into by the binding.
- **A decision made on another client** while the session's request item is open: the session sends
  `lmgw.approval.decided {approval_request_id, approve, by}`. A bound session already carries
  `lmgw.*` events, and the binding is the opt-in (chat-voice §8.6). The deciding client's request
  runs the continuation; the session's next response renders it.
- **A client that answers by voice** must keep the spoken answer out of the conversation: no
  commit while the request is open. The dictation route (`…/transcribe`) is there for it. Otherwise
  the answer becomes a user message, which declines the call (§6.3).

### 6.5 Feed

- **`approval.requested`:** `{thread_id, message_id, approval_request_id, server_label, name,
  arguments}`.
- **`approval.decided`:** the same ids plus `{approve, by}`.

Both are stored records rendered from the thread's pending state.

## 7. `/mcp` passes resources through; MCP Apps metadata (G) [Desktop 2]

### 7.1 Today

- `initialize` advertises `tools` only (`mcp/ingress.rs:758-764`), and `resources/*` answers
  `-32601` (`:819-823`), on all three revisions `/mcp` speaks, though each defines resources.
- **Contradiction found:** tools' `_meta` passes verbatim (`:248-254`). An MCP Apps tool's
  `_meta.ui.resourceUri` reaches a client that then cannot read the resource.

### 7.2 Passthrough (L14)

- **Capabilities**, on the revisions `/mcp` already speaks: `resources {listChanged: true}`, and the
  extension `io.modelcontextprotocol/ui` with `mimeTypes: ["text/html;profile=mcp-app"]`. No new
  protocol revision is negotiated; a revision is its own change, with its own list of differences.
- **Methods:**
  - `resources/list`: the aggregate of the ready servers' resources, paged;
  - `resources/templates/list`;
  - `resources/read`: routed to the owning server.

  Subscriptions are §Later.
- **Namespacing.** A server with tool prefix `p` has its resource URIs rewritten by prefixing the
  authority: `ui://weather/card` becomes `ui://p__weather/card`. The same rewrite applies to tools'
  `_meta.ui.resourceUri` in `tools/list`, and to resource links and embedded resource URIs in tool
  results. A server without a prefix keeps its URIs, and the first connected wins, as for tool
  names. The rewrite of results is the one departure from server-tools decision 7.
- **Scope:** a resource is readable when the caller reaches its server (§5.6 for device rows).
- **`_meta.ui.visibility`:**
  - runs that offer tools to a model (Chat, `/v1/responses`, realtime) skip a tool whose
    `visibility` excludes `"model"`;
  - `/mcp` lists every tool with its `_meta`, so hosts can call app-only tools for their views;
  - a view's `tools/call` reaches app-only tools through `/mcp`.

  This follows the extension's own rule; it is not content-based routing.

### 7.3 Tool frames carry what an MCP Apps host needs

- **The Chat `tool` result frame** (`web/agentchat.rs:363-377`) gains:
  - `server_label`;
  - `ui_resource` (the namespaced URI, when the tool declares one);
  - `structured_content`.

  The bound session's `lmgw.chat.frame` relays the same fields.
- **These are lmgw-native frames.** The realtime `mcp_call` item stays OpenAI's shape.

### 7.4 The first step of WP8

Re-read the extension's current stable specification and pin its revision in the code's doc
comment: the capability key, the MIME type, the `_meta.ui.*` keys and the host obligations. If a
newer stable revision changed any of them, this section is amended before the build.

## 8. `ring.js` takes a transparent flag (H) [Desktop 1, optional]

- **`inputs.transparent`** (boolean, default false) joins the contract (chat-voice §10).
- **`engine.js:342`** hands the factory `{palette, transparent}`.
- **`ring.js`** then uses `getContext("2d", {alpha: transparent})` (`:28`) and `clearRect` in place of
  the background fill when transparent (`:72`).
- **The ribbon and the orb** ignore the flag, and the contract says so.
- **The dashboard** passes nothing and renders byte-identically.

## 9. Tests

- **The route walk** (`tests/it/route_walk.rs`) covers the new rows.
- **A capability matrix** for every Chat route across owner, device, client key, agent (must be
  refused, R16) and anonymous.
- **A device's turn:**
  - rows under the device key;
  - an alias outside its scope refused;
  - an expired key refused at the route and at the call;
  - rpm counted per call;
  - `lmgw`, out-of-scope labels and a bare unconnected server refused in writes;
  - an admin thread 404 through thread, message and attachment ids, search, folder counts, feed,
    export and bind;
  - a device's folder delete leaving admin threads, with the count.
- **Binding from a device:** `Chat` accepted; the policy check before the 101; the refusal rows; the
  takeover naming the binder.
- **The feed:**
  - commit order, `since` and `Last-Event-ID`;
  - `resync` for an old cursor, a foreign epoch and a cursor newer than the newest;
  - retention and paged catch-up;
  - rendering at delivery (a deleted thread renders a tombstone);
  - a lagged live subscriber gets `state`;
  - `hold` on a fallback change made through the settings patch;
  - the sweep's archive events;
  - `revoked` on disable, rotate, delete and expiry;
  - per-principal filtering;
  - a failed write records nothing.
- **`current`:**
  - two concurrent callers get one thread;
  - an empty thread is reused;
  - idle measured from the newest message;
  - `gone`, `folder_no_model`;
  - the sweep skips current threads;
  - a hand-created chat thread becomes current and an admin one does not;
  - deleting the current thread records `folder.current`;
  - defaults reach the current thread, and `apply_to_current: false` stops it.
- **Reach (L16):** anonymous with auth off, an `all` key, a `deny` key, `allow` with `*` and with
  `desktop__*`, another device, the hosting device, an agent with and without the label, the owner,
  on `/mcp`, `/v1/responses` (including a cross-origin anonymous browser-shaped request),
  realtime, discovery and Chat turns.
- **The host link,** with a fake device over WebSocket:
  - `initialize` and listing;
  - a call with `_meta`;
  - `notifications/cancelled` on a timeout, a turn cancel and a link close;
  - a drop mid-call reported as abandoned;
  - a takeover;
  - an offline label failing at once while the turn still answers;
  - size overruns closing with the setting named;
  - an `Origin` refused;
  - revocation closing the link with 4003;
  - `allow_sampling` refused.
- **Approvals:**
  - the Chat round trip and the sibling rule;
  - the resumed turn as its starter;
  - `by` on the row, the event and `_meta`;
  - first decision wins;
  - a new message declines;
  - the bound session's items, and `lmgw.approval.decided` for a decision made elsewhere.
- **Resources:** list and read routing, rewrite consistency between `tools/list`, results and
  `resources/read`, scope, the extension capability, `visibility` filtering in model runs.
- **H:** the visualisation harness's corner pixels transparent with the flag, unchanged without.

Live checks run on a dev instance (`scripts/dev-instance.sh`), never the real data directory.

## 10. Work packages (build order)

| WP | Scope | Needed by |
|---|---|---|
| 1 | H: the flag in `engine.js` and `ring.js`, the contract text, the harness check | Desktop 1 (optional) |
| 2 | A core: `Device` kind, `Cap::Chat` with every `holds` arm positive, `describe`, the table rows, expiry on `Chat`, the migration (`last_seen_at`, `hosts_label`), `device_disabled`, the key ops (rotate as a new path), the pairing link, the Devices card with confirmations, the revocation signal (expiry timer, `revoked`, 4003), the documents of §1.9 | Desktop 1 |
| 3 | A in the Chat: the caller through handlers and `TurnOpts`, `policy_checked_call` per model call, the device's tool scope and attribution, L3 in `ChatRepo` and the collections, L5's write check, binding by `Chat` with the policy check and refusal rows, the takeover naming | Desktop 1 |
| 4 | B for Desktop 1: the table, epoch, `store::feed::record` for thread and folder writes and the sweep, the live broadcast (`turn.*`, `voice.*`, `hold`, `state`), `hello`, the SSE route with cursor, `Last-Event-ID`, paged catch-up and `resync`, the retention and keep-alive settings | Desktop 1 |
| 5 | C: the columns, the `current` route with its lock and `folder_no_model`, the sweep's skip, the hand-created current thread, the delete's `folder.current`, defaults applied to the current thread, the dashboard's folder form and sidebar | Desktop 1 |
| 6 | D for Desktop 1: the `lmgw-client` crate with the voice panel moved onto it; the DTOs and `DocRoute`s of the routes the desktop client uses | Desktop 1 |
| 7 | E: `/mcp/host`, the rmcp WebSocket transport and the manager's `Device` arm, the device rows from the grant, status and takeover, offline resolution that never refuses a turn, size and ping settings, `notifications/cancelled`, `_meta` stamping, L16 in `ToolScope` with the Devices card's reach list | Desktop 2 |
| 8 | G: §7.4's re-read, resources on `/mcp` with namespacing and the extension capability, `visibility` in model runs, the `tool` frame fields | Desktop 2 |
| 9 | F: `require_approval` on thread tools, the gated Chat turn and its route, `by`, the bound session's items and `lmgw.approval.decided`, the feed events | Desktop 3 |
| 10 | B and D for Android: `message.*` records in every message write, the remaining DTOs and `DocRoute`s, the QR code | Android |

## 11. Open questions for the owner, in priority order

1. **A new device's default scope and budget.** The draft prefills `all`, no budget, shown on the
   pairing form and flagged amber on the card; a stolen device key then spends on every alias.
   *Recommendation:* keep the prefill, but require the form to be confirmed with the scope visible.
   Set budgets per device where a cloud alias is reachable.
2. **Past threads of an ongoing folder** archive at 14 days and are deleted 30 days later, like any
   thread. *Recommendation:* a per-folder retention override (archive and purge days, empty =
   global), so an assistant's long history can be kept without pinning every thread.
3. **Defaults applied to the current thread by default** (L9). *Recommendation:* yes, with the
   checkbox.
4. **Feed retention 7 days, persisted** (L6). *Recommendation:* yes; an in-memory feed would force
   every client to `resync` after each lmgw start.
5. **Device keys hash-only** (L1). *Recommendation:* yes; Rotate re-pairs.
6. **Where the Devices card sits.** *Recommendation:* Usage → Keys, where every other credential is
   listed.

### Answers (the owner, 2026-10-06)

All six are approved as recommended: (1) the prefill stays, confirmed with the scope visible,
budgets per device where a cloud alias is reachable; (2) a per-folder retention override; (3) yes,
with the checkbox; (4) 7 days, persisted; (5) hash-only, Rotate re-pairs; (6) Usage → Keys.

## Later

- Folder-scoped device keys (a device that sees only some folders).
- Push to a backgrounded phone: lmgw posts feed events to the device's UnifiedPush endpoint.
- TLS on lmgw's listener, which brings the pairing link's `fp`.
- WebRTC realtime (`/v1/realtime/calls`) for mobile jitter.
- Live mirroring of another client's running turn in the feed.
- Sampling from device-hosted servers under the device key; elicitation; resource subscriptions.
- A URL scheme lmgw's shell handles to open its own window at a thread.
- Approvals on unbound realtime sessions (server-tools §6).

## Appendix: Review dispositions (2026-10-06, against 2baa764)

| # | Finding | Disposition |
|---|---|---|
| R1 | Anonymous and default-scope callers reach device-hosted tools | **Changed:** L16, §5.6. Explicit naming or owner or the hosting device only; anonymous never; browser keep-away stated; reach list on the card. The reviewer's "refuse a grant while auth is off" is **rejected**: anonymous never reaches a device label in either state, so that refusal would block the default setup and protect nothing more. |
| R2 | An approval does not say who approved | **Changed:** `_meta` approval names `by` (§5.5), `DecidedCall` gains `by`, the resumed turn runs as its starter (§6.3, L13). The device's trust list is the device's (L12). |
| R3 | "KeyPolicy applies whole" not built | **Changed:** `policy_checked_call` per model call (§1.3), expiry on `Chat` routes, revocation at `expires_at` (L17, §1.6). |
| R4 | L3 hides Admin Chat only partly | **Changed:** one rule in `ChatRepo`, 404 by any id including the bind, collections and exports filtered, folder delete keeps admin threads (L3, §1.3). |
| R5 | `current` has no model | **Changed:** 409 `folder_no_model`; ongoing requires a model; clients do not pass one (L8, §3.1–§3.2). |
| R6 | An offline device makes threads refuse every turn | **Changed:** offline labels are reported and skipped; refuse only when some failure is not "device offline" (§5.3). |
| R7 | Approving by voice declines the call | **Changed** on the API side: §6.4 says a voice answer must stay out of the conversation and points to the dictation route. The client's flow is its own spec. |
| R8 | Multi-device races | **Accepted** as client behaviour. The API gives `turn.started` with `by` and `voice.bound` with `by`; §1.3 states the expectation. |
| R11 | Hidden limits | **Changed:** host link size and ping settings (§5.1), feed keep-alive setting named in `hello` (§2.1). The AEC wait is the client's. |
| R13 | G pins an MCP revision without a source | **Changed:** the extension document is cited with its identifiers; no new revision; the departure from server-tools decision 7 is stated; §7.4 re-reads before the build. |
| R14 | Confirmation outlives the call's timeout | **Changed:** `notifications/cancelled` for every call lmgw stops waiting on (§5.4); devices must not run cancelled calls. |
| R15 | Feed rows store rendered JSON | **Changed:** change records, rendering at delivery, live-only turn/voice/hold, `hello` live state, sweep events (L6, §2.2–§2.3). |
| R16 | Negative matches hand out rights | **Changed:** every `holds` arm positive, agent × Chat is "no", `describe` gets a device arm (§1.2); matrix test (§9). |
| R17 | `hold` misses fallback changes | **Changed:** emitted on any change of the published `hold` (§2.2). |
| R18 | Revocation has no signal | **Changed:** `revoked` event and 4003 (L18, §1.6); the 401 for a rotated key is listed (§1.8). |
| R19 | Folder and current edges | **Changed:** only chat threads become current; a deleted current thread records `folder.current` (§3.3). The name lookup is the client's (folder names stay non-unique). |
| R20 | L5 passes bare servers and built-ins | **Changed:** `admits` on every exposed tool now; an unconnected bare server is refused; what the scope does not bound is stated (L5). |
| R21 | Dropped link reported as failed | **Changed:** abandoned wording (§5.4). |
| R25 | Phase 1 carries Android-sized work | **Changed:** `message.*`, the full DTO set and docs, and the QR code moved to Android (WP10); Desktop 1 keeps the table, so delivery is built once. |
| R26 | Sampling bypasses the device's policy | **Changed:** `allow_sampling` refused on device rows (§5.2); sampling under the device key is §Later. |
| R27 | The client hardcodes its label | **Changed:** `hello.hosts_label` (§2.1). |
| R28 | Public-docs hygiene | **Changed:** this record names no client spec and no person; the client is "a desktop client". |
| R30 | Resync detection and catch-up memory | **Changed:** epoch cursor and paged catch-up (§2.1, §2.3–§2.4). |
| R31 | Claim accuracy | **Changed:** `TurnOpts` fields, the table's range, chat-voice §8.6 for the events, `key_rotate` as a new path (L1), the manager's `Device` arm, the transport-binding statement (§5.1). |
| R32 | F's wording | **Changed:** v1's Q6 resolved through the binding's opt-in (§6.4); unbound approvals only in Later; one-response behaviour stated; `arguments` and `name` stated (§6.2). |
| R33 | Owner-only exports protect nothing | **Changed:** exports join `Chat`, filtered by L3 (L2). |
| R34 | Docs A must update | **Changed:** listed in §1.9. |
| R35 | The pairing key on argv | **Changed:** client guidance in §1.4; the client spec carries the handling. |
| R36 | Destructive actions on the card | **Changed:** confirm, then do (§1.4). |
| R9, R10, R12, R22, R23, R24, R29 | Client-only findings | Not applicable to this record; dispositions in the client's spec. |
