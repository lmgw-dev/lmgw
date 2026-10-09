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

*Changed during the build (review W3-1), 2026-10-07, decided by the owner:* **a thread with the
self-admin toolset (`lmgw`) among its tools is Admin Chat for L3**, and so is a folder whose
defaults attach it (every new thread in it would). The owner's turns in such a thread run the
self-admin tools; a device that could rewrite its prompt or history would steer them. For a device
they do not exist: every route by id answers 404, the collections leave them out, the bind refuses
them, and a folder route on such a folder is a 404. L5 already refuses a device writing `lmgw`.
**Attaching the toolset takes the thread out of a device's reach at once:** a session a device
bound to it closes with 4003 (`revoked: chat thread N is out of reach for this key`; 4004 with
the neutral reason since §1.6's close-code note, 2026-10-07), its
live events stop reaching devices, and a device's feed receives the thread's removal
(`thread.deleted`), or the folder's (`folder.deleted`). Taking the toolset off brings it back as
`thread.created` / `folder.created`. The rule is one predicate: `ChatThread::drives_self_admin`
in Rust, `self_admin_thread!` in the store's SQL, and the same pair for folders.

*Decided, 2026-10-07 (review W4-18):* the close is neutral, like every other L3 refusal. It
said that the self-admin toolset was attached, and so did the `error` event before it; both now
say only that the thread is out of reach for this key (`chat_thread_not_found`, then the 4003 —
4004 since §1.6's close-code note).

*Changed during the build (review W6-1), 2026-10-07, decided by the owner:* **a device's folder
delete leaves the folder to the threads it cannot see.** Moving the Admin Chat and self-admin
threads out of the folder (the bullet above) put them on the global retention, so a device
undid the owner's longer folder retention for them: W5-1's lever the other way round. Now a
device's delete takes only its own threads as it asked (`keep` moves them out, `delete`
deletes them); when the folder holds others it stays in place holding them, its retention
unchanged, and from then on is out of every device's reach, as a folder whose defaults attach
the toolset is (`chat_folders.devices_hidden`, migration 0066, in `self_admin_folder!` and
`ChatFolder::hidden_from_devices`). The device's answer is `{ok: true}` like any delete's (no
`left`, no message), and its feed reads a delete in a delete's order: its threads, then
`folder.deleted`; a current thread its threads took with them ends without a word to devices.
No answer a device gets says that other threads were there. The owner sees the folder with
them, and reads `folder.updated`. *Built as a mark, not as "a folder holding only hidden
threads is hidden" in general:* that rule would hide a device's empty folder the moment the
owner made an Admin Chat thread in it, and turn an attach to a folder's only thread into the
folder's removal — both telling a device what L3 hides.

*Changed during the build (the final review's F-7), 2026-10-07, decided by the owner:* **the
owner sees the mark and can take it back.** The owner's folder list carries `devices_hidden`;
the dashboard shows such a folder as "hidden from devices (a device deleted it)" in the list and
in its settings, which offer "Show to devices again", confirmed with what devices will then see.
The folder patch takes `devices_hidden: false` from the owner only (a device is refused as for
retention, `403 forbidden`; it cannot reach a hidden folder anyway; `true` is no request, a
device's delete is what sets it). The clear is recorded as the folder's level change and then
each thread in it (`store::show_chat_folder_to_devices`), so a device's feed receives the folder
as `folder.created` and its threads in it as `thread.updated`; the threads it may not see stay
out of its reach by their own level.

*Changed during the build, 2026-10-07, decided by the owner:* **a per-device switch, "may use
lmgw's admin tools"** (`api_keys.self_admin`, migration 0067), so the owner can administer lmgw by
voice through a desktop client. Off by default; set on the pairing form and on the Devices card
(turning it on is confirmed with "this device can then read and change lmgw's configuration
through the admin tools, within the self-admin level set in Settings"), owner-only through
`key_create` / `key_set` (refused for any other key kind). L3 is a level now
(`self_admin_thread!`, `self_admin_folder!`, `ChatThread::reach_level`,
`ChatFolder::reach_level`): `2` an Admin Chat thread or a folder a device deleted, `1` the
self-admin toolset attached (to a thread, or to a folder's defaults), `0` anything else. A reader
reaches `3` (the owner), `2` (a device with the switch) or `1` (any other device) and sees what is
below its reach (`store::AdminThreads`): with the switch a device sees and uses the threads and
folders that carry the toolset, in text chat and in bound realtime sessions alike; Admin Chat and
a folder a device deleted stay hidden from every device. Every place L3 is enforced reads the
level: by-id resolution, lists and counts, search, exports, the bind, `current` and its
rollovers (a current thread that took the toolset rolls over for a device without the switch
only), the folder delete's mark (an allowed device's delete takes the toolset's threads as it
asked and leaves the folder only to Admin Chat), and the feed's records and live events. The feed
records a level in `chat_feed.admin` (0067 rewrote every earlier non-zero value to `2`: no device
could have the switch then, and one given it later hears the toolset's threads as created). The
switch is read from the snapshot per request; a feed reads it from the key's row when it opens.
**Turning it off acts as attaching the toolset does:** the device's realtime sessions bound to
such a thread close with the neutral 4003 (4004 since §1.6's close-code note;
`LiveTurns::reach_changed`), and its feed hears the
threads go (`thread.deleted`), its plain threads in such a folder move to none
(`thread.updated`), and the folders go (`folder.deleted`) — a delete's order — then a fresh
`state`. Turning it on brings them as `folder.created`, `thread.created`, `thread.updated`. The
key write records a `device.reach` record in its own transaction; only that device's stream
renders it, in commit order, so a device that was away hears it when it catches up (its stream
starts at the reach before the first such record after its cursor). `hello.self_admin` says the
switch as it is. Tool calls and request rows are the device's as for every turn of it
(`admin-tool` rows included).

*Corrected (the pre-merge review's P-1, P-4, P-5 and P-9), 2026-10-07, decided by the owner:*
**a catch-up does not replay a switch.** Starting at the reach before the first switch record
after the cursor, as said above, rendered the toolset's threads and folders as they are now to a
device whose switch was off now, and an "on" met in the catch-up sent them in full before an
"off" took them back. Now the switch is read in the transaction that reads the feed's head
(`store::feed::bounds_and_switch`), so no switch falls between the two reads (P-5), and `hello`
and the live state are at that reach. The catch-up — the records up to that head — renders each
record with the narrower of the reach the device had when it was written and the reach it has
now, and a `device.reach` record of its own met there is one `resync` carrying its cursor, not a
replayed transition: the client reloads what it shows, at the reach it has now. A device never
receives what it may not see now, nor what was written while it could not see it. A record read
after the head plays the transition as above. Live frames waiting for the table are checked again
as they go out: an event about a thread the reach no longer covers, and a `state` computed for an
earlier reach, are dropped (P-4). `state` carries `self_admin` as `hello` does, so a connected
client learns of a change (P-9).

*Changed (the pre-merge review's P-8), 2026-10-07, decided by the owner:* **turning it off,
Disable and expiry stop in-flight work.** A device's text turn on a thread that leaves its reach
— its switch off, or the toolset attached — is cancelled as a delete cancels it
(`LiveTurns::reach_changed`, `self_admin_changed`): its stream says `superseded` and nothing of it
is saved. `devices::self_admin` reads `off` for a disabled or expired key. A device's `lmgw__*`
call is checked against its key row as well as the snapshot (`store::device_admin_now`,
`ScopedExecutor`), so a key write that committed is in force before the snapshot is reloaded. A
realtime session of a device whose switch moved lists its `lmgw` label again
(`devices::ReachMoves`): its model is offered what the device may use now. An `lmgw__*` call
already running finishes; the next one is refused. `/mcp` serves no admin tool, so nothing there
moves.

*Changed (the pre-merge review's P-7 and P-18), 2026-10-07:* **the hidden-folder rule takes in
the threads that stay.** A device's delete of a folder that keeps threads out of its reach
records each thread that stays before the folder's removal, so a device that sees some of them —
one allowed lmgw's admin tools, beside a deleter that is not — reads them in no folder, as the
by-id reads already said. Showing the folder to devices again is written in the folder patch's
own transaction (`ChatFolderPatch::show_to_devices`), never a second write after it.

*Changed (the pre-merge review's P-3), 2026-10-07, decided by the owner:* **the switch is a level
per device: off, read only or full** (`api_keys.self_admin` 0, 1, 2, `config::DeviceAdmin`,
migration 0068; a row that had the switch on reads as read only, the safer level, and nothing had
shipped). What a device's admin tools may do is its level capped by the gateway's self-admin
level (`DeviceAdmin::capped`): a device at full under a gateway at read only reads only. Its
reach into the toolset's threads and folders is the same at read only and at full; off takes it
away. The administration by voice the switch was built for — a desktop client — asks at read
only and changes things at full.
- **Programs on this machine need full.** Every tool that has lmgw run a program as the lmgw user
  is a write tool, so only a device whose capped level is full reaches one:
  `lmgw__mcp_server_set` (a stdio server's command, arguments, environment and directory; a
  container server's image and extra run arguments); `lmgw__agent_set`, `lmgw__agent_install`
  and `lmgw__agent_run` (an agent's container and its manifest's mounts); `lmgw__build_set`,
  `lmgw__build_run` and `lmgw__build_check_merge` (a git source's build code, a pull request's
  included); `lmgw__local_model_set`, `lmgw__aux_model_set`, `lmgw__image_model_set` and
  `lmgw__audio_model_set` (a row's image and flags decide what its container runs), with
  `lmgw__container`, `lmgw__local_model_test` and `lmgw__bench_start`, which start such
  containers, and `lmgw__container_image_pull`, which fetches their images. Below full they are
  neither offered nor run: `selfadmin::call_capped` refuses them, naming the device's level when
  it is the lower one, and the per-call row check refuses a level lowered before its snapshot.
- **The pairing form and the Devices card** choose the level: off by default, read only the first
  choice above it. Read only is confirmed with "this device can then read lmgw's configuration
  and state", full with "this device can then change lmgw's configuration and register programs
  that run on this machine as the lmgw user". The card shows the level beside the name.
- **On the wire** the level is said by name: `hello.self_admin`, `state.self_admin`,
  `KeyRow.self_admin`, and `key_create`/`key_set`'s `self_admin` (`off`, `read_only`, `full`;
  any other kind of key is refused anything but `off`). A bound session's `admin_tools` is true
  only for a binder at full.
- **Everything around a switch applies to a level change.** It is a `device.reach` record
  (`self_admin`, `self_admin_was`) in the key write's transaction. A move above off or back
  plays as the switch did — live, the threads and folders come or go; in a catch-up, a `resync`
  — and every move sends a fresh `state` with the level. A lowered level refuses a running turn's
  next write call, and the device's realtime sessions list `lmgw` again at the new level.

*Changed after the merge, 2026-10-07:* **the gateway's level moving plays as a device's own
level moving**, for each device whose capped level moved with it. Before, a change of the
self-admin level in Settings narrowed a device's calls (the mode gate reads the snapshot) but
left its realtime sessions offering what they listed and its feed saying the old level.
- **What the wire says, and what a device sees, is the capped level** (*decided by the owner
  the same day*). `hello.self_admin` and `state.self_admin` say what the device's
  admin tools may do now — its own level capped by the gateway's — and L3's reach follows the
  same value (`devices::may_do`, `devices::reach`, `store::feed::Levels`): at a capped `off` a
  device sees none of the toolset's threads and folders and may not attach `lmgw`
  (`scope::self_admin_of`), whatever its own level, so L3, `hello` and `state` agree.
  `KeyRow.self_admin` and the key ops stay the device's own level, the one the owner sets.
- **The move is a feed record, wherever it was saved.** `store::save_settings` records a
  `gateway.reach` record (`self_admin`, `self_admin_was`) in the save's own transaction whenever
  the stored level moves — a first save too, measured from the default `read_only` in force
  before it (review G-5). The gateway's
  level is read with the feed's head (`feed::bounds_and_switch`), and every device's stream reads
  the record in commit order as a move of its own capped level: to or from `off` it plays as the
  per-device switch — live, the threads and folders come or go in a delete's or a create's order;
  in a catch-up, one `resync` (the catch-up starts at the levels before the first record of each
  kind after the cursor, `first_gateway_reach_after`) — and a fresh `state` follows where the
  capped level moved at all. A device whose capped level did not move hears nothing, and the
  owner's stream renders nothing of it. The published snapshot whose level moved
  (`AppState::store_carrying_lease`) wakes the feed, closes a device's bound sessions on a thread
  it no longer reaches with the neutral 4004 (§1.6's close-code note) and cancels its turns there
  (`LiveTurns::reach_changed`), and raises `devices::ReachMoves`, so its realtime sessions list
  `lmgw` again.
- **The same in-flight rules.** A bound session's turns resolve the toolset per turn, so the
  next one is offered the narrower set. A device's `lmgw__*` call is checked against the stored
  self-admin level as well as its key row (`store::gateway_self_admin_now`, in
  `ScopedExecutor`): a lowered level refuses a running turn's next write call from its commit,
  before the snapshot is reloaded or when the reload failed. A call already running finishes.
  *Added by the branch review's G-6:* a device's attach of `lmgw` and its change of a toolset
  thread (L5's G-2 note) read the stored levels too; a device's text turn whose thread left its
  reach between the read and its registration is refused at registration (`chat_turn`); and a
  settings save whose reload fails twice lays the saved settings over the published snapshot
  (`AppState::settings_saved`), so a moved level still closes, cancels and re-lists, and the
  answer says the save beside the reload's error instead of failing a committed save.
  *Changed by the branch review's verification, 2026-10-07:* `selfadmin::call_capped` itself
  reads a device caller's own level and the gateway's from the store, so the rule holds for
  every call a device makes, an agent run it started included, which no `ScopedExecutor`
  wraps (V-7). The turn's reach is asked before it takes the thread as well as after, so a
  device whose level just dropped never cancels the turn running there (V-10). The fallback
  lays the settings over the snapshot it replaces, inside the swap, so a publish that lands
  meanwhile is kept (V-9).
- **`admin_tools` follows the binder's level** (review G-14): a bound session's
  `session.lmgw.resolved.chat_thread.admin_tools` is re-read before each response, and `true`
  only while the gateway's level is `full` and the binder's own is too (`bound::thread_ref`, one
  rule at the bind and at every re-read); a move shows in `lmgw.chat.thread` at the next
  response.
- **A catch-up across both kinds of record** plays each move of the reach the device had as one
  `resync`, as two switches of its own do (P-1): away across its own `off → read_only` and then
  the gateway's `read_only → off` is two, though the reach it ends at is the one it left with.

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
  that reason. `lmgw` is never allowed. (*Changed 2026-10-07:* except to a device the owner
  allowed lmgw's admin tools, below.)
- **What this does not bound, stated plainly:** a device may write a thread's system prompt and
  any label within its own reach. The owner's later turns in that thread read them and run with
  the gateway's scope. A device's scope bounds the device's own turns, not the content the owner
  later reads.

*Changed during the build, 2026-10-07, decided by the owner:* **a device allowed lmgw's admin
tools** (`self_admin`, L3's note) may attach `lmgw`: its tool scope takes in the label
(`ToolScope::of_request`), so its turns, its bound sessions' turns, an unbound realtime session
and `/v1/responses` resolve the toolset for it, and `/v1/mcp/servers` lists it. The self-admin
level in Settings still bounds what the tools may do (read only or full); `/mcp` still serves
none of them. Any other device is refused as before, the refusal saying it is not allowed lmgw's
admin tools. *Narrowed by the branch review's verification (V-4), decided 2026-10-07:* attaching
the label to a thread or a folder's defaults needs the device's admin tools at `full` (its own
level capped by the gateway's, as stored); below that it uses the toolset threads and folders
the owner made (G-2's note below).

*Changed (the pre-merge review's P-6), 2026-10-07, decided by the owner:* **the device's own
patterns narrow the label.** Taking in the whole label beside the device's list (9dce3d7) let a
switched device's `deny: lmgw__settings_set` or `allow: docs__*, lmgw__status` silently stop
applying. The switch grants the label, and the device's tool scope narrows it where it names the
namespace — an allow pattern that begins with `lmgw__`, or a deny pattern that can match such a
name (`ToolScope::admits`); a list that says nothing about it leaves the label whole. An attach
whose patterns leave none of the tools (judged against the whole catalog, since the self-admin
level may be raised later) is refused with `tool_label_out_of_scope`, so a thread never loses
every tool at turn time without a word. No other label is opened by this: `lmgw__` is the
reserved namespace no registered server, bare tool or device-hosted label may hold.

*Changed during the build, 2026-10-06:*
- **Only what the device writes is checked.** An entry the thread or folder already carries,
  unchanged, passes: a client that sends the whole list back is not refused for what the thread
  already carries. A scope with no list of its own (`all`) admits every tool but `lmgw`'s, so nothing
  else is listed or refused for it. An unknown label is refused without naming any other.
- **Knowledge bases** (review W2-4, decided by the owner). A device reaches the owner's bases as
  the `kb` toolset its tool scope bounds, never around it. A base it names needs `kb__search`
  within its scope: a thread's `kb_ids`, a folder's default `kb_ids`, a message's `kb_refs`
  (send, edit). Otherwise `403 tool_label_out_of_scope`, naming the base. *Corrected 2026-10-07
  (review W3-5):* the refusal names the tool and never the base, and it comes before the check
  that the bases exist, so a device out of the knowledge tools cannot list the bases by probing
  ids: a known id and an unknown one answer alike. At turn time, a
  device's auto-mode retrieval without `kb__search` is skipped, with the reason as a note in the
  `retrieval` event, and nothing is stored; the owner's own later turn still searches. Tool mode's
  `kb` label resolves under the device's scope like any label.

*Changed during the build (review W3-4), 2026-10-07:* **aliases are bounded like labels.** The
model and the voice's speech-to-text and text-to-speech aliases a device writes (a thread's
settings, a new thread's model, a folder's defaults) must be within its key's alias scope, or the
write is the key's own `403 key_scope` and nothing is written. The owner's later turns, dictation
and read-aloud go to those aliases, and a device must not choose for them an alias it may not use
itself. An alias the thread or folder already carries passes; a thread created in a folder whose
defaults name a model takes that one. The check is scope only, not counted: a write is no call.

*Changed during the build (review W5-1), 2026-10-07, decided by the owner:* **a folder's own
retention is the owner's.** The sweep applies a folder's `archive_days` and `purge_days` to every
thread in it, the Admin Chat and self-admin threads a device cannot see included, so a device
that set them could have those threads deleted: L3's folder delete, delayed. A device that
writes either (on create, or a patch that changes it) gets `403 forbidden` naming the fields;
the value already stored is no write. `ongoing` stays a device's to write. The global retention
settings say that a folder's own retention overrides them.

*Changed after the merge, 2026-10-07; widened by the branch review's G-1, decided by the owner:*
**the settings that decide who reaches lmgw are no tool's to change.** A device at `full` could
call `lmgw__settings_set` with `auth_enabled: false` and drop every key check, its own included;
the first fix refused it to devices only, and the review showed an agent's run, or the owner's
own steered turn, making the same call. Now no `lmgw__*` call changes them, whoever makes it — a
device's turn, an agent run, Admin Chat, a model on `/mcp/admin`. A call that names one is
refused with `<setting> cannot be changed through lmgw's admin tools … Change it on Settings in
the dashboard`, as a tool error, and nothing of the call is written, an ordinary setting beside
it included (`selfadmin::ACCESS_SETTINGS`, `guards::access_refusal`, in `call_capped` after the
level gate). They change on Settings, with the owner's credential (`settings_set_full`, and
`/api/op/settings_set` for `auth_enabled`, which the tool's schema no longer lists):
- `auth_enabled` — whether `/v1` and `/mcp` ask for a key at all;
- `self_admin` — the gateway's self-admin level, the cap on every device's own;
- `bind_addr` — loopback or the network;
- `agent_origin_suffix` — the names a service agent's UI answers under, lmgw's one origin
  setting.

Read off Settings → Network & access ("where the gateway listens and who may call it"), whose
other rows are `max_body_mb` (a size, not access) and the API keys (no setting; no tool writes
them). CORS is fixed (permissive), so there is no CORS setting to list. The tool's description
names the four.

*The branch review's G-1, decided by the owner, 2026-10-07:* **a device's agents are its own.**
An agent run a device starts (`lmgw__agent_run`) carries the device as its caller
(`batch::Input::started_by`, `batch::run_caller`): its calls of the admin tools are capped at what
the device's admin tools may do as each is made, read as stored (V-7), and are the device's rows.
Only those calls are: the run's tools are the agent's own, its manifest's labels, resolved and
called as the gateway's, not narrowed by the device's tool scope (*corrected by the branch
review's verification V-6:* this note said a device could not do through an agent what it may
not do itself, which overclaimed; a device writes and starts agents only at `full`, where P-3
already lets it widen its own scope). A device's `lmgw__agent_set` and `lmgw__agent_install`
record the agent as the device's (`agents.created_by_key`, migration 0069), and an existing agent
it did not create is refused, `replace` or not (`import_inner_as`), so a device cannot turn the
owner's agent into one that runs what it chose. *Added by the verification (V-5):* its
`lmgw__agent_delete` deletes only an agent it created, checked in the delete itself, so a delete
and a re-create under the same id cannot replace the owner's agent either; and the owner's write
to an agent — a manifest replace from the dashboard or by the owner's own tool call, or the
dashboard's config, enable, dev URL, pull or reset — adopts it (`created_by_key` back to `NULL`,
`store::adopt_agent`), so the device may no longer replace or delete what the owner has since
changed. Dashboard runs and the runs an agent opens itself (`started_by: None`) stay the
gateway's.

*The branch review's G-2, decided by the owner, 2026-10-07:* **a device below `full` reads a
toolset thread and does not steer it.** The owner's turns in a thread with the self-admin
toolset run the write tools at the gateway's level, so whoever writes the thread's prompt or
history steers them. At what its admin tools may do being `read_only`, a device reads such a
thread and runs turns in it (at its own level), but its write of the thread's settings (prompt,
tools and the rest), a message edit, delete or regenerate there, and a change of a toolset
folder's defaults are refused (`web::chat_steer`, reading the stored levels). At `full` it
may. *Changed by the branch review's verification, 2026-10-07:* the refusal is `403
chat_toolset_needs_full` (§1.8), its own code beside the other 403s, and says what the device
may still do and where its level is set (V-12). A thread's settings or a folder's defaults sent
back unchanged are no change and are not refused (V-12). The check sits in the settings write
both routes share (`chat::apply_settings_patch`), so a change of an ongoing folder's defaults
that would reach its current thread (L9) is refused when that thread carries the toolset, even
in a folder that does not; `apply_to_current: false` changes the defaults alone (V-3). And a
device below `full` does not attach the toolset at all (V-4, the attach note above), so it can
no longer make one of the owner's plain threads, or its own, a toolset thread with a prompt it
wrote. *Still open, and accepted:* a `read_only` device's own user messages in a toolset thread
stay in its history, and an owner's later turn there reads them as earlier user turns. Devices
are the owner's own paired devices, and nothing a device wrote steers the thread's settings. Two more cases remain, and each needs an action by the owner: a `read_only` device can still write the prompt of a plain thread, so if the owner later attaches `lmgw` to that thread, the owner's turns run with that prompt; and a prompt a device wrote while at `full` stays on the thread after the owner lowers the device's level or revokes it.

*The branch review's G-3, decided by the owner, 2026-10-07:* **a stored credential follows its
host.** A self-admin tool's call that moves an upstream's `base_url`, or an MCP server's `url`,
to another address while the row holds a credential must restate it in the same call — an
upstream's `api_key` (non-empty), an MCP server's `headers` as text (`""` sends none) — or it is
refused and nothing changes: one call must not send the owner's provider key to a host it names.
An upstream with extra headers, which no tool sets, moves on the dashboard only. The dashboard's
own ops keep their behaviour. *Changed by the branch review's verification, 2026-10-07:* the
check sits where the op applies the move, on the row it is about to write
(`ops::credential_move`, `RowWriter::Tool`), so `enable` and `disable`, which apply `base_url`
and `url` as `update` does, meet it, and so would any action added to that arm (V-1); a
`headers: null` keeps the stored headers, so it restates nothing (V-2).

*The branch review's G-4, 2026-10-07:* **what the guards do not bound at `full`.** The refusals
above stop a direct change. A device at `full` still reaches the program tools (P-3): an MCP
server's command, an agent's container, a build, a model row's flags each run as the lmgw user
and can rewrite `lmgw.db`, these settings included, and the next reload applies it. What a device
created outlives its revocation: its agents (with their own tokens), its MCP servers, its model
rows, and a service agent's unauthenticated UI on lmgw's listener; Disable, Rotate and Delete of
the device remove none of them. Only agents record their creator; the Devices card does not list
what a device created — that needs a creator on every kind of row and is not built.

**L6. The feed persists change records, not renderings.**
- A record is `{seq, at, type, thread_id, folder_id, message_ids, by}`, written by one store
  helper inside the change's own transaction.
- Delivery renders the current state at read time, or a tombstone for something gone.
- Turn, voice and hold events are live-only. `hello` carries the live state.
- The cursor is `"<epoch>:<seq>"` with an epoch per database (*as built:* `"<epoch>:<seq>:<tag>"`,
  the tag a random draw stored with the record; §2.3, reviews W5-4 and W6-3). Catch-up streams in pages. A cursor
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

*Changed during the build, 2026-10-06:*
- **The migration is 0061**, a rebuild of `api_keys` like 0037's (a CHECK cannot be widened in
  place). It adds `CHECK (hosts_label IS NULL OR kind = 'device')` and a unique index on
  `hosts_label`, the backstop between two devices.
- **`last_seen_at`** is stored as RFC 3339 UTC. It stays out of the snapshot, so a connection
  opening or closing reloads nothing; the Keys list reads it from the table.
- **The hosting label** is validated as §1.5 says: letters, digits, `_` and `-`, no `__`, not a
  reserved namespace, and not a server's prefix or name. The other direction is checked as well:
  `mcp_server_set` refuses a prefix or a name that is a device's label. The server row that a
  grant creates (§5.2) is WP7's. Until then the grant is stored and shown, and serves nothing.
- **The amber flag** marks a device whose alias scope is `all` and that has no budget. The tool
  scope does not count: it bounds no spend.
- *Corrected 2026-10-06 (review W2-8, W2-25):* "the other direction is checked as well" was not
  true of a service agent, whose MCP row takes its id as the tool prefix: an agent whose id is a
  device's hosting label is now refused when its tools would register. Migration 0062 makes the
  unique index on `hosts_label` case-insensitive, as every check that writes one already was.

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

*Changed during the build, 2026-10-06:*
- **The four new rows arrive with their routes** (WP4, WP5, WP7, WP9). The route walk fails on a
  row that no route answers. *`GET /chat/api/feed` arrived with WP4, 2026-10-07;
  `POST /chat/api/folders/{id}/current` with WP5, the same day.*
- **`/v1/realtime?chat_thread=` keeps its `Admin` check until WP3.** WP3 moves the bind to `Chat`
  together with the policy check before the 101. *Done in WP3, 2026-10-06* (§1.3).
- **The expiry check is `policy::check_expiry`**, the expiry half of `usable`. The refusal is
  `Refusal::key_expired`, flat JSON like the rest of the Chat API: `key '<name>' expired on
  <date>`, the same words `/v1` has always used. *Corrected 2026-10-07 (reviews W2-18, W3-13):* a
  device is named `device '<name>'` (its name without `device:`) in every key refusal — the Chat
  gate's, `/v1`'s, a realtime upgrade's, a call refused mid-turn — and in a tool scope's refusal
  (`ApiKey::described`); every other key stays `key '<name>'`.

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
- *Changed during the build (review of WP4), 2026-10-07:* a temporary thread's attachments
  resolve their thread as a stored one's do (W4-2): the owner can attach the toolset to a
  temporary thread too. And the flag is checked where it is acted on (W4-3): a device's user
  message (send, edit, a dictated or spoken turn) is re-checked under the thread's lock, which
  a settings write that may attach the toolset also holds; the bound session's journal checks
  it before a spoken row; and a bind re-reads the thread once its binding is registered.
  *Corrected 2026-10-07 (review W5-18):* a device turn already running when the toolset is
  attached saves nothing either. Its save re-checks the device's reach under the thread's
  lock, which the attach holds too, and the turn ends `not_saved`.
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

*Changed during the build, 2026-10-06:*
- **Every model call a device's request causes, not only its turns** (review W2-3). Dictation,
  read-aloud of a stored reply, an attachment's transcript (at upload, on retry and in a turn)
  each pass `policy_checked_call` against the device's key and are charged to it. `voice/warm`
  makes no model call, but it loads and may evict for one: for a device, each stage's alias
  passes the key's scope and budget first, and a refusal (`403 key_scope`, `key_budget`) warms
  nothing. A read-aloud checks its TTS alias the same way before its background warm. Nothing a
  device triggers runs as `internal:chat` with the gateway's reach.
- **Who runs as whom.** The handlers take a `Caller` from the request's principal: an owner key
  or the cookie is the owner, and every other key runs as itself (a device is the only other
  one that holds `Chat`). The owner's turns keep `internal:chat`, no key check and
  `ToolScope::gateway()`.
- **Tools.** A device's tool loop also re-reads its scope at each call (`ScopedExecutor`), so a
  key narrowed mid-turn stops reaching. Its tool rows, and the model calls a docs or knowledge
  search makes, are the device's.
- **A knowledge base's embedder and reranker** are checked per call, but a refusal degrades the
  search instead of ending the turn: a retrieval never fails, and says what it could not do in
  its notes (code over spec). In tool mode the refusal is the tool's error result.
- **L3 in code.** `ChatRepo::thread_as` is the one resolution every thread-, message- and
  attachment-addressed route uses; an attachment is reached through its thread's kind. A
  device's delete of a thread it cannot reach is a 404, and so is Keep (`persist`) of a stored
  one; the owner's delete stays idempotent. The walk test enumerates the routes from
  `CAPABILITY_TABLE`.
- **The bind's alias check is realtime §10.2's**, scope and budget with a refusal row each
  (`policy_checked`), not `policy_checked_call`: the gate's `admit_session` has already checked
  expiry, the rate windows and the session's concurrency slot, and counting the handshake as
  three model calls would spend a device's rpm on a connection. The owner's bind keeps today's
  behaviour: no alias check, and no row for a refusal.

*Changed during the build (review of WP3), 2026-10-07:*
- **Nothing is loaded before the key's check** (W3-2). A device's turn with tools passes its key's
  scope and budget for the thread's model, not counted, before the tools are resolved and the GPU
  admission runs; each model call of the loop is then counted as it is made. The plain turn's
  counted check already came first.
- **`concurrency_limit` bounds a device's turns** (W3-8). A Chat turn a device starts (send,
  edit, regenerate, continue) holds one of its key's concurrent-request slots for its length; at
  the limit the route answers the key's own `429 key_rate` before anything starts, with its row.
  The slot checks no rate window: each model call of the turn is checked and counted as it is
  made. A bound realtime session's turns run in the slot the session holds.
- **A retrieval a device causes is the device's on every plane** (W3-3). The `kb` label attached
  by hand searches as the device, like tool mode's; and a device's `docs__query` and `kb__search`
  through `/mcp`, `/v1/responses` and a realtime session's tools are checked against its key and
  charged to it. Every other key's retrieval there stays the gateway's own, as it always was.

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

*Changed during the build, 2026-10-06:*
- **The ops' shapes.** `key_create {kind: "device"}` takes `name`, `hosts_label`, `url` and every
  `key_set` policy field. It answers `{id, name, key, link, url, url_note}`; `url_note` is the
  loopback note or `null`. `key_rotate` on a device takes `url` too. Rotate keeps the row: its
  id, policy and history stay, and only the hash changes.
- **The form is confirmed on a review step** (§11 Q1). "Pair a device" shows the scopes and the
  budget prefilled. "Review…" then restates what the device will reach: aliases, tools, budget,
  hosting label and address. The amber note appears there when the scope is `all` and there is
  no budget. Only "Pair the device" mints the key.
- **Where the card sits.** The card is a strip above the keys table, which stays the page's one
  scroller. Past two fifths of the page the card scrolls its own rows. A device is listed only on
  the card, so its actions are the ones that say what they end. The policy dialog edits a
  device's scopes, budget, limits, expiry and hosting label. Disable stays on the card, behind
  its confirmation.
- **The link has Open and Copy**, and is shown only until the dialog closes.
- **lmgw's own window opens the link** (found while building a desktop client). A WebKitGTK view
  never launches a URL-scheme handler by itself, so a click on the link in lmgw's window did
  nothing. The Tauri shell's navigation handler now passes an `lmgw-pair:` URL to `xdg-open` and
  cancels the navigation in the webview. It does this only when the link's `key` is one of this
  gateway's device keys: the Chat's HTML preview can navigate its own frame, and the handler
  cannot tell a frame from the page. A forged link from a preview cannot launch a client at an
  address of its choosing. The link is never logged.
  - *Corrected 2026-10-06 (review W2-1):* that last claim was false. Every device holds a valid
    device key, its own, and `Chat`, so it can put HTML in front of the owner whose preview
    navigates to a link with its key and an address of its choosing. The shell now hands off
    only the **exact** link `key_create` or `key_rotate` minted in this process
    (`devices::MintedLinks`), **once**, and then forgets it; a revoke forgets it too. A link
    replayed from a preview can then only be the owner's own fresh one, to the address the owner
    chose. Every other `lmgw-pair:` navigation is cancelled and not handed off. The dialog's Open
    works once; Copy stays. With no client registered for the scheme (`xdg-mime query default
    x-scheme-handler/lmgw-pair`), nothing is opened, since a generic `xdg-open` may fall back to
    a browser with the credential in the URL (review W2-17).
  - A client must confirm before it replaces an existing pairing, showing the new `url`.
  - *Decided, 2026-10-07 (review W3-15):* a minted link has no time limit. It is handed off
    once, and forgotten when its key is rotated (the new link replaces it) or deleted, as on
    every revocation; closing the dialog does not drop it.

### 1.5 The hosting grant [Desktop 2]

- **`hosts_label`** is the label (tool prefix) the device may host tools under. It is validated like a
  `tool_prefix`: unique among the `mcp_servers` rows' prefixes and names, and not reserved
  (`mcp/mod.rs:397`).
- **Setting it creates the device's server row** (§5.2); clearing it deletes the row.
- **Without a grant,** `GET /mcp/host` is a 403 `host_not_granted`.

### 1.6 Revocation, expiry, last seen (L17, L18)

- **A disabled device** is a 401 `device_disabled`: "device '<name>' is disabled — enable it on
  Usage → Keys". It is a new arm of `principal.rs:354-369`. A rotated or deleted key matches no row,
  so its bearer is `Anonymous`, and `Chat` routes answer 401 `session_required` (*changed
  2026-10-07:* a device key with no row is 401 `device_key_unknown`, §1.8).
- **Disable, Rotate, Delete and `expires_at` end every open connection** of the device at once:
  - the feed sends `event: revoked` with `{reason}`, naming which of the four happened, then
    closes;
  - `/mcp/host` and realtime sessions close with 4003 and the same reason.

  One revocation signal keyed by key id is watched by those long-lived tasks; expiry raises it
  from a timer.
- **`last_seen_at`** (L15) is written when a feed, link or realtime connection of the device opens or
  closes. The card shows "online (feed, voice, tools)" while any is open. `last_used` stays the
  usage hour.

*Changed during the build, 2026-10-06:*
- **One signal, keyed by key id.** `devices::Revocations` holds a generation on a `watch` channel
  and the last revocation of each key. Every connection watches it, and a lagging watcher cannot
  miss a revocation.
- **When the signal fires.** Disable (`key_set`), Rotate and Delete raise it after the snapshot
  reload, so a client that reconnects at once meets the gate's refusal.
- **A mark closes the race at the door.** A principal is resolved with the generation of that
  moment (`RequestCtx::revocation_mark`), and its connection counts only revocations raised after
  it. A Disable that lands between the gate and the 101 still ends the connection.
- **Expiry has no central timer.** Each watch sleeps until its key's `expires_at` as the
  snapshot says it now, and re-reads on every change; `key_set` re-arms the watches when
  `expires_at` changes. Only a connection needs to know, so there is no timer task to keep
  alive.
- **Devices only.** The signal is raised for device rows only. A client key's realtime session
  keeps today's behaviour: its next model call is refused.
- **What ends today.** WP2 ends realtime sessions: the close is 4003 with the reason `revoked:
  device '<name>' was disabled` (or `was rotated — pair it again`, `was deleted`, `expired`).
  The feed (WP4) and the host link (WP7) watch the same signal through
  `devices::connect(…, LinkKind::Feed | Tools)`. The same call counts them as online and stamps
  `last_seen_at`.

*Changed during the build (WP3), 2026-10-06:*
- **Every key kind** (decided 2026-10-06: Disable means disable). Disable, Rotate and
  Delete raise the signal for every key: a client key's realtime session closes with 4003
  (`revoked: key 'laptop' was disabled`) like a device's, and so does an owner key's after a
  Rotate. Only a device's connections count as online and stamp `last_seen_at`. This replaces
  the "devices only" note above.
- **Any path.** Every published snapshot re-arms every watch, which re-reads its key: a key
  disabled, rotated or deleted by any path (an agent's page, a restore) ends its connections,
  not only the ones whose op raised the signal by name.
- **The credential, not only the row** (review W2-2). A principal carries the fingerprint of
  the key it was resolved with (the head of its hash). `policy_checked_call` refuses a call
  whose key was rotated since, and a watch that missed the signal sees the rotation in the
  snapshot.
- **Chat streams and `/mcp` watch the signal too** (review W2-2). A device's `send`,
  `continue`, `regenerate`, `edit`, `speak` and `voice/warm` streams end with a last `error`
  frame `{code: "revoked", message}`. The turn behind the stream hears its reader go and stops,
  saving what it had. `/mcp`'s notification stream of any key ends without a frame (MCP has no
  notification for it).
- **The edges** (review W2-5 to W2-7). The mark is read before the snapshot the credential is
  resolved against, and the `Chat` gate refuses `401 session_required` for a row gone or rotated
  since the root resolved it. A Disable, Rotate or Delete raises the signal even when the
  snapshot reload after it fails, and Rotate and create still hand back the minted key, with the
  failure said beside it. *Corrected 2026-10-07 (review W3-7):* that alone left the published
  snapshot admitting the revoked key to new requests until a reload worked, and an owner key's
  Rotate raised nothing when its reload failed. Now a failed reload lays the write over the
  published snapshot (the row disabled, gone, or holding the new hash), so the old credential is
  refused at once, and an owner key's Rotate raises the signal and hands back the new value with
  the failure said beside it, like a device's. A watch re-reads the wall clock at least every 30 s (a tokio sleep does
  not advance across a suspend), so an expiry is honoured within that whatever the clock did.
- **Owner keys in the Chat too** (review W3-6, 2026-10-07). An owner key's Chat streams are
  watched like a device's: its Disable, Rotate or Delete ends its running turn, read-aloud or warm
  with the `revoked` frame, and a tool loop stops with its reader. The dashboard's session is an
  owner key, so its own Rotate ends the turn it has running. An in-process caller holds no key and
  is not watched.
- **Open links counted** (review of WP5, decision 5 on W4-25): no cap on feeds per key; the
  Keys list's row carries `open_links` (`[{kind, count}]`) beside `online`, and the Devices
  card prints a kind held more than once with its count ("online (feed ×3, voice)"), so a
  client's reconnect pile-up is seen. *2026-10-07.*
- **The plain reason** (the first client's pairing, 2026-10-07). `revoked`'s `message` and the
  4003 close's reason are the reason alone — "device 'desktop' was disabled", "… was rotated —
  pair it again", "chat thread 7 is out of reach for this key" — without the `revoked:` they
  began with: the event's name and the code say it, and a client showing "revoked (…)" said it
  twice.
- **Closes a client can tell apart** (the desktop client, after the merge, 2026-10-07). A 4003
  stood for a revocation and for a thread out of reach alike, so a client read the reason's words
  and told a device to pair again when only its thread had gone. Now:
  - **4004** (`CLOSE_OUT_OF_REACH`; `CloseKind::OutOfReach`) is the close of a bound session whose thread
    left the key's reach — the toolset attached, the device's switched off, or the gateway's
    level set to off (L3's notes), and a device's session whose thread was deleted (the note
    below the table). The reason stays neutral, "chat thread 7 is out of reach for
    this key"; the key is still good, and the client asks for its current thread again.
  - **4003 is a revocation only**, and its reason starts with a stable token, a colon, then the
    sentence (`devices::close_reason`, `RevokeReason::kind`): `device_disabled` (a paired
    device disabled), `key_expired` (a paired device's key expired), `key_unknown` (a paired
    device's key rotated or deleted, not told apart: pair again, as the next request's `401
    device_key_unknown` says) and `revoked` for every other kind of key, whatever happened —
    "device_disabled: device 'desktop' was disabled". The cut to 123 bytes keeps the token. This
    takes back "the plain reason" above for the close; the feed's `message` stays plain.
  - **The feed's `revoked` carries the same token** as `kind`, beside `reason` and `message`
    (`RevokeKind`, with `Unknown` for a newer gateway's token; an older gateway's event without
    one reads as an empty `Unknown`, as a close reason without a token does — review G-8; the
    served schema states no default for it, since the empty `Unknown` is no value of its enum,
    and its description says an event without it is of an unknown kind — the branch review's
    verification V-8). A Chat stream's last `error {code: "revoked"}` frame carries the same
    `kind` (review G-7).
  - **`lmgw-client` reads it**: `realtime::close_kind(code, reason)` gives `CloseKind` —
    `Revoked {kind}`, `OutOfReach`, `ShuttingDown` (1001), `TakenOver` (4000), `Other {code}`
    for any code outside 4000–4999 but 1001, and `Unknown {code}` for an application code this
    build does not know (*corrected by review G-10:* a token is `[a-z0-9_]+`, bare or before the
    colon)
    — and the crate re-exports the four codes. A 4003 without a token is an empty
    `RevokeKind::Unknown`.

  | Close | Token / reason | Meaning for the client |
  |---|---|---|
  | 1001 | "lmgw is stopping or restarting" | reconnect once it is back |
  | 4000 | "voice mode moved to …" | another client took the thread |
  | 4003 | `device_disabled: …` | wait: the device may be enabled again |
  | 4003 | `key_expired: …` | wait: the expiry may be moved |
  | 4003 | `key_unknown: …` | pair the device again |
  | 4003 | `revoked: …` | any other key's revocation |
  | 4004 | "chat thread N is out of reach for this key" | ask for the current thread again |

  *Changed after the desktop client's live check, 2026-10-07 (the deleted thread decided by
  the owner):*
  - **The 4004 follows the write that moved the thread.** An attach of the toolset closed the
    device's session before the attach was committed, and for a few milliseconds after the close
    the client's next reads still found the old reach: `POST /chat/api/folders/{id}/current`
    named the thread it had just lost, and a new bind to it was accepted and closed with a second
    4004, or refused. Now the close is raised only once the write is committed, and for a level
    once the snapshot that says it is published (`LiveTurns::self_admin_changed`,
    `LiveTurns::reach_changed`, `LiveTurns::discarded`; `chat_live::voice`'s module doc).
    Before its commit an attach only stops the thread's live events reaching the devices that
    will not see it (`LiveTurns::self_admin_attaching`). A level's write commits its feed record
    before it publishes the snapshot, so a device's stream that reads the record in between holds
    it until the snapshot says its level — the writer wakes the feed after its publish — for up
    to about two keep-alives (one from the record's first read, found at the tick after it),
    after which the log says so and the record plays (`FeedStream::snapshot_behind`). So a client that reads on the 4004 or on the feed's
    `thread.deleted` — `current`, the bind, the thread by id, its feed — finds the new reach:
    `current` starts a new thread, and the bind to it stays open. The rollover takes the write
    lock up front (`store::create_current_thread`): it meets the closing session's last writes,
    and a deferred transaction was at times refused "database is locked" at once.
  - **A deleted thread ends a device's session as one out of its reach.** The delete left the
    session bound until it closed by itself, and its next spoken turn was told "chat thread N is
    gone (deleted …)": a device told a deletion from a thread hidden from it, which review W4-18
    ruled out. Now, once the delete is committed — the thread's own, its folder's with
    `threads: "delete"`, or the sweep's purge — a device's session on it sends the same `error`
    (`chat_thread_not_found`, "chat thread N is out of reach for this key, and this session
    closes") and closes with the same 4004 and reason. The owner's session on a deleted thread
    is left alone, as before. A folder a device's delete hides from devices (`devices_hidden`)
    takes no thread out of their reach by itself: the threads that delete took close as deleted.
  - **And so does everything else a device hears of it** (the same day, decided by the owner). Two more places told a deleted thread from a hidden one. A device's feed heard
    `voice.ended {reason: "thread_gone"}` for a deleted thread, and nothing for a hidden one; now
    a thread being deleted has its live turns and bound sessions reported to the owner alone from
    the delete on, `turn.done` and `voice.ended` included, as an attach does for a hidden thread
    (`LiveFeed::thread_going`, Admin Chat's level; a delete that fails takes it back), and every
    device's stream sends a fresh `state` without them, so `hello` and `state` on a resume leave
    them out too. A device hears `thread.deleted` and that `state`, for a delete as for a hide;
    the owner's `voice.ended` still says `thread_gone`. And a spoken turn in flight before the
    close was told "chat thread N is gone (deleted …)" for a deleted thread and
    `chat_history_write_failed` for a hidden one; for a device both are now
    `chat_thread_not_found` with the close's words, "chat thread N is out of reach for this key"
    (`realtime::thread::not_there`: the journal's user entry and the bound turn). The owner's
    turn still hears that the thread is gone.
  - **The delete's own order** (the branch review, the same day). A delete did more than narrow
    the live events before its commit: it cancelled a device's turn on the thread and moved its
    history's generation, so a device in voice mode heard its response fail while the thread still
    read as there. Now a delete does before its commit what an attach does — the thread's live
    events leave every device's reach (`LiveTurns::discard`) — and the rest under the thread's
    lock once it committed: the generation moves, the turn is cancelled, the session's
    `voice.ended` says `thread_gone`, a device's session closes (`LiveTurns::discarded`). A delete
    that fails only takes the narrowing back. A turn or a session registered from a read made
    before the delete is reported to the owner alone too, and a session's end says `thread_gone`
    (`going` beside `flipped` in the live feed, kept until an hourly prune finds it unused
    twice); a bind of a thread deleted meanwhile is refused 404 for the owner too. A folder
    delete ends the threads the delete took (`store::delete_chat_folder_ids`), not the ones it
    read before taking their locks: one a move took out goes back, one a move brought in goes as
    a purged one does. The sweep's purge cancels the purged threads' turns and closes a device's
    sessions before the feed is woken (`store::sweep_chat_threads_ids`, `LiveTurns::purged`). A
    device's feed opened between a level's commit and its publish reads its levels once the
    snapshot says them too, for up to one keep-alive (`chat_feed::levels_as_published`). A
    device's spoken turn dropped on a deleted or hidden thread is said in the log, which names
    the cause.

  *Changed after the branch review's re-verification, 2026-10-08:*
  - **A level plays at the publish that says it.** The write of a device's level woke the feed,
    closed the sessions out of reach and refreshed the devices' `state` only after its MCP
    reconcile, which one unreachable autostart server holds for its connect timeout; a device's
    stream holding the level's record, a feed opened in between, and the 4004 all waited for it.
    Now the publish that moves a device's level, or the gateway's, does all of that
    (`AppState::publish_over`), and the reconcile comes last (`ops::key_set`, as
    `ops::hold_set` splits them). A reload that loaded before a write committed no longer
    publishes over one that loaded after it (`SnapshotLoads`): an older snapshot never takes a
    published level back.
  - **A feed opened before the publish starts at once.** It waited for its levels with no status
    and no byte, for up to one keep-alive (15 s by default); a client that waits less for
    `hello` (the desktop client's `lmgw.timeout_s`, 10 s) gave up, and dropping the request
    cancelled the warning meant to say why. Now the response starts with a keep-alive comment,
    the log says the wait as it begins, and `hello` follows once the snapshot says the levels,
    or after one keep-alive at the store's, as before (`chat_feed::opening`, in place of
    `levels_as_published`). `hello` is still the first event (§2.1).
  - **A delete or an attach runs to its end.** A client that hung up between the write's first
    step and its commit left a thread hidden from devices after the commit rolled back, or a
    deleted or attached thread with a device's session still bound. The write now runs on a
    task of its own, with its locks, and the request waits for it (`chat_live::to_its_end`).
    Two deletes of one thread, its own and its folder's, no longer undo each other's
    narrowing: a failed one takes nothing back while the other is under way, or once it
    committed.
  - **The owner's bind of a thread it cannot read again** is a 500 `internal`, not the 404 of a
    thread that is not there; a thread deleted meanwhile is still the 404.

  *Changed after the last review's follow-ups, 2026-10-08:*
  - **A malformed settings blob opens a device's feed at once.** The snapshot loader reads a
    stored blob that does not parse as the defaults, and the feed read the gateway's level as
    the one field in it: the two disagreed for good, and every device's feed waited a full
    keep-alive before it opened. A blob that was not JSON failed the open. The feed now reads
    the level through the snapshot's own parse (`store::settings_from_blob`).
  - **A panic inside a delete or an attach takes its narrowing back.** The thread stayed out of
    the devices' live view until the gateway restarted. The narrowing is a guard now, given
    back unless the step after the commit ran (`chat_live::Going`, `chat::Attaching`).
  - **The MCP reconcile after a level's publish runs to its end** when its request is dropped,
    and a server's connect handshake is bounded by its `timeout_ms` (the MCP gateway design's
    §9 note of this date).
- **A stop ends every long-lived connection** (the first client's pairing, 2026-10-07; the
  heading said "with a word", which the streams that end without a frame contradicted, final
  review F-16). axum's graceful shutdown waits for open connections, and an open feed kept a
  headless gateway's Ctrl-C waiting for as long as its client stayed; the process that then
  exited reset a bound session mid-close (1006). Every long-lived stream now ends at its
  server's stop (`server::Stops`, a generation, so a restart's next server's streams run on):
  the feed, `/mcp`'s notification stream and `/api/events` end without a frame, a Chat stream
  with a last `error {code: "gateway_stopping"}`, and a realtime session closes with 1001
  (`lmgw-api-types::realtime::CLOSE_GOING_AWAY`, `SHUTTING_DOWN`; the reason reads "lmgw is
  stopping or restarting" since F-16, one sentence for a quit and a restart, which a client
  cannot tell apart until it reconnects). The server returns only once its sessions have
  ended (their close sent, a bound thread's last turns written).

  *Changed (the final review's F-1 to F-4), 2026-10-07:*
  - **Every quit of the tray app stops the server first** (F-1). The tray's Quit, an update's
    restart (`request_restart`) and SIGTERM/Ctrl-C all ask Tauri to exit, and the first exit
    request runs one sequence (`src-tauri gateway::quit`): the server's stop (its streams end,
    its sessions close with 1001), the wait until `serve` has returned, at most `QUIT_WITHIN`
    (the server's own bound and 2 s) and said in the log when that runs out, and only then the
    model containers' stop (`lifecycle::shutdown`); then the exit. Before, Quit force-stopped
    the containers under the running sessions and turns, and the server was abandoned as the
    process died. Tested against a real server (`gateway::tests`) and, live, in the debug shell
    (`scripts/shell-check.py --only quit`: SIGTERM with the feed open; the feed ends whole, the
    log's steps come in order, exit 0).
  - **A stop waits for its turns** (F-2). A Chat turn, a realtime response's model call, a bound
    voice turn and a request row's write each hold a `Running` (`Stops::running_at`,
    `Stops::writing`): a cut turn's partial save and its row are written before the server
    returns, and a headless Ctrl-C no longer loses them.
  - **A request carries its server's generation from its arrival** (F-3, `ServedAt`, put on
    every request by `serve_app`, `RequestCtx::served_at`): a feed still reading its catch-up,
    or an upgrade still in its handshake, when the stop comes ends with the others; a session's
    `Running` is taken in the handshake, before the 101.
  - **One bound for the whole stop** (F-4, `STOP_WITHIN`, 10 s): the drain and the wait for
    sessions and turns share it, and what was still in flight is named in the log
    (`Stops::open_requests`, counted per request by `serve_app`). A response that ends neither
    by itself nor at the stop — a `/v1` stream whose upstream stopped answering — no longer
    holds it. The agent proxy's streamed bodies end at the stop too, and so does its WebSocket
    tunnel (both its sockets are dropped: a byte pipe has no frame to say why); the exemption
    this note named is gone. A second Ctrl-C or SIGTERM exits at once, said in the log, in the
    headless binary and in the tray app.

  *Corrected (the pre-merge review's P-2, P-11, P-13, P-14), 2026-10-07, decided by the owner:*
  - **An update's restart runs the sequence before it restarts.** The F-1 note above is wrong
    for it: Tauri ignores `prevent_exit` for a restart's exit code, so an exit request could
    not hold a restart for the sequence, and the containers stopped beside a server still
    stopping. The updater now runs the quit sequence itself (`gateway::quit_then` with
    `AfterQuit::Restart`) and asks for the restart once it has run; the exit request then finds
    the quit done. An exit that goes through while a quit runs waits for it in `RunEvent::Exit`
    (`Gateway::wait_quit`, bounded by `EXIT_WAITS_WITHIN`, said in the log when it runs out),
    never a second containers' stop beside it. Tested with the seam F-1's quit is tested with:
    the server stops and is waited for, the containers stop, then the restart.
  - **A quit is seen, and always ends.** The window hides, and the tray's tooltip, status line
    and Quit item ("Quitting…", greyed out) say it while the sequence runs; a Quit asked for again
    is logged. The exit or restart runs from a guard, so a panic inside the sequence still ends
    the app, marked failed, and the exit stops the containers then.
  - The quit's log says whether the gateway returned; a second signal exits with that signal's
    status, 143 for SIGTERM and 130 for Ctrl-C.
- **Pushed, not polled.** `/api/events` carries a `keys` frame (`{key_id, what}`: `link`,
  `disabled`, `rotated`, `deleted`) when a device's connection opens or closes and when a key is
  revoked. The Keys page refetches on it, and on requests landing (at most every 15 s); it
  polls nothing.

### 1.7 Naming the binder

- **`LiveTurns::bind_voice`** (`web/chat_live/voice.rs:53-76`) takes the binder's description and
  keeps it with the binding.
- **The session taken over names it,** in the `chat_thread_taken_over` message
  (`realtime/thread/hooks.rs:57-66`) and the close reason (`realtime/thread.rs:73`): "voice mode
  moved to device 'phone'", or "…to the dashboard" for an owner session.
- **The feed's `voice.ended`** carries the same `by`.

*Built in WP3, 2026-10-06:* the bind passes its caller's name to `bind_voice`, and the session it
takes over says it in `chat_thread_taken_over` and in the 4000 close ("voice mode moved to device
'phone'", "voice mode moved to the dashboard"). The dashboard's voice panel shows the close's
reason.

*Added (the first client's review), 2026-10-07:* **a bind that never takes over**,
`/v1/realtime?chat_thread=<id>&takeover=never`. While another session is bound to the thread
the upgrade is refused, 409 `chat_thread_bound` ("voice is in use on device 'phone'", the
takeover's binder naming), and nothing is taken over; without the parameter a bind takes over
as before. The check and the registration share the thread slot's lock
(`LiveTurns::bind_voice_unless_bound`). A client uses it for its own automatic rebinds after a
rollover, so it never takes the voice from a phone that followed the same rollover; a user's
explicit press binds without it and wins. `lmgw-client`: `requests::realtime_unless_bound`.

*Changed (the final review's F-5, F-15), 2026-10-07:* two holders are not "another session":
one bound with the same key (the device's own session, after a dropped link the gateway has not
seen yet), and one whose session is already ending (it left its loop: after its 1001, a
revocation or a close, while it writes its last turns; `VoiceBinding::closing`). `takeover=never`
takes such a binding over, its fence kept, so the new journal still writes after the old one
drained; a client's automatic rebind is never refused in its own name. And a refusal re-reads
the thread first: one that left the binder's reach meanwhile is the 404, never "in use".

### 1.8 Refusals

| HTTP / close | `code` | When |
|---|---|---|
| 401 | `device_disabled` | the matched device row is disabled |
| 401 | `key_expired` | a device key past `expires_at`, on any route |
| 401 | `device_key_unknown` | a rotated or deleted device key (it matches no row; *changed 2026-10-07*, was `session_required`) |
| 403 | `forbidden` | a device on an `Admin` route, or creating an admin thread |
| 403 | `tool_label_out_of_scope` | a device writes a label L5 refuses |
| 403 | `approval_loosen_refused` | a device writes a `require_approval` that gates fewer calls of a server than the stored rule or the owner's floor (§6.6; *2026-10-09*) |
| 403 | `chat_toolset_needs_full` | a device whose admin tools are below `full` (its own level capped by the gateway's) attaches the self-admin toolset, or changes the settings or messages of a thread that carries it or the defaults of a folder that does, an ongoing folder's change reaching such a current thread included (L5's notes; *the branch review's verification, 2026-10-07*). The same settings sent back are no change |
| 403 | `host_not_granted` | `GET /mcp/host` without a hosting grant, or from a non-device principal |
| 403 | `cross_origin_refused` | `GET /mcp/host` with an `Origin` header (§5.6) |
| 404 | `not_found` / `chat_thread_not_found` | a device reaching an admin thread by any id (L3); a thread or folder with the self-admin toolset for a device not allowed lmgw's admin tools (*2026-10-07*) |
| 409 | `folder_no_model` | `current` on a folder whose defaults name no model (§3.3) |
| 409 | `not_ongoing` | `current` on a folder that is not an ongoing conversation (§3.2) |
| close 4003 | `device_disabled` / `key_expired` / `key_unknown` / `revoked` (the reason's leading token) | a revocation (§1.6; the tokens since the desktop client's note, 2026-10-07) |
| close 4004 | — (neutral reason, no token) | a bound thread left the key's reach (L3; was a 4003 until 2026-10-07) |

*Changed during the build, 2026-10-06:* a device's Chat SSE stream ends with an `error` frame
`{code: "revoked"}` (§1.6); `401 key_expired` and the 4003 close now apply to every key (§1.6). A
device's bind, dictation and warm refuse an alias outside its key's scope or budget with the
key's own `403 key_scope` / `key_budget`, as `/v1` does.

*Changed during the build (the first client's pairing), 2026-10-07:* **a device key that matches
no row is `401 device_key_unknown`**, "this device key is no longer valid (rotated or deleted) —
pair the device again", on every route but a public one, `/v1` and the realtime handshake in
their own shape. The key's prefix (`lmgw-device-`) says it was a device's even with no row
behind it; `session_required` gave a device the dashboard's login hint. It does not say which of
the two happened. The `Chat` gate gives the same refusal to a device whose row went or was
rotated between the root's resolution and the gate (W2-5). The client crate reads it with
`requests::key_refused` (`PairAgain`, beside `Disabled` and `Expired`).

### 1.9 Documents A updates

- the `/v1/realtime` `DocRoute`'s "`chat_thread` (dashboard only)" and its refusal list;
- the `bind.rs` module-doc table;
- chat-voice §8.1's refusal table, and §8.8's "pass trivially" and NIT 12;
- the labs' doc comment ("no key of their own", `web/mod.rs:147-149`);
- principals §3.2's table, with a pointer to this record.

*Changed during the build, 2026-10-06:* WP2 updated the documents that became true with it:
- the labs' doc comment: the Chat routes are their own router, `Chat`; the labs stay `Admin`;
- principals §3.1 and §3.2, with a pointer to this record;
- the `/v1/realtime` `DocRoute`, for the 4003 close.

The binding's documents describe the bind's capability and checks, which WP3 changes. They are
updated with it:
- the `DocRoute`'s "dashboard only";
- the `bind.rs` table;
- chat-voice §8.1;
- chat-voice §8.8 and NIT 12.

*Done in WP3, 2026-10-06:* all four, and `RealtimeQuery::chat_thread`'s own doc, which the
`DocRoute` derives its parameter from.

## 2. The Chat change feed (B) [Desktop 1, Android]

### 2.1 Route

`GET /chat/api/feed`, SSE, `Chat` capability.
- **Resuming:** `?since=<cursor>`, or the standard `Last-Event-ID` header. A cursor is
  `"<epoch>:<seq>"` (*as built:* `"<epoch>:<seq>:<tag>"`, §2.3). With neither, the feed starts at
  "now".
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

*Changed during the build (WP4), 2026-10-07:*
- **`Last-Event-ID` wins over `?since=`.** A browser's `EventSource` reconnects with the URL it
  opened, so its `since` is the old cursor and the header the new one. A value that is no cursor
  is a 400 `bad_request` naming the shape; a cursor the feed cannot honour is §2.4's `resync`.
- **`hello` comes first, always**, then the `resync` a cursor may owe. `hello.cursor` is where the
  stream continues: the cursor given, or the newest record when there was none or it got a
  `resync`. *Changed 2026-10-08 (§1.6's close-code note):* a device's feed opened while its
  admin-tools level is being published starts with a keep-alive comment, and `hello` follows
  within one keep-alive. `hello` is still the first event.
- **`hello` also carries `retention_days`** (`chat_feed_retention_days`), so a client knows how
  long it may stay away. `principal` is `{kind: "owner" | "device", name}`: the dashboard and every
  owner key are `{owner, dashboard}` (the key's name without `owner:`), a device is its name without
  `device:`.
- **The keep-alive interval is the stream's own**: a stream keeps the value it opened with and
  announced, and a changed setting applies to feeds opened after it.
- **Documented now.** The route has a `DocRoute` under a new "Chat" tag; its events are typed in
  `lmgw-api-types::chat_feed`. The rest of the Chat API stays excluded until WP6. *WP6 documented
  the desktop subset beside it (§4.3).*

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
| `device.revoked` | `{device: {kind: "device", name}, by}` (added 2026-10-09, below) | stored | Desktop 3 |
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

*Changed during the build (WP4), 2026-10-07:*
- **Where `by` goes.** A stored event's data is the rendering with `by` beside its fields (`null`
  for the gateway's own changes: the sweep, a voice seed drawn on first use). A thread or folder
  gone by delivery renders as `{thread_id | folder_id, deleted: true, by}`; `thread.deleted` and
  `folder.deleted` use the same shape, so they gain `deleted: true`.
- **What writes `thread.*`, as built:** create (the Chat's, a folder's, a catalog agent's "Open in
  Chat"), Keep, settings, the title (named from the first send, or from a spoken turn), pin, archive
  and restore (a send into an archived thread included), move, delete, a folder's delete (its
  threads deleted or taken out), an agent's delete (its threads unlinked), the voice seed drawn on
  first use, and the sweep.
- **A folder's counts** move with its threads, and no `folder.updated` is recorded for that: a
  client follows `thread.*` (whose rendering carries `folder_id`). A `folder.*` event renders the
  counts as they are at delivery, for the reader (L3).
- **A thread's `updated_at`** also moves with every message, without a record: that is
  `message.added` (WP10), which records in the same message writes.
- **`turn.done`:** `message_id` is `null` when the turn saved nothing; `code` is the code of the
  last `error` frame the turn sent that carried one (`superseded`, `gpu_hold`, `not_saved`, …), so
  a tool label reported without a code does not count.
- **`voice.ended`:** `by` names who bound the session that ended, as its `voice.bound` did.
  `taken_over_by` names the binder that took it over, for `reason: "taken_over"` (§1.7's "the same
  `by`"; one field per binder keeps both readable). `thread_gone` means the thread was deleted
  while the session was bound: the session itself stays bound until it closes, as a delete has
  always left it, and its `voice.ended` says why. *Changed 2026-10-07 (§1.6's close-code
  note):* a device's session closes once the delete is committed, as one whose thread left its
  reach; the owner's stays bound. A deleted thread's `voice.ended` and `turn.done` reach the
  owner alone, as a hidden thread's do: a device hears `thread.deleted`.
- **Temporary threads** have no live events either (L7): no `turn.*`, no `voice.*`.

*Added 2026-10-09 (`device.revoked`; for the desktop client's WP18):* a paired device's key
deleted is a stored event, so a client that runs work the device started (a host of MCP tasks
whose `lmgw/caller` named it) may cancel that work; before it, a client heard only its own key's
`revoked`. Recorded in the delete's own transaction (`store::feed::record_device_revoked`, from
`store::delete_api_key`), `by` the owner (key ops are on `/api` alone). `device` names the deleted
device as `hello.principal` names one and as `lmgw/caller` does on a forwarded call; its `by` form
(`FeedPrincipal::by`, "device 'phone'") is what its changes carried.
- **Who hears it** — no device the reader was not shown (L3): the dashboard and admin keys; a
  device that may see a stored record carrying the deleted device's `by` (the record's level is
  the lowest of those records', the owner's alone when the feed holds none, e.g. pruned by the
  retention); and a device hosting a task the deleted device started that was still open —
  running, or cancelled with the cancel still owed to its server (`cancel_owed`) — (its
  `lmgw/caller` named the device on the call, whatever the thread's level; the record keeps those
  hosts' key ids in `detail`).
- **Only a delete.** A disable and an expiry are reversible (the key can be enabled or renewed;
  a disabled host's tasks wait, MCP Tasks design §1.6), a rotate keeps the row and the device
  pairs again under its name, and a cleared hosting grant takes away hosting, not the device: the
  tasks it hosted end `abandoned`, each said by its `task.done`. None of them records anything.
- **lmgw ends nothing for it.** The deleted device's tasks run on and their results enter their
  threads; what to cancel is the host's call (the client's ruling; MCP Tasks design T7, T16).

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

*Changed during the build (WP4), 2026-10-07:*
- **The table** (migration 0063) adds two columns to the record. `admin` is the thread's kind at
  the write: a kind never changes, and a deleted thread can no longer be read at delivery, so a
  device's feed needs it to leave out an Admin Chat thread's `thread.deleted`. `detail` holds an
  event's facts that are not state (`folder.current`'s previous thread and reason). `at` is
  `datetime('now')`, like the Chat tables. `seq` is `AUTOINCREMENT`, so a number is never handed out
  twice.
- **The meta row** holds the epoch and `pruned_through`. Retention deletes a prefix of the
  sequence and moves `pruned_through`, and the newest number is SQLite's own counter for the table.
  A cursor `c` is whole while `pruned_through <= c <= newest`.
- **Same transaction, everywhere.** Every write that records does so in its own transaction:
  thread create (every path), Keep, settings, title, pin (with its restore), archive, restore,
  move, delete, the folder writes (a folder's delete with each thread it deletes or takes out),
  an agent's thread unlink, the seed draw, and each half of the sweep. A write that fails, or
  matches no row, records nothing. No write had to record outside its transaction.
- **What follows a commit is in-process only.** The wake (`Feed::wake`) and the live events are
  sent after the commit. A crash between the commit and the wake loses nothing: the record is in
  the table, and the process and its streams are gone with the crash. A client resumes from its
  cursor and reads it.
- **The `watch` carries a generation, not the newest `seq`.** The store's writes do not hold the
  gateway's state, so the layer above wakes the feed after each one. A stream also reads the table
  at every keep-alive tick, so a path that forgot to wake delays its event by one interval at most,
  and never loses it.
- **A live event waits for the table.** Before a live event goes out the stream reads the table
  up to date, so `turn.started` never precedes its thread's `thread.created`.
- **Two more settings, both in Settings → Chat → Change feed.** `chat_feed_page_size` (default
  500) is the records one catch-up query reads; it bounds memory, and every record is still sent.
  `chat_feed_live_buffer` (default 256) is the live events held for a slow reader. A reader that
  falls further behind gets `state` with a `reason` naming the setting, and the stream
  resubscribes, so what follows agrees with that state. A changed buffer size applies at once: the
  open streams move to the new buffer with a `state` that says it was resized.
- **L3 and the gaps it leaves (decided from the code).** A device's stream skips every record and
  live event about an Admin Chat thread, so the `seq` values it sees have gaps. Gaps are accepted,
  and there is no per-principal sequence. A gap tells a device that something it may not see
  changed at about that time, never which thread or what. The ids a device already holds tell it
  as much: thread, message and attachment ids are each one sequence across every kind of thread,
  so a device's new thread id already counts the Admin Chat threads created since. A dense
  numbering per principal would need a second sequence per principal and cursors bound to a
  principal, to hide timing the ids show anyway. `hello`, `state` and a folder's counts are built
  per reader and carry no Admin Chat thread.

### 2.4 `resync`

A cursor whose epoch is not this database's, whose `seq` is older than the oldest kept row, or
newer than the newest, gets `resync {reason}`, then events from now.
- The reason names the cause, e.g. "the cursor is older than the feed keeps (Settings → Chat →
  feed retention: 7 days)" or "the cursor is from another database".
- The client reloads what it shows. Nothing is skipped silently.
- *Added 2026-10-07 (the branch review's 3b):* a client may coalesce consecutive `resync`s into
  one reload; a catch-up across two level records sends one per move (L3's note).

*Changed during the build (WP4), 2026-10-07:* the reasons name the setting as the page shows it
("Settings → Chat → Change feed → retention: 7 days"). A cursor newer than the newest event says
the data was restored from an older copy.

*Changed during the build (review of WP4), 2026-10-07:*
- **`admin` is a flag, not a kind** (W4-12). Since W3-1 a record's `admin` says whether the
  thread or folder drove the self-admin plane at the write; the flag can change, a write that
  flips it stores `admin_was` in `detail` beside `folder.current`'s facts, and a device's
  rendering re-checks the state as it is now. Migration 0063's comment still says "the
  thread's kind": a migration's text cannot change (its checksum), so the Rust doc and this
  note say it instead.
- **`principal`, both facts** (W4-12). An owner key's feed is `{kind: "owner", name}` with its
  key's name without `owner:` (`cli` for `owner:cli`, `dashboard` for the dashboard), while
  every `by` an owner key causes says "the dashboard".
- **Nothing is skipped silently.** A page is read with the table's bounds in one transaction:
  retention that prunes past a stream still catching up is its `resync` (W4-4). A record whose
  thread or folder cannot be read is an error, never a tombstone: the stream waits at it and
  reads it again at its next wake or keep-alive (W4-5). The retention reason names the
  setting as the page labels it, *Keep changes for*, and with retention now 0 says what was
  pruned under an earlier value (W4-12, W4-24).
- **A restored copy is another database** (W4-11). The feed keeps a mark of the newest
  cursor it handed out beside the database (`chat-feed.mark` in the data directory, written
  when the head it covers moves). At start, a database older than its mark under the same
  epoch gets a new epoch, so every client resumes with a `resync`.
  - *Changed 2026-10-07 (review W5-4, decided by the owner):* the mark caught only a database
    file restored on its own; restoring the data directory as a whole (a snapshot rollback,
    an rsync from a backup, a dev copy) brought the mark back with it. **The cursor checks
    itself now:** it is `"<epoch>:<seq>:<tag>"`, the tag 8 hex digits of a hash of the record
    at `seq` (when, what, which thread or folder, by whom; `store::feed::Record::tag`). At a
    resume the record the table holds at that number must carry the same tag, or the client
    gets `resync` ("its data was restored from an older copy"), whatever the numbers say. The
    mark file is gone (less code). A cursor at a number the table no longer holds (the
    retention's boundary) or without a tag is checked by its numbers alone, as before;
    clients treat the cursor as opaque text.
  - *Changed 2026-10-07 (review W6-3, decided by the owner):* **the tag is a random draw
    stored with its record**, 16 hex digits (`chat_feed.tag`, migration 0065, drawn by a
    trigger on every insert and for the rows there were). The hash was of guessable fields,
    and a device handed the cursor of a record it may not see (a keep-alive's `id:`,
    `hello.cursor`) could match it offline and learn which thread changed, how, when and by
    whom. A resume compares the cursor's tag with the stored one, so a restored copy is still
    found, and a tag says nothing about its record.
  - *Changed 2026-10-07 (the final review's F-10):* a tag of another form than 16 hex digits
    (the 8-digit checks the builds before W6-3 issued) is checked by its numbers, as an
    untagged cursor is, instead of being answered with a false "restored" resync.
- **A device's cursor moves through what it may not see** (W4-15): the keep-alive carries it
  as an `id:` with no data, so a reconnect after a stretch of Admin Chat activity resumes
  without a `resync`. A device's lag `state` says "some" events, not how many (W4-14).
- **The live view across a flip** (W4-8, W4-3). Attaching the toolset takes the thread from
  devices before the commit and again after it, taking it off gives it back after it, both
  decided from the store's before and after. The live registry keeps each flipped thread's
  latest flag, so a turn or session registered from an older read takes the right side, and
  every flip that moved a live entry sends each device a fresh `state` of its own.
- **A record that never renders** (review W5-16) stalled the stream at it for good. A
  stream now reads it again at its next wake or keep-alive up to 3 times in a row
  (`RENDER_ATTEMPTS`, named in the log line and the reason), then moves past it with a
  `resync` naming its cursor, so the client reloads; nothing is skipped silently.
- **A page loads the folders' purge days once** (review W5-15), not once per `thread.*`
  record.
- **The live buffer and the catch-up page have stated maxima** (W4-1): 65 536 events (the
  buffer is allocated whole, about 80 bytes a slot, at every resize) and 10 000 records. Both
  save paths refuse past them by name, the page shows them, and a stored value out of range
  is clamped on load with a warn line.
- **The route fails closed** (W4-9, W3-12): it takes the Chat API's caller extractor, and an
  anonymous principal is never the owner anywhere (`Caller::Refused` sees and reaches nothing).
- **Threads in a folder out of a device's reach** (W4-7, decided from the code): such a
  thread stays the device's when its own tools are plain, and is in no folder for it — by id,
  in the list, search (a hidden folder filters to nothing), exports and the feed. A folder's
  flip records `thread.updated` for each of its threads. *Corrected 2026-10-07 (review W5-5):*
  pin's and archive's answers named the hidden folder; they read the thread as the device
  does now.

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

*Prepared in WP4, 2026-10-07:* `store::feed::record_folder_current(tx, folder_id, thread_id,
previous, reason, by)` writes the record inside the caller's transaction, and the feed already
renders it (`FolderCurrent` in `lmgw-api-types::chat_feed`). The `current` route and the delete of
a current thread call it; nothing records it before the columns exist.

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

*Added 2026-10-08 (found in use on 0.4.1: a paired client's changes showed only
after leaving the Chat tab):* **the Chat page follows other writers live.** `/api/events`
carries a `chat` frame, `ChatChanged {threads, folders, messages, resync}`: ids only, so the
page reads them again through the Chat API.
- **Where it comes from.** `threads` and `folders` are the feed's own records (§2.3), read by
  each `/api/events` stream on the feed's wake from the head it opened at, for the owner (every
  thread; the admin-level records are skipped). `messages` has no record yet (`message.*` is
  WP10's), so every message write through the Chat's repository seam marks its thread in
  memory (`chat_feed::marks`); WP10's records replace the marks. A thread delete forgets its
  marks, since its `thread.deleted` record names it. `tests/it/chat_repo_seam_scan.rs` refuses
  a message write past the seam in any crate, SQL of a file's own that writes `chat_messages`
  outside the store, and a writer call in the seam outside `marked(`;
  `web/chat_repo/marks_tests.rs` checks that each of the seam's writes marks its thread.
- **Delivery.** The first change of a burst waits a fixed 200 ms (not a setting), then one read
  names everything changed up to it: at most one frame per 200 ms plus that read, except one
  the keep-alive read finds. One with `resync: true` on every connect, since frames are not
  replayed across a reconnect. A prune past what a stream had read is a `resync`.
- **The page** (`chat_sync`) reads the whole list on a frame that names a thread or a folder,
  or says resync. A frame that names only messages moved those threads' `updated_at` and
  `last_message_at` and nothing else the list shows: the page reads just their rows,
  `GET /chat/api/threads/rows?ids=3,7` (the list's own rows, active or archived; the owner's
  alone, `Cap::Admin`; not in the API document, like the rest of the dashboard's backend),
  and sorts its list again as the server does (pinned first, then `updated_at`, ties by the
  newer id; the archived list by `archived_at`). A named row missing from the answer, a failed
  read, a row that belongs in the list shown and is not in it (or the other way round), or a
  read of the whole list still out, reads the whole list instead. It reads the open thread when
  a frame names it, patching its messages in place by id (every field, cleared where the row
  has none).
  It waits while its own reply streams into that thread or a voice session holds the transcript
  (connecting, live or closing; not the Ended panel). Another client's reply shows when it is
  saved: the feed's live turns are not mirrored (§2.5).
- **What the page holds wins.** The composer and its drafts are never touched. An unedited
  settings form follows the stored row; an edited one follows in the fields the owner did not
  touch. The model picker follows only while it shows the stored model, and the owner's picks
  are saved one at a time in the order made, so the last one is stored. A read of the open
  thread that went out before the page's own turn, edit or delete and answers after it is
  dropped and made again. A reader at the end of the transcript before new rows land is kept
  there; one who scrolled up is left where they are.
- **A thread deleted elsewhere** (a 404 from `GET /chat/api/threads/{id}`, and only a 404; a
  failed read is a 500 and keeps what the page shows) falls through to the next one, as a
  delete here does. With a draft in the composer (text, files or knowledge picks) the page
  holds an unsaved new chat instead, in the deleted one's folder while the list has it: an
  empty transcript, the draft with its chips and picks, a strip ("New chat in “F” · made when
  you send") and a toast. Nothing is stored until the owner presses Send, which makes it as
  the page's own New chat in that folder does (an ongoing folder's "New conversation", with the
  server's ordinary rule; a plain folder's "New chat here"; a New chat once the folder is gone),
  opens it with the draft and sends. So an ongoing folder's current thread, which the other
  client may be talking in, moves only at the owner's Send (review CL-14). Opening another
  thread or leaving the page drops it as an unsent draft goes. A draft never goes into a
  conversation it was not written for. Its files went with the deleted conversation; their
  chips say so, and files attach once the chat is made.
  *Added 2026-10-08 (reviews CL-19 to CL-21):* from Send to the send itself another Send
  waits and the model picker is held, so the chat is made and opened once. The open reads the
  made chat: one another writer has written in meanwhile (after a failed open, say) keeps the
  draft in the composer. A folder that names a model makes the chat on it; a model the owner
  picked while the draft waited is then set on the chat, before the send. Opening another
  thread or a New chat drops the waiting chat as it starts: a Send under way then opens and
  sends nothing, and the chat it made stays, empty, as the New chat button leaves one. An
  older open that answers after a newer one started is dropped.
  *Changed 2026-10-08 (reviews CF-1 to CF-4, CF-9):* in an ongoing folder the chat Send makes
  is the folder's current thread, the conversation a device is bound to, and its model is the
  folder's. While the draft waits there, the picker is held on the folder's model. A badge
  and the strip say why: "this folder's ongoing conversation uses its folder's model; change
  it in the folder settings". Send writes no model. A plain folder, or none, keeps the
  owner's pick, and only a real pick: a picker the owner did not move off the model it started
  on writes nothing. A pick that another writer's turn came before is named in the toast. An
  open or a New chat that left the waiting chat and failed puts it back, when nothing else is
  on its way to show; until then Send says the conversation is still opening. What the owner
  chose last is what opens: the open after New chat's or Keep's POST yields to a row clicked
  meanwhile, and the fall-through after a delete yields to any open started or still out. A
  New chat that fails cancels no open. Retry after a failed list read leaves a waiting draft
  where it is.

### 3.7 As built (WP5)

*Changed during the build, 2026-10-07:*
- **The columns** (migration 0064) are §3.1's two plus the folder's own retention,
  `archive_days` and `purge_days` (§11 Q2). The folder JSON carries `ongoing:
  {idle_minutes, current_thread_id} | null`, `archive_days` and `purge_days` (`null`: the
  global setting). `POST /chat/api/folders` takes `ongoing`, `archive_days` and `purge_days`
  too, so a client names and marks its folder in one call; a patch takes them, and `null`
  ends ongoing or goes back to the global retention. Ongoing needs the folder's model on
  create, on a patch that marks it, and on a patch that would drop the model of a folder that
  stays ongoing: a 400 naming `defaults.model_alias`.
- **A current thread always is one** (code over spec, §3.3 rule 1). A delete, a move out of
  the folder and an archive by hand end it in their own transaction and record
  `folder.current` with `thread_id: null` and reason `gone`; §3.3 had only the delete record
  it and found a moved or archived thread on the next call. So `ongoing.current_thread_id`
  never names a thread that is elsewhere or archived, and clients learn of the end at once.
  The column's `ON DELETE SET NULL` is the backstop. A folder that stops being ongoing loses
  its current thread with reason `not_ongoing`. A pin, and a move within the folder, keep it.
  *Stated 2026-10-07 (review of WP5, decision 1):* an archive followed by a restore does not
  make the thread current again (the next call starts a new one, `first`), and a device's
  archive ends the conversation for everyone, within L2.
- **The reasons, as built.** A new thread: `first` (the folder has no current thread, also
  after an end the feed already announced as `gone`), `gone` (the current thread is out of
  the caller's reach: for a device, the self-admin toolset attached to it since, L3),
  `idle`, `requested` (`new: true`, or a chat thread created in the folder by hand). None any
  more: `gone`, `not_ongoing`. `current`'s answer also has `note`, the reason in a sentence
  that names the setting behind it (for `idle`, the folder's idle minutes).
- **L3 for `current`.** A folder a device cannot see (defaults with the self-admin toolset)
  is the 404 of a missing one. A current thread out of a device's reach is `gone` for it: the
  device gets a new chat thread from the folder's defaults, which cannot carry the toolset,
  and the conversation moves on for everyone. A device's folder JSON does not name a current
  thread out of its reach.
  - *Changed 2026-10-07 (review W5-3, decided by the owner):* **a device is never told why.**
    `gone` could only mean the attach (a delete, a move out and an archive clear the pointer,
    so a later call says `first`), the same leak as W4-18's close. A device's `current` now
    answers `first`, with the note any folder without a current thread gets, and its feed's
    `folder.current` names no previous thread out of its reach and says `first` instead of
    `gone`; a move to none about such a thread is not rendered for it at all. The record
    keeps `gone`, and the owner reads it. *Corrected 2026-10-07 (review W6-15):* every reason
    is `first` for a device when the previous thread is out of its reach, not only `gone` (an
    owner's `idle` or `requested` rollover away from such a thread said a thread was there).
    What remains: after an attach a device's feed
    carries the thread's `thread.deleted` and no `folder.current` with `thread_id: null`,
    unlike a real delete; the next `current` call moves the conversation on either way. *Stated 2026-10-07 (review W6-4):* the `state` a
    device gets beside the thread's `thread.deleted` now says only "this is the live state now",
    no cause; that a `state` with no lag comes beside a `thread.deleted` at all (a real delete
    sends `turn.done` and no `state`) remains. *Resolved 2026-10-07 (§1.6's close-code note):*
    a real delete with a turn or a session live now sends a device that `state` and no
    `turn.done`, as an attach does. The missing `folder.current` with `thread_id: null` after an
    attach still remains.
  - *The dashboard says it* (decision 2): attaching the self-admin toolset to an ongoing
    folder's current thread shows, before Save, that paired devices can no longer reach it and
    that the conversation continues in a new thread for every client the next time one asks.
    The owner's thread stays in the folder, which a device still sees: that is why a folder's
    own retention is the owner's alone (L5's W5-1 note).
- **Idleness and a running turn** (review W5-11). A thread a turn answers now (a send, an
  edit, a regenerate, a continue, a bound session's voice turn) is never idle: another
  client's `current` keeps it however old its newest message is, so a long tool loop's reply
  does not land in a thread the conversation has left.
- **The body is optional** (review W5-10): `POST …/current` with none is `new: false`.
- **`folder.current` and L3** (review W4-6). The record is `admin` when the folder or the
  new thread drives the self-admin plane at the write. A device's delivery re-checks both as
  they are now, as `thread.*` and `folder.*` do, and does not name a previous thread out of
  its reach (`previous_admin` in `detail`, or the thread as it is now).
- **The lock.** One per folder (`FolderLocks`), held from read to write by `current`, a thread
  created in the folder, the folder's patch (so a rollover never starts from defaults
  between the patch's folder write and its current-thread write) and the folder's delete.
  *Corrected 2026-10-07 (review W5-19):* both of the patch's writes share one transaction,
  so that rationale is moot; the lock keeps the folder as read (its defaults, its current
  thread) from changing under the patch, and the current thread's own lock (W5-2) keeps the
  thread as read. `current` releases the folder's lock once its decision is written, before
  the answer is built (review W5-9), so another client's call does not wait on the voice
  resolution of this one's.
  The rollover's write is a compare-and-set on the pointer: a delete, move or archive that
  ended the current thread meanwhile (they take no folder lock) makes it decide again on
  what is there now.
- **L9, as built.** The changed fields are those whose value a new thread in the folder would
  start with differs between the old defaults and the new: a field set back to unset takes
  what a new thread starts with without it (the default prompt, the route's default), and a
  field the defaults did not change keeps the thread's own, a hand-made change included. The
  voice is compared field by field and the thread keeps its seed. The changes pass the thread
  settings route's own checks (one step, `apply_settings_patch`), are written in the folder
  patch's transaction, and the answer names them: `applied: {thread_id, fields}` (`voice.<field>`
  for the voice), or `null`. A device's patch never touches a current thread out of its
  reach. The live L3 flip follows the store's before and after, not a read made earlier
  (review W4-3).
  - *Corrected 2026-10-07 (review W5-2):* "never touches" held only at the time of the read.
    The settings are written whole, from a copy read earlier, so a device's write that read
    before the owner attached the self-admin toolset and landed after it undid the attach
    (the thread settings route and a folder patch's current thread alike). Both now take the
    thread's lock before they read it (folder, then thread, then the database), and the store
    refuses a device's settings write to a thread that drives the self-admin plane as the
    write's own transaction finds it: the route's 404, or `applied: null` in a folder patch.
    The folder patch also writes the thread only while it is still the folder's current
    thread (W5-12). A write that fails after the pre-commit flip gives the thread back its
    real flag (W5-7).
  - *Changed 2026-10-07 (reviews W6-6, W6-13, W6-14):* **reach is checked twice, the lock held
    from the second check to the write.** A device's move, pin, archive and delete check the
    thread's reach, then again under the thread's lock, which they hold across the write
    (`chat::reach_held`; a delete discards under it, `LiveTurns::discard_held`), so none lands
    on a thread the owner attached meanwhile; move answers with the thread as the caller reads
    it, and a thread not there for the caller any more is a 404, not a 500. The settings route
    and the folder patch's current thread run their patch and a device's checks first, without
    the lock (L5's listing may take the lazy-list budget, and the thread's turn saves would
    wait on it), then read again under the lock and lay the patch again only when the thread
    changed. An id out of reach is the 404 before any lock, so the answer's timing says
    nothing about that thread. The flips after a write (the post-commit flip, W5-7's
    restore) run before the lock is let go.
  - *Changed 2026-10-07 (review W6-10):* **a folder save names only what it changed.** The
    patch takes `defaults_patch` beside `defaults`: each field given replaces the stored one
    (`null` unsets it), `voice` field by field, the others stay as stored. The dashboard's form
    sends the defaults fields it changed there, and the name only when changed (the ongoing
    and retention fields already were, W5-17), so it never writes back, and L9 never
    re-applies, what a device changed while the form was open.
- **Retention (§11 Q2).** Any folder may carry `archive_days` and `purge_days`, not only an
  ongoing one, and only the owner sets them (L5's W5-1 note): both halves of the sweep take
  the folder's days, else the global setting, `0` disables the step, and every thread's `purge_at` uses the same days. There is no upper
  bound: a count past what the clock can reach means "never". *Reworded 2026-10-07 (review
  W6-7): the owner's setting, from the dashboard; a device's write of it is a 403.* A year of history for a
  desktop client's folder is `purge_days: 365` (or `archive_days: 0`, never archived).
- **The dashboard.** The folder form has *Ongoing conversation* (its idle minutes, 0 "only
  when asked"), the model marked required while it is ticked, *Retention* (empty names the
  global value), and *Also apply the changes to the current thread* in the footer, checked.
  The sidebar marks the current thread `current`. For an ongoing folder the menu's *New
  conversation* replaces *New chat here*: a thread made by hand there becomes the current
  thread anyway, without reusing an empty one.
  - *Added 2026-10-07 (review W6-11):* the folder form says before Save that the self-admin
    toolset gained in an ongoing folder's defaults takes the folder and its conversation out
    of every paired device's reach (and the current thread too, with the change applied).
  - *Added 2026-10-07 (the first client's review):* **a deep link to a folder's settings
    form**, `/chat?folder=<id>&settings=1`: the dashboard opens that folder's form once the
    list is loaded, a folder not in the list said in a toast. A client sends the owner there
    to mark its folder ongoing, give it a model or its own retention — what a device key
    cannot set, and `current`'s `folder_no_model` remedy. `lmgw-client`:
    `requests::folder_settings_page(id)`.
- **The newest message's time** (the first client's review, 2026-10-07). A thread row carries
  `last_message_at` (unix seconds, `null` without a message): in the thread list, the feed's
  `thread.*` and `current`'s thread. An ongoing folder's idleness is measured from it, and a
  client timed the rollover from the turns it saw, so a missed message delayed it by up to an
  idle period. `lmgw-client`: `requests::idle_rollover_at(folder, thread)`.

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

*Built in WP6, 2026-10-07:*
- **The modules.** `realtime` (`ServerEvent`, parsed by `realtime::parse`; `ClientEvent` with
  `to_json`; `SessionFacts`, `ErrorFacts`, `ThreadFacts`, `ChatReply`, `TurnDetection`,
  `Updates`, the close codes), `voice` (`Machine`, `VoiceState`, `INTERRUPTED_MS`,
  `cancel_after_truncate`), `truncate` (`Replies`, `Reply`, `Incoming`, `PlaybackCursor`),
  `feed` (`SseDecoder`, `FeedReader`, `FeedItem`), `requests` (`Request` and the builders of
  `folders`, `create_folder`, `current`, `threads`, `feed` and `realtime`, with `read_*` for
  their answers and `read_refusal`) and `base64`. The wire types are `lmgw-api-types`',
  re-exported as `lmgw_client::types`.
- **Code over spec: what stays in the dashboard.** The captions line (`Captions`) stays in
  `machine.rs`: its hints name the dashboard's keys ("Hold Space to talk"). So does the player:
  the crate's `Replies` keeps each reply's assistant item, and the dashboard keeps the player's
  item beside it.
- **`Machine::quiet()`** joins the machine from the desktop client's interim copy: nothing open,
  waited for or playing, and the user silent.
- **A few payloads stay `serde_json::Value`** (a chat frame's data, a message's voice record, the
  timing line): a client passes them on whole. An FFI wrapper hands them over as JSON text.
- **Shared with the gateway**, so the two cannot drift: the realtime close codes (4000, 4003), a
  voice stage's `ModelState`, and the feed's `by` names (`BY_ADMIN`, `by_device`,
  `FeedPrincipal::by`) are `lmgw-api-types`', and the gateway uses them.
  *Corrected 2026-10-07 (review W6-9):* not `ModelState`: the gateway writes its frames from
  its own struct (`realtime::warm::outcome::ModelState`, whose enums are its own). A test reads
  every frame it writes into the API type field for field, and the API type gained the two
  fields clients dropped (`needed_bytes`, `capacity_bytes`). The 1001 close and its reason
  joined the shared constants.
- **No behaviour change in the panel.** The client events are byte for byte the panel's old
  frames (the crate's tests), and the voice drives that need no model pass on a dev instance
  (`scripts/chat-drives.sh --voice`: chat-voice-panel, chat-voice-reasoning, chat-voice-viz,
  with chat-voice-render).
- **The gate** checks the crate for `wasm32-unknown-unknown` on its own (`ci/check.sh`; in
  `--changed` when `lmgw-client` or `lmgw-api-types` changed); natively the workspace build,
  clippy and the suite cover it.

*Changed after the WP6 review, 2026-10-07 (decided by the owner):*
- **Forward compatible (W6-2).** Every enum a client reads from the wire has an `Unknown`
  fallback and is `#[non_exhaustive]`: `KbMode`, `TurnDetectionMode` (renamed from the
  api-types `TurnDetection`, which clashed with the realtime one, W6-18) and `AudioInputMode`
  keep an unknown value as sent and write it back unchanged (`#[serde(untagged)]`);
  `CurrentReason::Unknown`; `FeedEvent::Unknown {event, data}` (was `Other`) and
  `ServerEvent::Unknown {kind, data}` (was `Other(type)`, now the event kept whole). An unknown
  event or one whose data does not read never stops a client: `FeedReader` yields it as
  `FeedItem::Unknown {event, data}` (was `Unreadable`), the cursor moving past it knowingly
  (*changed 2026-10-07, the final review's F-13:* a known event whose data does not read is
  `FeedItem::Unreadable {event, data}` again, apart from an unknown type: the client reloads what
  it shows, as for `resync`, while it skips and logs an unknown one). The
  document lists the known values only. `VoiceState` stays exhaustive: the crate derives it,
  no wire carries it, and a new state comes with a new crate a client compiles against.
- **Refusals (W6-5).** `read_refusal` reads the Chat's flat `{code, message}` and the realtime
  handshake's `{"error": {code, message, …}}`. `key_refused` says when the key itself opens
  nothing (`device_key_unknown`, `device_disabled`, `key_expired`), `code::CHAT_THREAD_BOUND`
  the bind's refusal.
- **Smaller (W6-16 to W6-18).** `ws_url` matches the scheme in any case and refuses anything but
  http(s) (`bad_base_url`); the module doc names the folder settings page as the remedy for
  `folder_no_model` (no folder patch builder: the folder's model and retention are the owner's
  form); `Replies` keeps an ordered list (no hasher seed) and lists its items
  (`Replies::items`, the first client's review). A few getters still lend (`&str`, `&Reply`):
  they run per audio frame, and an FFI object keeps the state behind its own lock and clones
  what it hands over, so owned returns would cost the native client and buy the wrapper
  nothing.
- **A later `lmgw.chat.reply` replaces the earlier** for the same message: the journal
  finalizes a barge-in's reply at the gateway's cut and re-cuts it when the client's truncate
  comes after (chat-voice §8.3's late truncate), each with its event. Said in the realtime
  DocRoute and on `ServerEvent::ChatReply`.

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
    *WP4, 2026-10-07:* the feed's own shapes are in `lmgw-api-types::chat_feed` already: the
    cursor, `Hello`, `LiveState`, each live event, the tombstones and `FolderCurrent`. A
    `thread.*` or `folder.*` event's data is the thread or folder row plus `by`, typed once
    `ThreadRow` and `Folder` are. *Typed in WP6* (below).
  - **Android:** `Message`, `Attachment`, `SendRequest` and the send stream's frames.
- **Documentation.** Each route gets a `DocRoute` under a "Chat" tag, and the API docs page lists
  them. The Chat block leaves `openapi/exclusions.rs`.
- **This reverses, for the subset, the owner's 2026-09-28 decision** that the dashboard's backend
  is not a contract (`exclusions.rs:9-14`). The owner's 2026-10-06 decision D is the reason.
- **Compatibility.** The routes stay lmgw-native: not OpenAI Conversations, no `/v1` prefix.
  Changes are additive within a release line; a breaking change is a new route or field.

*Built in WP6, 2026-10-07:*
- **The types** are `lmgw-api-types::chat`: `ThreadRow`, `Thread` (the row with
  `voice_resolved` and, where the thread is read whole, `continue`), `Folder`, `ThreadList`,
  `FolderList`, `FolderCreate`, and their parts (`ThreadVoice`, `ThreadDefaults`, `ThreadMcp`,
  `ContinueState`, the enums). `Current` is WP5's `chat_folders::CurrentThread`, its `thread`
  now a `Thread`. `ApiError` was typed already: the documented routes' error. The feed's
  `thread.*` and `folder.*` data are `chat_feed::ThreadChanged` / `FolderChanged` (the row with
  `by`) or the tombstone, and `chat_feed::FeedEvent` reads any event by its name.
- **The handlers serialize them** through `web/chat_wire.rs`, which converts the store's rows
  field by field, destructured without `..`: a new column does not compile until it is placed.
- **Byte-compatible.** The `json!` builders wrote their bodies through a `serde_json::Value`,
  whose map sorts its keys; the DTOs are written the same way (`chat_wire::wire`, and
  `chat::wire_order` for `current`'s `thread`), so every answer is byte for byte what it was.
  The tests compared the DTOs with the old builders on representative rows before the builders
  went, and pin the bytes.
- **Documented:** `GET /chat/api/threads`, `GET` and `POST /chat/api/folders`,
  `POST /chat/api/folders/{id}/current` and the feed, under the "Chat" tag; they left
  `exclusions.rs`. A test keeps the feed's documented events equal to the ones `FeedEvent`
  reads.
- *Changed 2026-10-07 (review W6-8):* the `thread.*` and `folder.*` events' `oneOf` could match
  no instance (both shapes allow any field and require none); the tombstone branch now requires
  `deleted: true` and the row branch has no `deleted`, and a test validates both shapes of both
  events against the document. *W6-7:* the folder create's DocRoute and the folder fields say
  that a device key cannot set the retention.
- *Changed during the build, 2026-10-07:* **`voice_resolved` is documented as an object**, its
  parts named in prose, and typed with Android's DTOs (WP10). Its nested shapes (the stages, the
  hold fallbacks, the recursive audio-input verdict) are the dashboard's voice editor's, and no
  Desktop 1 client reads them. **`GET /chat/api/threads/{id}` stays excluded** until then for the
  same reason: its `messages` are Android's `Message` and `Attachment`.
- *Added 2026-10-08 (review CL-11):* **`GET /chat/api/threads/rows?ids=`** is the dashboard's,
  `Cap::Admin`, and excluded as the dashboard's backend (§3.6): the Chat page's re-read of the
  rows a `chat` frame named. A device's feed carries whole rows already.

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

*For WP7 (review W3-9), 2026-10-07:* L5's write check has a shortcut, `!scope.narrows()` →
admitted, for a tool scope with no list of its own (`all`). Once device-hosted labels exist that
shortcut would let an `all` device write another device's hosted label into a folder default,
which the owner's turns then call. WP7 must make `narrows()` true for an `All` that carries
exceptions, or replace the shortcut with `may_reach` plus `admits` on the namespace; §9's reach
tests gain "an `all` device writes another device's label → 403".

*As built (WP7), 2026-10-09:* `mcp/host.rs` and its children (`transport`, `link`, `links`,
`conn`, `calls`, `caller`), `mcp/scope/hosted.rs`, migration 0072. Where this section was silent:
- **The row** is named after the device key (`device:<name>`), as an agent's is `agent:<id>`; a
  server name with that prefix is refused for every other writer. The MCP page may change its
  `enabled` flag and `timeout_ms` only; a create or a delete by hand is refused, naming the
  grant. The grant writes the row in the key write's own transaction.
- **A row switched off** refuses the upgrade (`403 host_not_granted`, saying so) and closes an
  open link with 1000; so does a cleared grant. The handshake (`initialize` and the paged list)
  is bounded by the row's `timeout_ms`; a failure leaves the row `Error` with the message and
  closes the link 1002. A binary frame, a batch or text that is no JSON-RPC message closes 1002
  too; a missed pong 1011, an overrun 1009 (naming the setting), a stop 1001.
- **`_meta`:** `lmgw/approval` is `null` until F (WP9) decides calls; the gateway's own runs are
  `{kind: "gateway", name: "lmgw"}`; a request with no key would be `anonymous` (L16 keeps it
  from ever reaching a device row, so it is never sent; a device reads it as the least trusted).
  A reader takes each part as optional: a missing timeout is no deadline.
- **A turn whose every failed label is an offline device** runs as a plain chat turn (the plain
  path, not the tool loop), after the `error` frame naming each label.
- **Settings:** `mcp.host_*` are the dashboard's settings save (Settings → MCP), not
  `lmgw__settings_set`'s; a change applies to links opened after it.

*Changed after the WP7 review, 2026-10-09:*
- **A run calls only the tools it offered.** Every in-process run's MCP executor (a Chat turn, an
  agent run, `/v1/responses`, realtime) takes the names its labels listed, with the server each
  came from, and calls each there only (`call_listed`); any other name is a tool error naming
  why, never routed by the aggregate. So a `/v1/responses` continuation that resumes approved
  calls must carry its `mcp` blocks again, as OpenAI's API has it.
- **A name is a device's by the server that serves it** (the aggregate's routes), not by its
  spelling: a bare server's own `desktop__x`, or a server prefixed `desktop_`, is not the
  device's while it serves the name. A name nothing serves now is held back by the device
  namespace it falls in (the longest label's), failing closed. A `/mcp` call is held to the
  server its name routed to when the scope admitted it. A hosting label ending in `_`, and a
  label and a tool prefix (or two labels) whose namespaces run into each other, are refused.
- **W3-9, as built:** `narrows_for(server)` — the list narrows, or `server` is a device row the
  caller does not reach whole. An `all` scope keeps its other servers (a bare one not connected
  included) listed and writable without connecting anything.
- **Closes:** a key deleted or disabled closes its link 4003 with the revocation's reason, also
  when the configuration reload that drops its row comes first. Of two links of one device
  opening at once, the one with the lower link number is told to close (4000) and the other
  serves. A call cut off by a close lmgw made reports that close's reason, not "disconnected".
  Every write on the link is bounded: by `mcp.host_ping_interval_s`, or with pings off (0) by
  the row's `timeout_ms`, so a device that stops reading cannot hold a close or a revocation.
- **`initialize` carries the link's limits** in `params._meta["lmgw/host_limits"]`
  (`lmgw_api_types::mcp_host::HostLimits`, re-exported by `lmgw-client`):
  `{max_message_bytes, max_frame_bytes, ping_interval_s, call_timeout_ms}` — the sizes set on
  the upgrade (`max_message_bytes` `null` when `mcp.host_max_message_mb` is 0, no bound; the
  frame is always bounded), the ping interval the link runs with (0 = no pings) and the row's
  `timeout_ms`. Read once per link; a setting changed later applies to the next link.
- **An older server row named `device:<name>`** (from before the prefix was the grant's) keeps
  0072 from creating that device's row. lmgw renames nothing: the start that runs 0072 logs a
  warning naming both, the upgrade's `403 host_not_granted` names the row, and a key write that
  sets or changes the grant is refused naming it (other writes of the key go through) until
  the owner renames or deletes that row.

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
   gated call (`call_id` added 2026-10-09, §6.6), then `done {pending_approvals: […]}`. `arguments` is a JSON string, as OpenAI's,
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
  arguments}` (and `call_id`, added 2026-10-09, §6.6).
- **`approval.decided`:** the same ids plus `{approve, by}`.

Both are stored records rendered from the thread's pending state.

### 6.6 As built (WP9), 2026-10-09

`store/chat_approvals.rs`, `web/chat_approvals.rs`, `web/agentchat/gated.rs`,
`web/chat_turn/resume.rs`, `web/chat_temp/approvals.rs`, `web/chat_feed/approvals.rs`,
`agent/approval.rs`, `realtime/thread/approvals.rs`, migration 0074;
`lmgw_api_types::chat_approvals`, re-exported by `lmgw-client` (`requests::approvals`). Where §6
was silent:
- **The stamping rule.** `_meta["lmgw/approval"]` is `{decision: "approved", by}` exactly for a
  gated call a verdict approved, run by the turn that verdict resumed, and `by` is the principal
  that decided (`{kind, name}`, as `lmgw/caller`). The loop runs each such call inside a
  task-local scope (`agent::approval`); the MCP executor and the call's request row read it there,
  so no wrapper between them can drop it. A sibling that waited beside a gated call, a declined
  call (never forwarded), and every call outside a resume carry `null`. A `/v1/responses`
  continuation's verdicts are decisions too, by the request's principal. It composes with MCP
  Tasks: the call id is set as for any loop call, so an approved `required` call carries both
  `lmgw/approval` and `lmgw/task`. Every other call the loop makes runs inside the scope with
  nobody as approver, so a run started inside an approved call's scope cannot inherit it.
- **Self-approval is the device's call.** lmgw stamps every real decision, whoever made it: `by`
  is always the principal that decided (the route's and a bound session's caller, a
  `/v1/responses` continuation's request principal), also when that is the call's own caller. A
  device decides whether a self-approval counts: Kai treats `approval.by == lmgw/caller` as not
  approved unless the caller is the owner, the gateway or its own key.
- **The decider's reach** (taken in this draft, open to the owner's veto). The resumed turn runs
  as its starter, whose reach may be wider than the decider's, so an approval would be a
  device's path to calls it may not make itself. When the decider is not the owner, every
  approving verdict is refused whose call its own tool scope (`ToolScope::admits`) does not
  admit, and every approving verdict on an `lmgw__*` call unless its own admin tools may do
  everything (its level `full`, capped by the gateway's): `403 approval_out_of_scope`, naming
  each call, and nothing of the batch is decided. Declining stays open to any client that sees
  the thread. The owner approves anything the thread offers.
- **Stored state.** `chat_messages.pending_approvals` (JSON): the starter's key id and name, the
  calls of the turn's last stop (gated and siblings, in the model's order) and every decision
  made on the reply (call, verdict, reason, who, whether by moving on). `request_logs.approved_by`
  is `<kind>:<name>` (`owner:dashboard`, `device:phone`). An edited reply loses it with its tool
  record; a deleted or regenerated one takes it along.
- **Ids.** A gated call's id is `mcpr_` and 64 random bits, unique in the gateway whatever ids the
  model gave its calls. A bound session's `mcp_approval_request` item has it as its `id`, which
  its `mcp_approval_response` quotes, as OpenAI's.
- **The call's id** (added 2026-10-09, for the desktop client's app views, which match an
  approval to the call whose view they show). Every place an approval is named carries the
  model's id for the call, `call_id`, the id the turn's `ready` and `result` frames carry
  (§7.6): the `tool {event: "approval"}` frame, `done.pending_approvals` and a message's
  `pending_approvals` (`ApprovalRequest::call_id`), and the feed's `approval.requested` and
  `approval.decided`. Only added: every other field stays, and a reader of an earlier lmgw finds
  none (`Option`). A bound session's `mcp_approval_request` item stays OpenAI's shape, which has
  no call id; the `lmgw.chat.frame` relaying the turn's `approval` frame, sent right after the
  item, carries `call_id` beside the same `approval_request_id` (lmgw-only data stays in
  `lmgw.*`). `lmgw-client`'s `ServerEvent::ApprovalRequest`, read off the item, has `call_id:
  None` for that reason.
- **The route's answers.** `400 bad_request` for no verdicts, `400 approval_missing` naming each
  waiting call without one, `404 approval_not_found`, `409 approval_decided` naming who decided,
  `409 approval_starter_unavailable` naming the key (checked before anything is decided), `409
  approval_moved_on` when the reply is no longer the thread's last message (checked inside the
  decision's own write, so nothing is decided then), `403 approval_out_of_scope` (above). The
  starter's concurrency slot is taken before the decision too: a key at its limit is its `429
  key_rate`, nothing decided, and the resumed turn holds the slot for its length. `speak: true` reads
  the resumed reply aloud as a send's does; the stream and the read-aloud belong to the approver's
  request, the turn (its model calls, slot, tool scope, rows) to the starter.
- **The resumed turn** always runs the tool loop (also when the thread's tools went since: their
  calls then answer that the tool was not offered), and is appended to the gated reply: text,
  record and pending state, the decisions kept, so a second gated stop gets new requests on the
  same reply.
- **Decided, not saved.** A decision is written before its turn runs. When that turn is
  superseded or fails before it saves, the next message closes the calls: an approved one (and
  a sibling decided with it) with "decided to run, but the turn that ran it ended before its
  result was saved, so it may or may not have run".
- **Decided, never started.** When the turn cannot start after the decision was written (its
  starter's key went in between, a send made the reply no longer the last message, the turn
  was refused or stopped before its tool loop), its calls are closed at once (`close_unrun`):
  an approved call and its siblings read "not run: the turn that was to run it once it was
  decided could not start" (a result the next message had already closed with the "may or may
  not have run" words is reworded), a declined one keeps its words. Each decision is marked
  `not_run`, and an approved one gets a second `approval.decided` record, rendered (as the first
  is from then on) with `approve: false, not_run: true`: the feed never says approved for a
  call that never ran.
- **A new message** from any writer (a send, a bound session's turn, a temporary thread's)
  declines in its own insert; a sibling reads "not run: it waited beside a call that needed an
  approval, and the user moved on without deciding"; `approval.decided` names the writer.
- **Feed.** Records carry `{message_id, approval_request_id}` in `detail`, at the thread's level
  (L3); the rest is rendered from the reply at delivery, and a reply gone by then renders
  nothing. `approval.decided` carries `{thread_id, message_id, approval_request_id, approve,
  by}`, `by` the feed's author name, as `lmgw.approval.decided`'s.
- **The thread's read** lists a reply's waiting calls as its `pending_approvals`.
- **A bound session's answer** runs without the journal: it writes no user turn, and its reply is
  the gated one, saved already, so no heard cut or voice timing is recorded for it. A committed
  turn with words takes precedence (it is a new message, which declines what waits) and the
  queued answers go. The answers are checked when that response runs: a refusal fails it with the
  route's code. Every decision and every new message wakes the thread's bound sessions by thread
  id, and so does every edit, delete, resend or cut-back of the history; a session that lagged
  reads its own thread again. A session does not show the calls that waited before it bound: a
  client reads them from the thread or the feed.
- **A call gone from under a bound session** (taken in this draft, open to the owner's veto):
  when the reply that holds a call the session showed is edited, deleted or cut away, the
  session says `lmgw.approval.decided` with `approve: false` and `by: null`, and stops waiting
  for it: nobody decided it, and it never runs. A decided call whose turn never started is said
  with `approve: false` too.
- **A bound session's resume holds its starter's slot** (taken in this draft, open to the
  owner's veto): as the route's, it takes the starter key's concurrency slot before deciding,
  unless the session holds that key's own already (the session's slot holds its place, realtime
  §10.3); a refusal fails the response with `key_rate`.
- **An unbound session** refuses `mcp_approval_response` (`invalid_value`, saying approvals are
  taken on a session bound to a chat thread) and `mcp_approval_request`, as before.
- **Temporary threads** take approvals too, in memory, and record nothing in the feed.
- **`POST …/transcribe`** is documented now (`Dictation`), with `requests::transcribe`.
- **Dashboard (built after WP9):** a waiting call's card shows its arguments with Approve and
  Decline under it (with several waiting, each card picks a verdict and a bar sends them, or
  approves or declines all), the resumed reply streams into the same message, and the route's
  refusals read inline on the card. A call that ended without running reads as declined, not
  decided (a new message came), not run, or possibly not run, from the words of its result. The
  thread drawer's and the folder defaults' tool picker edit `require_approval` per source: never,
  always, or a per-tool list (`{always: {tool_names}}`); a value those cannot show (a `never` list)
  is kept as read until a mode is picked. `scripts/chat-approvals-drive.sh` drives all of it.
- **Devices only tighten `require_approval`** (decided by the owner 2026-10-09; built in
  `web/chat_tool_write/approval.rs`). A device, or any non-owner, that writes a thread's
  settings, a folder's defaults or `apply_to_current` (which runs the thread's settings
  checks) may not make a server require approval for fewer calls than before. A loosening is
  `403 approval_loosen_refused`, naming the label and the tool, and nothing is written;
  tightening passes, a server whose stored entries are all carried unchanged (none added)
  passes, and the owner may do anything. Labels are stored trimmed (`" kb"` is `kb`), from a
  request and from a stored row alike.
  - *Scope.* The floor protects Chat threads — stored threads and the realtime sessions bound
    to them. Per-request tools on `/v1/responses` and unbound realtime sessions are bounded
    only by the caller's tool scope, not by any thread's floor.
  - *Servers, not labels.* Entries are compared by what their label names as a turn resolves
    it (`mcp::exec::label_target`): a built-in toolset, or a registered server by its tool
    prefix or its name; a label that names nothing is compared with the same label only. Two
    entries of one write that name one server (`gh` and `github`, or one label twice) are `400
    bad_request` for every writer, the owner too, where the write changed one of them: which
    rule holds is not what the list shows. A pair stored before this rule and sent back
    unchanged passes; dropping the stricter of the two is a loosening. For a device the `400`
    comes after the scope check, so a label out of its reach is answered as such and never
    says a server exists.
  - *Either spelling.* A tool name in a `tool_names` list is compared in the tool's own
    spelling (`<prefix>__` taken off once, `<label>__` for a built-in), as a turn matches
    either: `{always: [search]}` to `{always: [search], never: [gh__search]}` ungates
    `search` and is refused; `{always: [gh__a]}` to `{always: [a]}` is the same rule.
  - *The comparison.* Per baseline, the old and new rules are compared tool by tool over every
    tool either names and over "the server's other tools": `always` to `never`, a tool dropped
    from an `always` list or added to a `never` list, a dropped field, a filter that stops
    gating the other tools (`{never: [x]}` to one whose lists are both empty, which gates
    nothing).
  - *The owner's floor* (migration 0076: `approval_floor` on every thread and folder). A
    device's entry is compared with the stored rule for its server **and** with the owner's
    floor for it, the strictest of both counting. Only the owner's writes move the floor: a
    server the owner writes takes the owner's rule (a looser one too), one the owner removes
    has none, one the write does not touch keeps what it had. A device removing an entry
    leaves its floor, so an entry added back is no looser than the owner's last rule. Rows
    stored before 0076 start with their stored rules as the floor (they were written before
    this rule existed). A thread made in a folder starts from the folder's floor, whoever
    makes it (a rollover too); a device's new folder has none.
  - *Folders.* For a thread in a folder, a server the thread has neither a floor nor an entry
    for is compared with the folder's floor and defaults for it, so a new entry is never
    looser than the folder's default (a refusal, not a silent rewrite). A thread leaving its
    folder — a move out or into another folder, or the folder's delete keeping its threads —
    takes those folder rules into its own floor for the servers its floor does not name
    (`store::chat_approval_floor`), so moving never lowers it.
  - *At the turn.* A write checks labels as they resolve then; a label that named nothing at
    the write (a server deleted, a device's grant cleared) or that an owner's rename or
    re-prefix retargets later resolves at a turn. So a turn applies the floor again
    (`mcp::exec::ApprovalFloor`): per server it resolves, the effective rule is the strictest
    of every thread entry naming it and every entry of the thread's floor naming it — or,
    where that floor names it nowhere and the thread is in a folder, of the folder's floor and
    defaults (the same baselines a write is compared with, so an owner who loosened a
    thread's entry against its folder's default keeps that). Names compare in either
    spelling.
  - *Tool mode.* The `kb` toolset knowledge-base tool mode attaches runs under the same
    effective rule for `kb`: tool mode never ungates the owner's `kb: always`, also when a
    device drops the entry in the write that turns tool mode on.
  - *Shapes.* `require_approval` is `"never"`, `"always"` or `{always: {tool_names},
    never: {tool_names}}` (either key may be left out, `{}` is `"never"`); any other shape — an
    unknown key, a filter that is not an object (`{always: ["a"]}`), one without `tool_names`
    (`{always: {}}`) — is `400 bad_request` for every writer, and refused on `/v1/responses`
    and `/v1/realtime` too, never read as "gates nothing". Checked on the entries a write
    changes: a stored value sent back unchanged (a `defaults_patch` of another field) passes.
    A stored value that does not parse gates every tool.
  - *What stays robust.* Only `"always"` and a never-only filter (`{never: [...]}`: every tool
    but those named) gate a tool the server adds or renames later; an `always` list gates the
    names it lists. A device hosting its own server controls its tool names, so on such a
    server only those two forms hold against it; and on such a server, a tool whose own name
    starts with `<prefix>__` can make the either-spelling equivalence map a name to the wrong
    tool. And a device whose admin tools are `full` is the owner's equal on configuration:
    changing a server's tool prefix with `lmgw__mcp_server_set` retargets every stored rule
    that names the server by it.

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
  results. A server without a prefix keeps its URIs, and the first by name wins (tool names
  no longer work that way: §7.6). The rewrite of results is the one departure from
  server-tools decision 7.
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

*Re-read for WP8, 2026-10-09:* `specification/2026-01-26/apps.mdx` (SEP-1865) is still the newest
stable revision (the repository holds it and `draft`). The capability key, the MIME type and the
`_meta.ui.*` keys are as §"What exists today" lists them; the flat `_meta["ui/resourceUri"]` is
deprecated (removed before GA) and still read. One point moved against §7.2: the revision defines
`io.modelcontextprotocol/ui` as the **host's (client's)** capability and says servers SHOULD check
it before registering UI tools; it defines no server-side declaration. So lmgw advertises it in
its own `initialize` to every southbound server (and device link), without which an SDK-built
server offers no UI tools at all; `/mcp`'s server capabilities keep it as §7.2 says, which
SEP-1724 permits on either side. A server MAY leave a UI resource out of `resources/list` (hosts
find it through tool metadata), which shaped the routing below. Pinned in
`mcp/resources.rs`'s module doc and `lmgw_api_types::mcp_apps::REVISION`.

### 7.5 As built (WP8), 2026-10-09

`mcp/resources.rs` with `uri` (the rewrite, template matching), `apps` (tool metadata, results)
and `route` (aggregate, read routing, reach); `mcp/ingress/resources.rs`; `lmgw_api_types::mcp_apps`
(the revision, the identifiers, `ToolResultFrame`), re-exported by `lmgw-client`. Where §7 was
silent:
- **Whose a URI is.** Its candidates are the enabled servers it can belong to, by name: the servers
  of the longest tool prefix whose namespace holds it, else every bare server (and for a URI with
  no authority — `urn:…`, `data:…`, which no prefix changes — every server). A candidate *claims*
  it in one of three ways, strongest first: a tool of its own names it in
  `_meta.ui.resourceUri`; it lists it; it has a `uriTemplate` it fits (each `{…}` matching any run
  of characters). The strongest claim has it, and the first by name among the candidates that
  claim it that way, so a template never takes a URI another server lists or a tool names. A
  prefixed server's template needs a literal `scheme://` (which the namespace then prefixes); one
  without (`{uri}`, `urn:{id}`) would claim every other server's `urn:…`, so it claims nothing and
  is not listed. A URI in a namespace that no candidate claims is still the first candidate's (an
  unlisted UI resource, a server not connected); any other unclaimed URI is `-32002`. A namespace
  with one server is answered without listing anything. The template match is linear in the URI
  per literal run (each run at its leftmost place), not a backtracking search, so a long URI
  costs no more than reading it.
- **The listings agree with reads.** `resources/list` shows a server's resource only where that
  rule gives it the URI; a bare server's URI in a prefix's namespace, or one another server claims
  more strongly or earlier by name, is left out (logged at debug). Who has each URI is worked out
  once per listing. Templates are shown once each, by their namespaced text. Both lists answer in
  one page (a cursor is `-32602`) and connect nothing: the ready servers'.
- **Who is asked.** A listing asks the servers the caller reaches, and of the others only those
  that could take one of their URIs (a candidate earlier by name; a tool's claim is known without
  asking). A read asks the URI's candidates, and none when the caller's scope reaches none of
  them (`-32002` at once, nothing started). A device row the caller does not reach is never asked
  about a URI with an authority; what its tools name still counts. A URI without one can still
  take an unreached device's listing to settle.
- **Reads go where calls go.** A server not connected is connected (a service agent's app started
  first); for a caller whose scope narrows it, before its reach is judged, since a stopped (idle-
  reaped) server offers no tools to judge by — only for a caller whose scope can reach it at all
  (`may_reach`). Any other read is bounded by the row's `timeout_ms`; a device row's goes over its
  link as a call does — `_meta` (`lmgw/caller`, `lmgw/approval`, `lmgw/timeout_ms`), cancelled on
  the device at the timeout and when lmgw stops waiting, cancelled before lmgw closes the link
  (the link's open requests count reads as well as calls), and a closed link reported with the
  close's reason; an offline device row is answered at once. The answer's
  `contents[].uri` comes back namespaced; a server's JSON-RPC error passes on with its code. No
  size bound beyond the link's own (`mcp.host_max_message_mb` closes a device link, naming it).
  Reads write no `request_logs` row; they are no tool calls.
- **Reach.** A resource is listed to and readable by a caller that reaches its server:
  `ToolScope::may_reach` for the row (L16 for a device row), and where the scope narrows it
  (`narrows_for`), a tool of that server the scope admits and the owner has not switched off, or
  the server's whole namespace (`admits_namespace`). So a bare server that offers no tools is
  out of reach for every narrowing scope. Out of reach is `-32002` with why, as a tool out of
  scope answers `-32601` with why.
- **Notifications.** `GET /mcp` sends `notifications/resources/list_changed` beside every
  `tools/list_changed` (a server connecting or going changes whose resources are listed) and on
  an upstream's own `notifications/resources/list_changed`. `subscribe` is not offered; the method
  answers `-32601`.
- **`/mcp/admin`** serves no resources and does not advertise them.
- **`visibility`** is applied in `exec::resolve`, the one path every model run lists tools through
  (Chat, `/v1/responses`, realtime, in-process agents, `/v1/mcp/servers/{label}`); a label whose
  remaining tools are all app-only fails with that reason. On `/mcp`, `tools/list` gives the
  app-only tools only to a session whose `initialize` declared `io.modelcontextprotocol/ui` in
  `capabilities.extensions` (the session keeps that beside its client name): lmgw advertises the
  extension to every southbound server, so their app-only tools would otherwise reach clients
  that have no views to call them for. A call is not refused on it. A malformed `visibility` (no list)
  counts as absent; a single string is read as a list of one.
- **The `tool` result frame** always carries `server_label`, `ui_resource` and
  `structured_content` (`null` when none). `structured_content` is `ToolOutcome::structured`, the
  MCP result's `structuredContent` kept apart; the model still gets it among the result's blocks,
  as before (the extension says hosts keep it out of the model's context: open for the owner —
  decided 2026-10-09, §7.6: it does not, when the content is not empty).

### 7.6 Decided by the owner, 2026-10-09, and as built

**Overlapping tool names are not refused, and neither tool is shadowed: the colliding tools take
their server's prefix** (`mcp/names.rs`). Before, two servers whose tools had one exposed name (two
bare servers' `read`, a bare server's literal `gh__search` beside the `search` of the server
prefixed `gh`, two servers sharing a prefix) kept the first by server name and dropped the other
with a warning; a bare server's tool in `lmgw__`, `docs__` or `kb__` was dropped too.
- **A tool's names, in order:** its own name (the upstream's, or the owner's rename), under its
  server's prefix when it has one — the name it has when nothing collides — then
  `<server name>__<own name>`, the server's name with every character a tool name cannot carry as
  `_` (what a bare server's label is, spelt as a prefix; `my server` → `my_server__read`).
- **Who moves: every side of equal standing.** Decided by the owner (2026-10-09): every tool
  caught in a collision moves, so the collision stays visible. A collision is not settled quietly in one server's favour: every tool caught in it shows,
  by its new name, that another source offers its name. A name is held most strongly by a prefix,
  then by a server's name, then not at all (a bare name). Where two tools have one name, the weaker
  hold moves to its next name; two of one strength both move. So two bare servers' `read` are
  `alpha__read` and `beta__read`, and `read` routes nowhere — not the first by name keeping it,
  because which server is "first" changes when a server is added, renamed or not connected, and a
  name that silently routes to another server than the one a client listed it from is what
  `call_listed` exists to refuse. A bare server's literal `gh__search` moves for the prefixed
  server's (`aaa__gh__search`), which keeps its name; two servers that share a prefix both take
  their names. A tool in a reserved namespace moves out of it (`impostor__lmgw__status`). Repeated
  until nothing collides or nothing can move; what still collides (two server names that spell
  alike) goes to the strongest hold, then the first by server name, and the other is skipped and
  surfaced, as a name past 64 characters is. A server offers each name once: one it lists twice,
  or a rename onto another of its tools, keeps the first and skips the rest (surfaced), before any
  name is settled. **A device row's tools never leave its label:** they have only `<label>__<own>`,
  so a name two of its own tools share is skipped and surfaced, never moved to
  `device_<key>__…` outside the namespace its reach is decided by (§5.6).
- **Stable.** Only colliding tools change name; every other keeps its own. Which tools collide is
  read from every enabled server's tools: the connected ones' as listed now, every other's as it
  last listed them — **`mcp_known_tools`** (migration 0078: the upstream names per server,
  replaced whenever a connected server's listing differs, gone with the server; renames and the
  prefix apply when the aggregate is built). So a name does not change when a server is reaped,
  sleeps, goes offline or the gateway restarts, nor with the order servers connect in. A disabled
  server claims nothing. A new tool that collides moves the existing one with it — the price of
  never routing a name to a server it was not listed from.
- **Routing.** The aggregate's reverse map routes the prefixed name to its server, which receives
  its own tool name, as for every tool. A call of the bare name answers `-32601` (`tool_moved`)
  naming where the tools went. A run that listed the bare name before the collision (a realtime
  session, a `/v1/responses` continuation, a Chat turn's listing) calls the tool of the server it
  listed it from under its new name (`call_listed`): that is the tool the client was shown.
- **The owner's switch** holds for a tool under every name it has or would have
  (`Aggregate::spellings`: its own, then under its server's name): switched off under the name it
  had before a collision, it stays off under its new one; switched off under the name a collision
  gave it, it stays off when the collision ends and it has its own name again. The inventory shows
  it off, saying which switch (the now stale record of the other name) turns it on, and
  `tool_set` switching the tool's current name on says it is still off and which switch holds it.
  Every tool entry a collision moved carries `moved_from` and `moved_reason`: who else claims the
  name — another server's tool, or one of lmgw's own namespaces — and, for a server not connected
  now, that its claim is the tools it listed when it was last connected.
- **A key's tool scope** (and a device's) holds the same way, failing closed: a **deny** list
  refuses a tool when it denies any of its names, so denying `delete` keeps out `alpha__delete`
  and `beta__delete`, a pattern `gh__*` keeps out the twins `gh_work__search` and
  `gh-home__search`, and denying `alpha__delete` keeps alpha's tool out once it is `delete` again.
  An **allow** list admits the names it names and no other: allowing `delete` admits no tool a
  collision moved away from it. Read wherever the scope is (`ToolScope::admits`: `/mcp`'s list and
  call, `/v1/responses` and realtime runs, a device's Chat turn, resource reach).
- **Approvals.** `require_approval` names match either spelling as before (the exposed name, the
  tool's own), so a rule written `read` or `alpha__read` gates `alpha__read`; a turn gates a tool a
  rule gates under any of its names, so a rule written `alpha__read` while the collision lasted
  still gates alpha's `read` after it. A label's spellings (`exec/target.rs`) gain
  `<server name>__<own>` for the server's tools a collision moved now (and only those: a server's
  own literal `alpha__x` is a tool of that name, not `x`), so the floor (`effective.rs`) and the
  write check (`chat_tool_write/approval.rs`) compare a rule written in the prefixed spelling with
  one in the tool's own as one tool. An approval request's `name` stays the tool's own.
- **Wire names** (realtime's `mcp_list_tools`, `/v1/mcp/servers/{label}`) of a moved tool are its
  own name, as every other tool's are.
- **MCP Apps.** A tool's `_meta.ui.resourceUri`, the resource links in its results and the
  resources `/mcp` lists stay in the server's namespace (its tool prefix; a bare server's URIs
  unchanged): a collision moves a tool's name, never its resources.
- **Whose a listed tool is** (added 2026-10-09). A view calls its server's tools by the server's
  own names; a host that maps such a call onto `/mcp` by the server's prefix cannot find a tool a
  collision moved to `<server name>__<tool>` (or one the owner renamed): nothing in the listing
  said which server the moved name belongs to. So `/mcp`'s `tools/list` stamps **every** tool of
  a registered server, moved or not, with `_meta["lmgw/server"]`: `{label, name, tool}` — the
  server's label (its tool prefix, else its name: the Chat frames' `server_label`), its name
  (unique; two servers that share a prefix share a label), and the tool's name as the server
  itself lists it. Every tool, not only moved ones, so a host needs one lookup — the listed tool
  whose stamp has the view's server and the called name — and the answer does not change when a
  collision starts or ends. lmgw sets the key, replacing anything a server put under it; lmgw's
  own toolsets (`docs__`, `kb__`) carry none; only `_meta` is added (MCP leaves it to the
  server). Built in `mcp/ingress/server_meta.rs`, typed as
  `lmgw_api_types::mcp_apps::{SERVER_META, ToolServer}` (re-exported by `lmgw-client`).
- **What is still refused:** a hosting label or a tool prefix whose *namespace* runs into another
  (§5.6: a device's reach is decided by its namespace, so that stays a config-time refusal, not a
  naming rule) — and so is a server's **name** whose spelling as a prefix runs into a device's
  label (`phone.` and `phone_` are `phone_`, whose moved names `phone___…` would fall among
  `phone__…`), both when the server is written and when the label is granted — a tool prefix of
  lmgw's own namespaces, and in a realtime session a client function named like a listed MCP tool
  (the client's own name; it renames it) or two labels of one server.

**`structuredContent` stays out of the model's context when `content` is not empty** (MCP: content
is for the model, structured content for the host and its views). `mcp::exec::blocks_from_result`
— the one conversion from an MCP result to model input — gives the model the content blocks, and
the structured content (as JSON) only when the content is empty, as before. It covers every path:
a Chat turn, `/v1/responses` (its `mcp_call.output` too), a realtime session, in-process agents
(a batch step reads the host's copy, `ToolOutcome::structured`, as data first) and an MCP task's
result (`mcp::tasks::ending_blocks`). The host still gets it: the Chat frame's
`structured_content`, and for a late task result the result row's `task.structured_content`
(stored beside the blocks in `mcp_tasks.result` as `{blocks, structured_content}`; a row of an
earlier build, the bare block array, still reads).

**The Chat's `tool` frames carry what a client that shows a call's view needs** (Kai shows a
call's view by itself). Fields are only added; every existing one stays.
- `ready`: `{index, name, arguments, call_id, server_label, needs_approval, ui_resource}` —
  `needs_approval` from `LoopEvent::CallReady` (a gated call is followed by its `approval` frame;
  a resumed turn's decided calls say `false`), `server_label` `null` for a client function.
- `result`: adds `call_id` and `content`, the MCP result's content blocks as the server sent them
  (annotations kept, resource URIs namespaced as `/mcp` sends them); a tool lmgw runs itself or a
  call that never reached a server says its result in the same shape. With `structured_content`
  and `is_error` that is MCP's `CallToolResult` for the view. `output` stays the flattened text.
- **Size:** no bound of lmgw's own. A result is as large as its server sent it, images in full; a
  device's is bounded on its link by `mcp.host_max_message_mb`, which closes the link naming the
  setting. The SSE stream has no frame limit; a bound realtime session's `lmgw.chat.frame` is as
  large as the frame, and a client sets its WebSocket's message limit to what it accepts.
- Typed as `lmgw_api_types::mcp_apps::{ToolReadyFrame, ToolResultFrame}` (re-exported by
  `lmgw-client`); the API docs' tool frame and `lmgw.chat.frame` say the fields.

**Also decided (recorded; the code already agrees):**
- **A resource read is not a request:** it writes no `request_logs` row (none ever did — the
  routing above). Should a client show one, it shows it as a tool call in the thread, never in the
  request log.
- **Screenshots stay in the request log by default:** the call of a tool that takes one (a
  device's `see_screen`) writes its tool-call row like every call, and nothing hides it unless a
  later setting does. The row holds the call (tool, server, caller, timing, error), not the image:
  `request_logs` stores no bodies.
- **MCP UI support is announced to every connected server**: lmgw's client capabilities carry
  `io.modelcontextprotocol/ui` on every southbound `initialize`, device links included
  (`mcp/handler.rs`), whatever its views will be used for.
- **Calls a view makes under Kai's (a device's) key are acceptable**: a view's `tools/call` on
  `/mcp` runs as the device, within its key's reach (§5.6), as the device's own calls do.
- **A folder with no profile (`profile_id: null`) means inherit**: its new threads take Settings →
  Chat's `chat_profile`, else none (personality-profiles design §3.1); a folder default never
  clears a thread's profile.

## 8. `ring.js` takes a transparent flag (H) [Desktop 1, optional]

- **`inputs.transparent`** (boolean, default false) joins the contract (chat-voice §10).
- **`engine.js:342`** hands the factory `{palette, transparent}`.
- **`ring.js`** then uses `getContext("2d", {alpha: transparent})` (`:28`) and `clearRect` in place of
  the background fill when transparent (`:72`).
- **The ribbon and the orb** ignore the flag, and the contract says so.
- **The dashboard** passes nothing and renders byte-identically.

*Built (api WP1), 2026-10-07:* as above. `engine.js` hands every factory `{palette,
transparent}`, `ring.js` clears where it filled, and the contract (chat-voice §10) and
`engine.js`'s module doc name the input. A client mounts with `{...inputs, transparent: true}`
and drops its shim. The viz harness reads the corner pixels: the ring's transparent with the
flag and opaque without it, the ribbon's opaque with it (`scripts/drive/chat-voice-viz.json`).

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
  realtime, discovery and Chat turns; and an `all` device writing another device's label into a
  thread or a folder default (L5, review W3-9) refused.
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
  *As built (WP8):* also the claim order (tool, listing, template), a long URI against a deep
  template, a narrowed key reading an idle-reaped server, an unreachable caller's read starting
  nothing, a listing asking no unreached device, app-only tools only for an apps-host session, an
  upstream's own `resources/list_changed`, and a device read's timeout, cancel and link close.
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

- ~~A per-device opt-in grant for the self-admin toolset, so a chosen device could reach the
  threads that carry it.~~ *Built 2026-10-07:* the device's switch "may use lmgw's admin tools"
  (`self_admin`, L3's and L5's notes), a level per device since the pre-merge review's P-3.
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

## Appendix: The final review's dispositions (2026-10-07, against 46332aa)

| # | Finding | Disposition |
|---|---|---|
| F-1 | The tray's Quit skips the stop | **Changed:** every exit runs `gateway::quit` — the server's stop, its return within `QUIT_WITHIN`, then the containers' stop (§1.6's note); tested in `gateway::tests` and live (`shell-check.py --only quit`). |
| F-2 | A cut turn's save and row not waited for | **Changed:** turns, model calls and row writes hold a `Running`; the stop waits for them (§1.6). |
| F-3 | A stream's generation taken when it is built | **Changed:** `ServedAt` on every request; the session's `Running` taken before the 101 (§1.6). |
| F-4 | The drain unbounded; the agent proxy's streams | **Changed:** one bound (`STOP_WITHIN`) for drain and wait, what is open logged; the proxy's bodies and tunnel end at the stop; a second signal exits at once (§1.6). |
| F-5 | `takeover=never` refused by an ending session | **Changed:** an ending binding and the same key's do not count (§1.7). |
| F-6 | The folder delete reads in a deferred transaction | **Changed:** `begin_write`; no other new store path reads first. |
| F-7 | `devices_hidden` invisible and permanent | **Changed:** shown to the owner, cleared by "Show to devices again" (L3's note). |
| F-8 | The device-key 401s undocumented | **Changed:** the bearer scheme, the Chat tag and the handshake name them. |
| F-9 | Test gaps | **Changed:** a Chat turn and `/mcp` at a stop, a stalled stream and the bound, `takeover=never` at once / own key / after a restart, the folder delete beside a commit; the F-3 window by the `Stops` unit test. |
| F-10 | Old 8-hex tags read as a restore | **Changed:** checked by their numbers (§2.3's note). |
| F-11 | The in-memory anchors leak, and are partial | **Changed:** held by each pool's `after_connect` hook, for the store, knowledge and quickdoc pools. |
| F-12 | `idle_rollover_at` overflows | **Changed:** checked arithmetic, `None`. |
| F-13 | `FeedItem::Unknown` folds two cases | **Changed:** `FeedItem::Unreadable` again (§4.1's note). |
| F-14 | W6-9's test checks listed fields only | **Changed:** the frame round-trips through the clients' type. |
| F-15 | `takeover=never`'s 409 before the reach re-check | **Changed:** re-read first, 404 when out of reach (§1.7). |
| F-16 | Texts | **Changed:** "lmgw is stopping or restarting"; §1.6's heading; `FolderCreate`'s retention fields. |
| F-17 | UI and crate small things | **Changed:** the toolset notice for any folder; the deep link's query dropped; `Replies` documented, with `forget`. |

## Appendix: The pre-merge review's dispositions (2026-10-07, against a57ee57)

| # | Finding | Disposition |
|---|---|---|
| P-1 | A catch-up across a switch-off renders the toolset as it is now | **Changed:** the catch-up renders at the narrower of the reach then and now; a switch met there is one `resync` (L3's note). Tested: away across an off, across an on and an off, across an on. |
| P-2 | An update's restart skips the quit sequence | **Changed:** the updater runs the sequence, then restarts; an exit during a quit waits for it (§1.6's note). Tested with F-1's seam. |
| P-3 | At `full`, the switch hands a paired device the host | **Changed (the owner's decision):** a level per device — off, read only, full — capped by the gateway's; programs on this machine need full; confirmations say so (L3's note). |
| P-4 | Live frames queued under the old reach go out after it moved | **Changed:** re-checked as they go out; a stale `state` is dropped (L3's note). |
| P-5 | `open()` reads the switch in the wrong order | **Changed:** the switch and the head in one read transaction (L3's note). |
| P-6 | The switch ignores the device's own `lmgw__*` patterns | **Changed:** they narrow the label; an attach they leave nothing is refused (L5's note). |
| P-7 | Another device's folder delete strands a switched device's threads | **Changed:** the threads that stay are recorded before the folder's removal (L3's note). |
| P-8 | Switch-off and Disable leave in-flight work running | **Changed:** turns cancelled, `self_admin` checks enabled and expiry, the row checked per call, realtime lists again (L3's note). |
| P-9 | A client cannot learn while connected that its switch changed | **Changed:** `state.self_admin`, sent after every change. |
| P-10 | Test gaps | **Changed:** the P-1 catch-ups, the restart order, read only through the device path, `/v1/responses`, `/v1/mcp/servers` and unbound realtime with the switch, P-4, P-7 and P-8, and P-3's levels. The second signal's exit stays without a test of its own: its status is unit-tested (`server::signal_exit_code`), the live shell check is unchanged. |
| P-11 | A quit has no visible state, and is not panic-safe | **Changed:** window hidden, tray says "quitting", a second Quit logged, the exit runs from a guard (§1.6's note). |
| P-12 | Log strings with runs of spaces | **Changed:** the line continuations are back. |
| P-13 | "The gateway has stopped" after a timeout | **Changed:** the log says whether it returned. |
| P-14 | The second signal's exit code | **Changed:** 143 for SIGTERM, 130 for Ctrl-C. |
| P-15 | Doc comments | **Changed:** `principal_list` has its doc back; `labels()` names the device case. |
| P-16 | A false sentence in the hidden-folder notice | **Changed:** it says what a device allowed the admin tools still sees. |
| P-17 | `takeover=never` over an ending session says "taken over" | **Changed:** an ending binding is not named taken over. |
| P-18 | "Show to devices again" in a second transaction | **Changed:** part of the folder patch's transaction. |
