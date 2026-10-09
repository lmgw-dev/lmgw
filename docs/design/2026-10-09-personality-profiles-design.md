# Personality profiles: how a Chat thread's model talks, in text and in voice

**Reviewed by the owner 2026-10-09: D1–D23 approved, with decision 7 added.** Docs only; nothing is built. Paths are relative to
`crates/lmgw-core/src` unless they name a crate, and line numbers are at `main` f57aa71. Companion
records are cited by short name: "chat-voice §n" (2026-10-03), "chat-complete §n" (2026-09-30),
"client-apps §n" (2026-10-06), "realtime §n" (2026-10-01), "capabilities §n" (2026-09-17). The
desktop client's own design record is "the client's record" (its rulings 26 and 29, K29 and K31).

**Today:**
- A voice turn's system message is the thread prompt, then a voice block (chat-voice §8.5,
  `web/chat_voice/prompt.rs:56-150`). The thread prompt is a copy taken at creation of
  Settings → Chat's default (`web/chat.rs:183-188`, `web/chat_folders/apply.rs:56`,
  `web/chat_folders/current.rs:82`). For nearly every thread that copy is
  `BUILTIN_CHAT_SYSTEM_PROMPT` (`config/chat_prompt.rs:14`), an explainer that asks for Markdown,
  tables and code.
- The voice block only changes form: `VOICE_FORM` says "short and conversational", with no length
  (`config/settings_classes.rs:497-542`). Spoken replies come out as walls of text.
- A voice turn already asks reasoning off when the thread sets no reasoning field
  (`voice_request`, `prompt.rs:104-120`). Text turns use the thread's fields or the route default
  (`web/chat_reasoning.rs:17-24`).
- Speech style, TTS alias and voice exist per thread (`store/chat_voice.rs`, `ThreadVoice`). They
  resolve thread → `chat_*` → `realtime.*` (`web/chat_voice/resolve.rs:268`). A voice applies only
  with the TTS it was chosen for (chat-voice §2.3, M1).
- A bound session re-reads the thread before each response (`realtime/thread/turn.rs`). It reuses
  its warm stages while the thread's `updated_at` and the snapshot are unchanged
  (`realtime/thread/stages.rs`).
- Devices hold `Cap::Chat`, which covers every `/chat/api` row. They never reach `/api/op`
  (client-apps §1.2). The desktop client's conversation is the current thread of an ongoing
  folder. Each rollover copies the folder's defaults into a new thread (client-apps §3).
  `POST /chat/api/folders/{id}` with `defaults_patch` also applies the change to the current
  thread (§3.4; `web/chat_folders/apply.rs:65-`).

## Decisions

*Owner, 2026-10-09* (the desktop-client design discussion; the client's record ruling 29, K31):
1. **Profiles are an lmgw Chat feature, not the client's.** Any chat thread can pick one. The
   client's conversation picks one too: from the client's tray, and by voice through a client
   tool `personality_set {name}`. The personality is the same from the desktop client, the phone
   and the dashboard, and the thread owns the prompt.
2. **The reason:** a voice turn is the thread prompt (by default the built-in explainer that
   answers in Markdown) plus a voice block that only changes form, so spoken replies are walls of
   text.
3. **What a profile holds, as proposed:**
   - a persona (who it is, how it sounds);
   - a concrete length rule instead of "short": one to three sentences, details on request, no
     closing recap, no "let me know if";
   - three to five short static example exchanges. They are static so the prompt cache holds
     them, and their token cost is shown the way a tool list's is;
   - the speech style and the TTS voice (both exist per thread already);
   - reasoning on or off, since thinking is the costliest voice delay;
   - its own voice block, replacing the generic one.
4. **The editor is one Leptos component in a crate shared with the client's settings window.**
   It has the editor's Test button plus "speak a sample".
5. **No interim prompt change.** The current prompt stays until profiles exist.
6. **Profiles overlap Presets** (a planned feature).
7. **The editor also lives in lmgw's own dashboard**, not only in the desktop client's settings
   window: it is the same kit component (§4.2), because lmgw's own Chat voice uses profiles.
   Designed in §4.1 (`/chat/profiles`, the thread drawer, the folder form, the voice panel's
   picker) and §2.2–§2.3 (voice turns take the profile's voice block, TTS alias, voice and speech
   style).

*D1–D23, taken in the draft and approved by the owner on 2026-10-09, each with its reason:*
- **D1. A profile is how the model talks, not what runs.** It holds persona, length rule,
  examples, voice block, reasoning on/off, TTS alias, voice and speech style. It holds no model,
  sampling, tools or knowledge bases: those are Presets' bundle, and a future preset may name a
  profile. *Why:* this keeps the two features from owning the same field twice.
- **D2. "Default" is no profile** (`profile_id` NULL), not a stored row. It is today's code path,
  byte-identical, and the name "Default" is reserved. *Why:* nothing changes for any thread
  until a profile is picked (decision 5).
- **D3. A non-empty persona takes the thread prompt's place.** The thread's `system_prompt` is
  kept unchanged in storage and applies again when the profile is unset or has no persona.
  *Why:* layering a persona over the built-in explainer leaves exactly the contradiction of
  decision 2 (its Markdown and "owner trying out models" lines).
- **D4. The length rule and examples apply to every turn of the thread, text and voice.** *Why:*
  decision 1 says the personality is the same everywhere. A profile that wants long text and
  short speech puts its length rule into its voice block.
- **D5. Examples go into the system message as a labelled block, not as fake user/assistant
  turns.** *Why:* the result is just as static and cacheable, the model never treats an example as
  something already said in this conversation, and history, exports and the feed stay true.
- **D6. Voice block:** absent means today's generic block. `""` means none. A text is used
  verbatim and wins over `realtime.default_instructions`. *Why:* this is the same three-way rule
  as `realtime.default_instructions` (chat-voice §8.5).
- **D7. Reasoning order:** the thread's explicit fields, then the profile's on/off, then the
  voice turn's default off, then the route default. *Why:* the more specific choice wins, as with
  every override in the Chat. The profile's value goes through the existing `ReasoningControl`,
  so local llama-server rows get `chat_template_kwargs.enable_thinking`, and the capabilities
  §5.6 fit handles the rest.
- **D8. Voice order:** thread → profile → `chat_*` → `realtime.*`, field by field. A profile's
  voice belongs to the profile's TTS alias, or to the Chat's own TTS when the profile names none,
  by the existing M1 rule. *Why:* there is one resolution chain with one more tier, and no voice
  is sent to a model that does not have it.
- **D9. Assignment:** a thread references a profile by id. A folder's defaults carry
  `profile_id`, copied into each new thread like every default. Settings → Chat gains
  `chat_profile`, the profile new threads start with (empty = none, the default). *Why:* the
  client's conversation rolls over, so its profile must live on the folder; and the global
  default mirrors the default chat prompt.
- **D10. Storage:** one table with a JSON `body`, strict on input and tolerant on read (the
  `ThreadVoice` pattern). Every profile is loaded into the config `Snapshot`. *Why:* voice
  resolution is synchronous over the snapshot (`resolve(snap, thread)`), and a profile edit then
  invalidates a bound session's cached stages as any config change does.
- **D11. One built-in profile ships, "Concise"** (§3.3). Its texts are not stored, so it follows
  improvements, as `BUILTIN_CHAT_SYSTEM_PROMPT` does. It can be reset, deleted (with a confirm)
  and re-created. *Why:* the voice case needs a good default nobody has to write, and decision 5
  forbids changing anything unasked.
- **D12. The HTTP surface is `/chat/api/profiles*` (`Cap::Chat`), with no `/api/op` op.** The
  logic lives in `ops/chat_profiles.rs`, which the routes and the self-admin tools share. *Why:*
  the client's window edits profiles with a device key, and devices are closed out of `/api/op`.
  Folders and thread settings live on `/chat/api` already.
- **D13. Devices may list, create, edit and delete profiles.** A TTS alias they write passes
  their key's scope. *Why:* decision 4 puts the editor in the client's window, and the client is
  the owner's own.
- **D14. `personality_set` needs no new lmgw route.** The client resolves the name against
  `GET /chat/api/profiles`, then sends `POST /chat/api/folders/{id} {defaults_patch: {profile_id}}`.
  `apply_to_current` (default true) switches the current thread through the thread settings
  route's checks. *Why:* client-apps §3.4 already is this mechanism.
- **D15. The feed gains stored `profile.created`, `profile.updated` and `profile.deleted`
  events.** *Why:* the client's tray menu and its tool's description list the names and must
  follow edits made elsewhere.
- **D16. Delete is confirm-then-do.** One `begin_write` transaction sets every thread using the
  profile to none (FK `ON DELETE SET NULL`) and strips `profile_id` from every folder default.
  The answer names what changed, and the editor's confirm says it beforehand. *Why:* this is a
  single-user app, and nothing should silently keep pointing at a gone row.
- **D17. Preview, Test and Speak work on the unsaved draft.** Test is one model call and stores
  nothing. Speak answers with one WAV. *Why:* the editor's loop is edit → try. One WAV keeps the
  shared crate free of the Chat's Web Audio pipeline, so an `<audio>` element plays it.
- **D18. Token cost is counted on demand, through the universal counter**
  (`proxy/count.rs:245`), with its approximation flags shown. The list never counts. *Why:*
  counting on a cold local model loads it.
- **D19. No caps.** Example count and text lengths are unbounded; the route's body limit shows
  up as a visible 413, and the model's context is the real bound. *Why:* the owner's no hidden
  limits rule. The editor's "three to five" is guidance text only.
- **D20. Unbound `/v1/realtime` and `/v1/chat/completions` get no profile in this round.**
  `session.lmgw.profile` (opt-in, lmgw-only) is Later. *Why:* they are OpenAI-shaped, and no
  client needs it now (the desktop client binds a thread).
- **D21. Edits apply from the next turn of every thread using the profile.** That includes a
  bound voice session's next response, with no rebind. *Why:* the prompt is built per turn, and
  D10 makes the stage cache see the change.
- **D22. Admin Chat threads may take a profile.** The admin wrapper (`agentchat::system_prompt`)
  stays around the assembled text. *Why:* there is no reason to special-case them; devices never
  see admin threads anyway (client-apps L3).
- **D23. The shared UI crate is `crates/lmgw-ui-kit`, the minimal cut for the editor** (§6.2).
  It has no Tauri, no router and no `/api` paths. The base URL and the 401 hook are injected, and
  lmgw-ui re-exports the moved modules so no importer changes. *Why:* the client's record leaves
  the crate's extent open (its §16 Q18); this cut is what one shared component needs.

## 1. Data model

### 1.1 The profile (wire shape, `lmgw-api-types/src/chat_profiles.rs`)

```json
{"id": 3, "name": "Concise", "builtin": "concise",
 "persona": "…", "length_rule": "…",
 "examples": [{"user": "…", "reply": "…"}],
 "voice_block": null,
 "reasoning": "off",
 "voice": {"tts_alias": null, "voice": null, "speech_style": null},
 "follows_builtin": ["persona", "length_rule", "examples", "voice_block", "reasoning"],
 "used_by": {"threads": 2, "folders": [{"id": 1, "name": "Assistant"}]},
 "created_at": "…", "updated_at": "…"}
```

- `name`: trimmed, non-empty, unique case-insensitively, and never `default` in any case
  (`409 profile_name_taken`, `400 profile_name_reserved`).
- `persona`, `length_rule`: text. Empty means none. `{{model}}` and `{{date}}` are expanded as in
  the thread prompt (`expand_chat_prompt`), and so are all profile texts.
- `examples`: an ordered list of `{user, reply}`, each side non-empty.
- `voice_block`: `null` = the generic block (D6), `""` = none, text = verbatim.
- `reasoning`: `null` (inherit) | `"on"` | `"off"`.
- `voice`: the `ThreadVoice` subset `tts_alias`, `voice` and `speech_style`, with the same
  checks and the same `""` = none meaning for `speech_style`. `tts_alias` is checked as task
  `tts`/`vdes`, as the thread's is.
- `builtin` and `follows_builtin` are read-only. On a built-in row, a written field equal to the
  built-in text is stored as absent, so it keeps following the built-in (the
  `set_default_chat_prompt` rule).
- `used_by` is computed on read and is the source for the delete confirm.
- The text form of examples, for MCP tools and paste:
  - one exchange is a `User:` line followed by a `Reply:` line;
  - continuation lines are indented by two spaces;
  - exchanges are separated by a blank line.

  It is parsed and printed by one function pair in `config/chat_profile.rs`, unit-tested
  round-trip.

### 1.2 Storage (`migrations/0071_chat_profiles.sql`; take the next free number at build time)

```sql
CREATE TABLE chat_profiles (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  builtin TEXT UNIQUE,                 -- NULL for the owner's own
  body TEXT NOT NULL DEFAULT '{}',     -- §1.1's content fields
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
ALTER TABLE chat_threads ADD COLUMN profile_id INTEGER
  REFERENCES chat_profiles(id) ON DELETE SET NULL;
INSERT INTO chat_profiles (name, builtin, body, created_at, updated_at)
  VALUES ('Concise', 'concise', '{}', <now>, <now>);
```

- **`body`:** a key that is absent means unset on an owner's row, and the built-in value on a
  built-in row. An explicit `null` means unset on either. It is read tolerantly: an unknown or
  unreadable key is dropped on its own (`ThreadVoice::from_stored`'s rule).
- **The seed runs once, in the migration.** A deleted built-in is never re-seeded.
  `POST /chat/api/profiles {builtin: "concise"}` re-creates it (`409` while it exists).
- **Folder defaults:** `ThreadDefaults.profile_id: Option<i64>`, inside the existing JSON. It
  joins `apply` and `check_defaults`, and existence is checked in the handler against the
  snapshot.
- **Temporary threads** hold `profile_id` in memory. Keep (`insert_kept_chat_thread`) copies
  it. Exports carry `profile: {id, name} | null`, `null` when no profile was
  used (decided by the owner 2026-10-09, as built).
- **Every write** (create, update, delete with its folder sweep, thread and folder assignment) is
  one `begin_write` transaction. The snapshot reload follows the commit, as for every config
  write. `store_begin_scan.rs` covers the new file unchanged.

### 1.3 The built-in "Concise" (`store/chat_profiles/builtin.rs`)

- **persona:** "You are a voice assistant on the owner's own machine, reached through lmgw, a
  self-hosted LLM gateway. You are the model behind the alias \"{{model}}\". Today is {{date}}.
  You talk like a capable colleague: direct, friendly, in plain words. When you are not sure,
  say so briefly. You can call tools only when tool definitions come with the request; without
  them, never claim to have looked something up."
- **length_rule:** "Answer in one to three sentences. Give details only when asked for them. Do
  not end with a summary or a recap, and do not offer further help (no \"let me know if…\")."
- **examples:**
  - "Should I take an umbrella today?" → "I can't see the weather from here, so I don't know. A
    quick look at a forecast will tell you."
  - "What's the capital of Australia?" → "Canberra. Many people guess Sydney, but Canberra was
    built as a compromise between Sydney and Melbourne."
  - "How does a heat pump work?" → "It moves heat instead of making it. A refrigerant picks up
    warmth from the outside air, a compressor makes it hotter, and it gives that heat off
    indoors."
- **voice_block:** `null`, so the generic block applies, and with it the language sentence and
  the date rule. **reasoning:** `"off"`. **voice:** all unset.

The owner reviews these texts; they are the one place this draft writes prompt words.

## 2. Prompt assembly (`web/chat_profile/assemble.rs`, new)

### 2.1 The parts

With `P` the thread's profile from the snapshot (none when `profile_id` is NULL or names a row
that is gone):
- **base:** `P.persona` when non-empty, else the thread's `system_prompt`, as today. Expanded.
- **length:** `P.length_rule`, when non-empty, as its own paragraph.
- **examples:** when `P.examples` is non-empty, its own paragraph:
  `Examples of how you answer:` followed by `User: …` / `You: …` pairs, separated by blank
  lines.
- **static part:** base, length and examples joined by blank lines. Nothing in it depends on the
  turn, except `{{date}}`, which changes daily, as today.

### 2.2 The system message per kind of turn

| Turn | System message, in order | Reasoning |
|---|---|---|
| Text turn, no reply language | static part | thread → `P.reasoning` → route default |
| Text turn with a reply language, `speak: true`, bound session with text output | static part, language sentence (as today) | thread → `P.reasoning` → route default |
| Voice turn (bound, audio output) | static part, voice block, tag hint | thread → `P.reasoning` → **off** → route default |
| Admin Chat | the admin wrapper around whatever the row above gives | as the row above |

**The voice block** keeps chat-voice §8.5's shape, with one new first branch:
1. `P.voice_block` is a text: the bridge, then that text verbatim; the language sentence goes in
   its own paragraph.
2. `P.voice_block` is `""`: no block and no bridge (the language sentence still goes in its own
   paragraph).
3. Otherwise today's rule: `realtime.default_instructions` set or `""`, else the built-in text
   without `VOICE_PERSONA`. `VOICE_NO_DATE` is judged on the **base**'s `{{date}}`, not on the
   thread's prompt.

The bridge opens the block when the static part is non-empty.

**No profile** gives base = the thread prompt, with no length and no examples: every branch
above is the current code, and `chat_golden.rs` pins it byte for byte.

### 2.3 Precedence, field by field

| Field | Order |
|---|---|
| Who the model is | `P.persona` (non-empty) → the thread's `system_prompt` |
| Length, examples | `P` only (no thread field) |
| Voice block | `P.voice_block` → `realtime.default_instructions` → built-in |
| Reasoning | thread's explicit fields → `P.reasoning` → voice default off → route |
| TTS alias, voice, speech style | thread → `P.voice` → `chat_*` → `realtime.*` |
| Languages, read-aloud, turn detection, ASR, audio input | unchanged (not in a profile) |
| Model, sampling, tools, KBs | thread only (D1) |

- `voice_resolved` gains `source: "profile"` for the three voice fields.
- The profile's voice follows M1: it is reported with `note` when the thread's own TTS differs
  from the one it was chosen for.
- `speech_style` with source `profile` goes to a bound session as `session.lmgw.speech_instructions`,
  as a thread's own does (`realtime/thread.rs:281-284`).

### 2.4 Caching and costs, stated

- The static part leads the system message and changes only when the profile, the thread prompt
  or the day changes. Switching profiles re-prefills the history once.
- Text and voice turns still differ after the static part (chat-voice §8.5's stated cost).

## 3. API

### 3.1 Routes (`web/chat_profiles.rs`, `web/chat_profiles/try.rs`; all `Cap::Chat`)

| Route | Body → answer |
|---|---|
| `GET /chat/api/profiles` | → `{profiles: [Profile], default_profile_id}` |
| `POST /chat/api/profiles` | `{name, …fields}` or `{builtin}` → `Profile` (201) |
| `GET /chat/api/profiles/{id}` | → `Profile` |
| `POST /chat/api/profiles/{id}` | fields to change (absent = unchanged, `null` = unset) → `Profile` |
| `POST /chat/api/profiles/{id}/delete` | → `{deleted, threads_cleared, folders_cleared: [{id, name}]}` |
| `POST /chat/api/profiles/preview` | `{profile: Draft, thread_id?, model?}` → `{static, text_turn, voice_turn, tokens?}` |
| `POST /chat/api/profiles/test` | `{profile: Draft, thread_id?, model, text, voice: bool}` → `TestAnswer` |
| `POST /chat/api/profiles/speak` | `{profile: Draft, thread_id?, text}` → `audio/wav` |

- **`Draft`** is §1.1's content fields. `thread_id` assembles as that thread would: its prompt,
  languages and voice overrides, with the draft in place of its own profile. Without it, an
  empty thread is used with Settings' defaults.
- **`tokens`** (`model` given): `{alias, answered_by, static, text_turn, voice_turn, approx: […]}`
  from `count_alias`, with `approx` as in `x-lmgw-count-approximate`.
- **`TestAnswer`:** `{system, reply, reasoning, reasoning_note, usage, first_token_ms, total_ms,
  reasoning_ms, answered_by}`.
  - It is one request through the Chat's egress, with no history and no max_tokens.
  - Under a device key it is a `policy_checked_call`, otherwise `internal:chat`, with its request
    row (client-apps §1.3).
  - `voice: true` assembles a voice turn with the tag hint for the resolved TTS.
- **`speak`** runs the read-aloud pipeline (clauses, the speakable pass, cues, the resolved
  TTS/voice/style/seed, chat-voice §6.1) and joins the clauses into one WAV.
  - Before any audio, the refusals are those of the read-aloud's speech plan (`tts_not_configured`,
    `voice_not_found`, …) as flat JSON.
  - It writes one TTS row, as `messages/{mid}/speak` does.
- **Writes and refusals:**
  - Profile writes and deletes record `profile.*` feed events in the same transaction.
  - A device's `voice.tts_alias` outside its key's scope is a `403 key_scope`.
  - An unknown `profile_id` in thread settings or folder defaults is a `400 unknown_profile`.
- **Thread and folder routes:**
  - `POST /chat/api/threads/{id}/settings` takes `profile_id: <id> | null`.
  - The thread JSON and the thread list carry `profile_id`.
  - Folder `defaults` / `defaults_patch` take `profile_id`.
  - `apply.rs`'s delta carries it to the current thread, with no special case.
  - A new thread takes the folder's `profile_id`, else `chat_profile`, else none, as it takes the
    prompt. A folder with no profile (`profile_id: null`) means inherit (the owner, 2026-10-09):
    it never clears a thread's profile, and its new threads take `chat_profile`.

### 3.2 Feed (`store/feed.rs`, `web/chat_feed/*`, `lmgw-api-types/src/chat_feed.rs`)

- **Stored events** `profile.created`, `profile.updated` and `profile.deleted`, each with
  `{id, name}`. They are delivered to devices too (no per-device filter), and a `resync` repeats
  the list as `profile.created` events (decided by the owner 2026-10-09, as built).
- **Changes the feed already carries:** a thread's or a folder's profile change arrives as
  today's `thread.updated` / `folder.updated`.
- **Old clients** read the new events as `Unknown`.

### 3.3 Settings key `chat_profile`

- A profile id or empty, through the settings path's files: `config/settings.rs`,
  `lmgw-api-types/src/settings.rs`, `ops/settings_patch.rs`, `ops/reads.rs`,
  `web/api_settings.rs`, and `lmgw__settings_set`'s description.
- An unknown id is refused on save. Deleting that profile empties the key in the same
  transaction.

### 3.4 Self-admin tools (`mcp/selfadmin/catalog/chat_profiles.rs`, dispatch in `mcp/selfadmin/chat_profiles.rs`)

- **`lmgw__profiles`** (read) lists every profile with its fields, examples in the text form,
  and `used_by`.
- **`lmgw__profile_set`** (writes) takes `action=create|update`, `id`, `name`, `persona`,
  `length_rule`, `examples` (the text form), `voice_block`, `voice_block_mode=generic|none|own`,
  `reasoning=inherit|on|off`, `tts_alias`, `voice` and `speech_style`. All are flat scalars, and
  each is optional on update.
- **`lmgw__profile_delete`** (writes) takes `id`. Its result names the threads and folders
  cleared.
- **Shared code:** all three call `ops::chat_profiles` and are gated by `self_admin` like every
  writer. Assigning a profile to a thread is not a self-admin tool; it is a Chat write.

### 3.5 The desktop client's path (D14; built in the client's repository)

- **List:** `GET /chat/api/profiles`, kept fresh by `profile.*` events. The tray submenu shows
  "Default" plus the names, with the folder's `profile_id` checked.
- **Set:** `POST /chat/api/folders/{folder}` with
  `{defaults_patch: {profile_id: <id|null>}}`. The current thread follows (`applied` in the
  answer), and the bound session takes it from its next response.
- **`personality_set {name}`:**
  - the name is matched case-insensitively, and `default` means null;
  - an unknown name is the tool's error, listing the names in one line;
  - it is a state-changing tool under the client's confirm defaults (K15/K24).
- **lmgw-client** gains the request builders and the DTOs; the feed reader learns `profile.*`.

### 3.6 API-docs entries

- A `DocRoute` for each of the eight routes in a new `openapi/planes/chat_profiles.rs`, merged
  by `planes/chat.rs`.
- `CAPABILITY_TABLE` gets the eight rows (`server.rs:195-`).
- The feed's `oneOf` gains three branches (`openapi_coverage.rs`'s one-branch test).
- The thread, folder and settings schemas gain `profile_id` / `chat_profile`.
- **No op** (nothing in `op_names.rs`/`OpDoc`) and **no `x-lmgw` header** (nothing in
  `LMGW_HEADERS`), by D12. `route_walk.rs` and `openapi_coverage.rs` enforce the rest.

## 4. UI

### 4.1 Chat page (lmgw-ui)

- **The thread drawer** opens with a **Profile** picker: "Default" (no profile), the profiles,
  and "Edit profiles…".
  - While the picked profile has a persona, the System prompt box stays visible, read-only and
    greyed, under "Replaced by the profile 'Concise' while it is picked"; its text is kept.
  - The Reasoning and Voice fields show `profile` as the source of what they inherit (§2.3).
- **The folder defaults form** gets the same picker. Settings → Chat gets "Profile for new
  threads".
- **The thread header** shows a profile chip beside the model when a profile is set; a click
  opens the drawer.
- **The editor page** is `/chat/profiles`, with the profile list beside the editor (PageFrame
  rules, one scroller). The editor component is the kit's (§6.2); the page only frames it and
  passes `thread_id` when it was opened from a thread. The dashboard hosts the editor as well as
  the desktop client (decision 7).
- **The voice panel** (the Chat page's voice mode) shows the same profile chip and picker as the
  drawer, writing the thread's `profile_id`. It plays replies in the voice that `voice_resolved`
  reports (§2.3, source `profile` when the profile supplies it), so nothing in the page picks a
  voice on its own; the source label says where each of the three voice fields came from.

### 4.2 The editor component (in `lmgw-ui-kit`)

- **Fields:**
  - Name, Persona, Length rule;
  - Examples: pairs that can be added, removed and moved, with "three to five" as help text;
  - Voice block: generic / none / own, with the generic text shown greyed as the reference;
  - Reasoning: inherit / on / off;
  - Voice: the kit's model picker (task `tts`), voice picker and speech style.
- **Static part:** the Preview's text-turn and voice-turn system messages, folded. **Count** on
  a chosen model gives "412 tokens on <alias>", plus each approximation flag in words. The button
  says that counting on a local model loads it.
- **Test:** a model (the thread's when opened from a thread), a message, and Text/Voice. It shows
  the reply, its reasoning folded, `first_token_ms`, `reasoning_ms`, `answered_by` and the
  reasoning note. Nothing is stored.
- **Speak a sample:**
  - the text defaults to the last Test reply, else the first example's reply, and is editable;
  - it plays through `<audio>` on the default output;
  - per-window output selection (chat-voice §12.3) is the Chat page's, not the kit's.
- **Built-in rows:** a "built-in" badge and "Reset to built-in".
- **Delete** is a two-click `ConfirmButton` whose armed label says what it clears ("used by 2
  threads and the folder 'Assistant'").
- **Look:** the existing graphite-and-blue tokens with no new colours, judged at 125% scale.

## 5. The shared crate cut (`crates/lmgw-ui-kit`, D23)

- **Moves out of lmgw-ui, unchanged:**
  - `api.rs` becomes `kit::http`: the base URL and an `on_unauthorized` hook are set once at
    boot. lmgw sets `""` and `session::lock`; the client sets its scheme's prefix and its own
    hook.
  - `scope.rs`, `fmt.rs`, `prefs.rs`, `catalog.rs`;
  - `charts::use_element_size` becomes `kit::element_size`;
  - `widgets.rs`'s base (toasts, `Select`) and `widgets/{confirm, form, modal, popover, section,
    clamp, split, dirty_guard, model_picker, voice_picker}.rs`.
- **lmgw-ui's shims:** its modules become `pub use lmgw_ui_kit::…` shims, so no page import
  changes.
- **Stays in lmgw-ui:** Tauri, `shell`, `ui_scale`, router-bound widgets (`sub_nav`, `page`) and
  everything else.
- **CSS:** the design tokens and the moved widgets' rules move from `assets/app.css` into
  `lmgw-ui-kit/assets/kit.css`. lmgw-ui's `index.html` links it before `app.css`, and `kit.css`
  references no lmgw-ui asset path.
- **The kit's network contract:** it calls only `/chat/api/profiles*`, `/v1/models` and
  `/v1/audio/voices`, relative to the base. A unit test greps the kit's sources for `/api/`
  outside `/chat/api/`, for `tauri` and for `leptos_router`, and fails on a hit.
- **What the client does with it** (its repository, after the pin moves):
  - its scheme handler proxies exactly those three prefixes to lmgw with its device key;
  - its `index.html` links `kit.css` by path;
  - `just rpm` stages the crate like `lmgw-client`;
  - it uses the same leptos minor version.

## 6. Tests

New integration tests are modules of the one `tests/it` binary. WP1 creates every new module
file below as an empty stub with its `mod` line, so later packages never touch `main.rs`.

- **`chat_profiles.rs`:**
  - CRUD;
  - name rules (case-insensitive taken, reserved `default`);
  - a built-in's absent fields follow the built-in text, and writing the built-in text stores
    absent;
  - delete clears threads, folder defaults and `chat_profile` in one transaction, and the answer
    counts them;
  - re-creating a built-in;
  - a body with an unknown key reads tolerantly;
  - the snapshot reloads after a write.
- **`chat_profiles_prompt.rs`:**
  - goldens for each row of §2.2, with and without each field;
  - `voice_block` null / `""` / text, with and without a reply language;
  - the `{{date}}` rule judged on the persona;
  - examples formatting;
  - reasoning precedence, including the voice default and an explicit thread field;
  - Admin Chat wrapping.
- **`chat_golden.rs`:** a thread with no profile sends byte-identical system messages for the text
  turn, the voice turn and the language variants.
- **`chat_profiles_voice.rs`:** the resolution tiers, `source: profile`, M1 for a profile voice,
  and `voice_resolved` notes.
- **`chat_profiles_try.rs`** (mock upstream and mock TTS):
  - preview texts equal what a send builds (the same function, asserted on a real send's
    captured request);
  - `tokens` with approximation flags;
  - Test stores nothing and writes one request row;
  - Speak answers a valid WAV, and its refusals are flat JSON.
- **`device_chat/profiles.rs`:**
  - a device lists and edits profiles;
  - a TTS alias out of scope gets 403;
  - folder `defaults_patch` applies `profile_id` to the current thread;
  - an admin thread stays unreachable.
- **`chat_feed/profiles.rs`:** the `profile.*` events, delivery to a device, and `resync`.
- **`realtime_chat_thread/profile.rs`:** a bound session's next response uses an edited profile
  without a rebind (the stage cache is invalidated by the snapshot).
- **Existing suites extended:**
  - `mcp_selfadmin.rs`: the three tools listed by mode, flat schemas, and the example text form
    round-trip;
  - `migrations.rs`: 0071 applies on a populated DB and seeds "Concise" once;
  - `route_walk.rs`, `openapi_coverage.rs` and `store_begin_scan.rs` pass unchanged.
- **The kit:** unit tests for the example text form (shared through `lmgw-api-types`) and the
  forbidden-path grep.

**Live checks** (dev instance via `scripts/dev-instance.sh`; synthetic or TTS speech only):
1. **Default unchanged:** a voice turn in a thread with no profile is the same system message as
   on `main`, read from the request log.
2. **Concise on a local llama.cpp model:**
   - twenty voice questions under Default, then under Concise, compared on sentences per reply,
     closing recaps, and first-audio time;
   - `reasoning_ms` is 0 under `off` (the current image toggles thinking only through
     `enable_thinking`).
3. **Editor in the app window at 125%:**
   - Count, Test (text and voice), and Speak a sample through `<audio>` in WebKitGTK;
   - the same in a browser tab.
4. **The folder path:** switching an ongoing folder's profile while a bound session runs: the
   feed shows `folder.updated` and `thread.updated`, and the next spoken reply follows the new
   profile.
5. **The desktop client:** tray pick and `personality_set` by voice. This one is in the client's
   repository.

## 7. Work packages (build order)

Each WP ends green on `bash ci/check.sh`, commits with explicit paths, and lands by rebase and
fast-forward. "∥" marks packages that share no file and run in parallel worktrees.

| WP | After | Owns | Delivers |
|---|---|---|---|
| **WP0 UI kit extraction** ∥ WP1 | — | `crates/lmgw-ui-kit/**` (new); workspace `Cargo.toml`, `Cargo.lock`; `crates/lmgw-ui/{Cargo.toml, index.html, assets/app.css}`; `crates/lmgw-ui/src/{api, scope, fmt, prefs, catalog, charts, widgets}.rs` and the moved `widgets/*.rs`; `ci/check.sh` if it lists crates | §5's cut with no behaviour change. Proven by `ci/check.sh` and a `webkit-check` screenshot match of Chat, Settings and Models |
| **WP1 Storage and types** ∥ WP0 | — | `migrations/0071_chat_profiles.sql`; `store/chat_profiles.rs`, `store/chat_profiles/builtin.rs`; `store.rs` (mod); `store/chat.rs` (`profile_id`, Keep); `store/chat_folders.rs` (`ThreadDefaults.profile_id`); `store/snapshot.rs`, `config/snapshot.rs`; `config/chat_profile.rs` (types, example text form); `lmgw-api-types/src/{chat_profiles, chat, chat_folders, lib}.rs`; every new `tests/it` file of §6 as a stub plus its `mod` line in `main.rs` and `device_chat.rs`/`chat_feed.rs`/`realtime_chat_thread.rs`; `tests/it/chat_profiles.rs` and `migrations.rs` filled | §1, without routes |
| **WP2 Prompt assembly** ∥ WP3 ∥ WP4 | WP1 | `web/chat_profile.rs`, `web/chat_profile/assemble.rs` (new); `web/chat_turn.rs` (`build_messages`/`request` call sites only); `web/chat_voice/prompt.rs` + `prompt/tests.rs`; `web/chat_reasoning.rs`; `tests/it/chat_profiles_prompt.rs`, `chat_golden.rs` | §2.1–§2.2, the reasoning half of §2.3 |
| **WP3 Voice resolution** ∥ WP2 ∥ WP4 | WP1 | `web/chat_voice/resolve.rs` + `resolve/tests.rs`; `realtime/thread.rs` (`shape_session`); `lmgw-api-types/src/chat_voice.rs` (`Source::Profile`); `tests/it/chat_profiles_voice.rs` | the voice half of §2.3 |
| **WP4 Routes, assignment, feed, setting** ∥ WP2 ∥ WP3 | WP1 | `ops/chat_profiles.rs`, `ops.rs` (mod); `web/chat_profiles.rs` (CRUD); `web/mod.rs`; `server.rs` (table rows); `openapi/planes/chat_profiles.rs` + `planes/chat.rs` merge line; `web/chat.rs` (thread settings, create); `web/chat_folders.rs`, `chat_folders/{apply, current}.rs`; `store/feed.rs`, `web/chat_feed/*`, `lmgw-api-types/src/chat_feed.rs`; `lmgw-client/src/{feed, requests}.rs`; the settings files of §3.3; `web/chat_export.rs`; `tests/it/{chat_profiles.rs (routes half), device_chat/profiles.rs, chat_feed/profiles.rs}` | §3.1's CRUD rows, §3.2, §3.3, the CRUD half of §3.6 |
| **WP5 Preview, Test, Speak** ∥ WP6 ∥ WP7 | WP2, WP3, WP4 | `web/chat_profiles/try.rs` (new); its three rows in `web/mod.rs`, `server.rs` and `openapi/planes/chat_profiles.rs`; `tests/it/chat_profiles_try.rs`, `realtime_chat_thread/profile.rs` | §3.1's last three rows |
| **WP6 Self-admin tools** ∥ WP5 ∥ WP7 | WP4 | `mcp/selfadmin/catalog/chat_profiles.rs`, `mcp/selfadmin/chat_profiles.rs` (new); `mcp/selfadmin/catalog.rs` (registration); `mcp/selfadmin.rs` (mod line, dispatch arm); `tests/it/mcp_selfadmin.rs` | §3.4 |
| **WP7 Editor component** ∥ WP5 ∥ WP6 | WP0, WP1 (DTOs); wired live after WP5 | `crates/lmgw-ui-kit/src/profiles.rs` + `profiles/*`; `crates/lmgw-ui-kit/assets/profiles.css`; the kit's `lib.rs` mod line | §4.2 |
| **WP8 Chat page integration** | WP4, WP7 | `lmgw-ui/src/pages/chat_profiles.rs` (new); `pages/chat_settings.rs`; `pages/chat.rs` (header chip, prompt-box note: call sites); `pages/chat_folders.rs`; `pages/chat_voice/section.rs` (source label, the voice panel's profile chip and picker); `pages/settings/chat_voice.rs` or the Chat settings section (`chat_profile`); `app.rs` (route); `index.html` (`profiles.css` link) | §4.1 |
| **WP9 Acceptance** | all | release notes, this record's "as built" notes | §6's live checks 1–4 |

- WP2's and WP4's tests both extend `chat_profiles*` files, but different ones: WP4 owns
  `chat_profiles.rs`, and WP2 owns `chat_profiles_prompt.rs`.
- WP0 and WP7 are the only packages in the kit.
- WP8 is the only package in lmgw-ui pages.
- The client's side (the tray, `personality_set`, the settings window hosting the editor, the
  pin bump, RPM staging) starts after WP5 and WP7 land and is designed in its own repository.

## Later

- `session.lmgw.profile` on unbound `/v1/realtime`, and a profile named by a Preset for API
  clients (D20, Presets' open question).
- A "voice profile" setting: the profile only voice turns use in threads without one.
- Per-profile reply language.

## Rejected

- **Examples as user/assistant turns** (D5).
- **Layering the persona over the thread prompt** (D3). The alternative, if vetoed: the thread
  prompt follows the persona as "This conversation's own instructions", which keeps the
  explainer's Markdown lines for text turns.
- **Profile CRUD as `/api/op` ops.** Devices cannot reach `/api/op`, and the editor must work
  from the client's window (D12).
- **Counting tokens on every list or open** (D18).
- **An interim change to `DEFAULT_VOICE_INSTRUCTIONS` or the built-in chat prompt** (decision 5).

## As built / review fixes (2026-10-09)

The whole-feature review after the build found the points below; each fix changed behaviour as
stated. Decisions marked **draft** were taken in the fix, not by the owner, and stand open to
the owner's veto; R1, the `null` profile in exports and the profile frames a `resync` repeats
were decided by the owner on 2026-10-09 and are no longer open.

- **R1. A device cannot steer the owner's admin tools through a profile** (D13, D22). A profile
  used by Admin Chat or by a thread with the self-admin toolset, or named by a folder default
  that attaches the toolset, puts its text into a system prompt that steers lmgw's admin tools,
  and the device could not even see that use (`used_by` counts only what it reaches). **Decided by the
  owner 2026-10-09, kept as built:**
  a device's change, reset or delete of such a profile is `403 profile_in_admin_use` unless the
  device may use lmgw's admin tools with writes: its own level capped by the gateway's, as stored
  now, the same gate as a device's `lmgw__*` write call. The check runs on the write's
  transaction (`store::chat_profiles::admin_use_in`, `ops::chat_profiles`), so the routes and
  the self-admin tools share it. The message counts the threads and folders and names none. A
  create is never refused (a new profile is used by nothing).
- **R4. The self-admin tools check a device's TTS alias against its key's scope** (D13, §3.4).
  The scope check moved from the routes into `ops::chat_profiles` (`check_tts_alias`), which
  both share, and runs before the alias is looked up. A device whose key is gone admits no alias.
- **R5. Preview, a voice Test and Speak check the draft's resolved TTS alias against a device's
  key first** (§3.1), before the hint reads its speech profile or Speak lists its voices: `403
  key_scope`. **Draft:** Preview is refused as a whole, not answered without the voice turn's
  hint, so a device fenced off the Chat's TTS cannot preview; a text Test still works.
- **R3. A folder's `profile_id: null` means "Settings → Chat's `chat_profile`"** for its new
  threads, and for its current thread when a change is applied (D9 as built; decided by the owner
  2026-10-09: a folder set to no profile inherits — null is not "none"). The
  folder form's picker now says so, naming the profile `chat_profile` holds, or that it is empty.
  The create route, a folder's new thread and the comparison that applies a folder's change to
  its current thread now share one start (`chat_folders::current::thread_start`/`new_thread`),
  so an Admin Chat thread never takes `chat_profile`, not even through a folder's change; it
  takes none of a folder's defaults, as at its create.
- **R2. The editor shows what was stored** (§4.2). After a create, a save or a reset the kit's
  panel puts the returned row into its list before the editor mounts on it; a save or a reset of
  the row already shown does not mount the editor again (it has taken the stored row itself).
  Before, a create left the pane empty and a save could show the list's older copy.
  `ProfilesPanel` exposes `dirty`: opening another profile, "New profile" or the re-added
  built-in with unsaved edits asks first (Discard and open / Stay), and `/chat/profiles`
  registers it with the dirty guard, so leaving the page asks as well. Discard on a new profile
  (pressable before anything is typed) drops the draft: the pane goes back to what it showed
  before "New profile" (that row, while the list holds it, else nothing), with no request.
  Before, it only emptied the form and left the draft open (the owner's report, 2026-10-09).
- **R6. "No speech style" (`""`) can be written and is kept** (§1.1, §3.4, §4.2). The editor's
  speech style is a three-way choice like the voice block's: Inherit (`null`), None (`""`), Own
  (the text; an empty own text blocks the save). A stored `""` loads as None and is written back
  unchanged when another voice field is saved; before, it became inherit. `lmgw__profile_set`
  gains `speech_style_mode=inherit|none|own` (as `voice_block_mode`; `own` is the default when
  `speech_style` is given); naming `speech_style` in `clear` stays "inherit", and giving both is
  refused.
- **R7. A deleted profile's id never comes back** (D16, §1.2, §3.3).
  - A folder's create or patch checks its defaults' `profile_id` again on its own write
    transaction (`400 unknown_profile`, nothing written), so a delete between the route's
    snapshot check and the write cannot be undone by it.
  - Temporary threads lose the id with the delete (`TempChats::clear_profile`, after the
    commit); the thread header's chip shows nothing for an id the profile list does not hold.
  - `save_settings` saves a `chat_profile` naming no row as none, with a warning in the log, so a
    settings save made from a snapshot whose reload failed after the delete cannot write the
    dead id back. **Draft:** dropped with a warning rather than refused, since the save is usually
    about another setting.

