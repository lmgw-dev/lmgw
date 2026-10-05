# Chat voice: dictation, read-aloud, audio devices, voice settings, and a realtime mode bound to the thread

Requested 2026-10-03: the Chat gets speech in (ASR) and speech out (TTS) so that voice
models can be tested where the owner already works. Requirements:
- audio devices the owner can choose;
- default models, with per-thread overrides;
- models loaded on first use, never when a chat opens;
- a **realtime mode** that replaces the text input with voice controls and a visualisation of the
  generated audio.

The design may be extended during implementation.

**Decided by the owner (2026-10-03):**
1. **No audio is stored.** Spoken turns are kept as text.
2. **The visualisation** is the owner's pick from a separate sample page with three variants: glowing orb,
   radial spectrum ring, mirrored waveform ribbon. §10 is the contract any of them slots into.
3. **The default setup** is speakers plus an input device that cancels echo itself, against the
   default output's monitor. The default echo mode is therefore "the input device cancels echo":
   full duplex with barge-in, browser echo cancellation off (§12).
4. **The accepted feature list:**
   - thread binding (§8);
   - the heard part stored, the unheard rest greyed (§3, §8.4);
   - load on first use (§4);
   - dictation (§5) and read-aloud (§6);
   - realtime controls (§9);
   - opt-in timing, served-model and loading events (§8.7);
   - the Voice settings group (§2);
   - mic badges (§3).
5. **Voice turns add realtime's spoken-style prompt and tag hint** to the thread's prompt (§8.5).
6. **Mic audio takes the configured fallbacks**, like any request. The model that served is always
   shown (§4.4).
7. **No realtime mode on Admin Chat threads, for now** (§8.1).

An adversarial review (2026-10-03, against 8df0374) is folded in. Branch `feat/chat-voice`.
Companion to [2026-09-30-chat-complete-design.md](2026-09-30-chat-complete-design.md)
("chat-complete §n") and [2026-10-01-realtime-voice-design.md](2026-10-01-realtime-voice-design.md)
("realtime §n").

## Ground rules for every work package

chat-complete's ground rules hold unchanged: file size, no hidden limits, routes, the settings
path, flat `ApiError`s from `/chat/api`, and the UI rules. On top of them:

- **File size.** These files gain call sites only:
  - `pages/chat.rs` (3.06k lines), `pages/settings.rs` (3.59k);
  - `web/chat.rs` (1.39k), `web/chat_turn.rs` (905), `web/api_settings.rs` (1.51k). It still
    gets the new fields; the settings path requires that.

  New code goes in:
  - core: `web/chat_voice.rs` + `web/chat_voice/*`, `realtime/thread.rs` + `realtime/thread/*`,
    `store/chat_voice.rs`;
  - UI: `pages/chat_voice.rs` + `pages/chat_voice/*`, `pages/settings/chat_voice.rs`;
  - shell: `src-tauri/src/media.rs`, `src-tauri/src/audio_out.rs`.
- **One mechanism per capability.** The new routes are thin adapters; nothing is re-implemented
  in wasm.
  - The realtime speech pipeline does clause cutting, speakable text, delivery cues, voice
    resolution and per-clause synthesis (`realtime/clauses*`, `realtime/responder/speech*`,
    `realtime/voice.rs`, `proxy/synthesize.rs`).
  - Transcription is `proxy::transcribe*`.
  - A chat turn is `web/chat_turn.rs`.
- **No new container-start path.** A load goes through the gate's admission (`gate::resolve` →
  `admit`) or realtime's warm (`realtime/warm.rs`). The GPU hold, `gpu_block` and the benchmark
  lease therefore apply unchanged.
- **The microphone is released** when dictation or realtime mode ends, on every exit path:
  button, key, thread switch, page leave, socket close, error.
- **Release notes** go to the local, gitignored `docs/release-notes.md`; they are not part of the
  commits.
- **Test privacy.**
  - Automated tests use synthetic or TTS-generated speech only.
  - Live checks with the owner's own voice run with the GPU hold off, or with no cloud fallback on
    the ASR alias.
  - Recordings are never committed.

## What exists today (relied on, not re-derived)

- **The send.** `POST /chat/api/threads/{tid}/send` → `chat_turn::start_turn`.
  - It runs knowledge retrieval, renders attachments, builds the request from the thread's
    settings, and spawns `chat::run_send` (plain) or `agentchat::run_send` (tools).
  - Frames go to an `mpsc` of `SseFrame`s: `turn`, `retrieval`, `delta`, `reasoning`, `tool`,
    `usage`, `stats`, `stop`, `error`, `done`.
  - The reply is saved under the live-turn ticket (`web/chat_live.rs`). Any `chat_live.write`
    or `begin` moves the thread's generation and cancels its live turn.
- **`agent::run`** collects each model turn's deltas and emits them only after the turn returns
  (`agent.rs:732-790`). Its comments say otherwise. Tool threads and `/v1/responses` therefore do
  not stream live.
- **The store.** `ChatMessageRow` has no metadata column; the newest migration is 0056.
- **`/v1/realtime`** (realtime §2–§11): PCM16-LE mono 24 kHz, `server_vad` / `semantic_vad` /
  manual turns, barge-in, `truncate`, `response.cancel`, the strict `session.lmgw` object.
  - The handshake resolves and policy-checks realtime's default chat, ASR and TTS
    (`realtime/mod.rs:166-197`). `session.created` and the connect warm follow at once
    (`realtime/session.rs:423-430`).
  - A cancel is synchronous in the core. What the detached call says afterwards is dropped by its
    generation (`realtime/lifecycle/cancel.rs`).
  - Timings go to one log line (`lifecycle/timing.rs`).
  - Warm never evicts and only logs.
- **The webview** (WebKitGTK 2.54, wry 0.55.1):
  - it has getUserMedia, enumerateDevices, AudioWorklet, AudioContext at 24 kHz, AnalyserNode,
    WebSocket and WebGL2;
  - no permission handler is installed, so mic requests are denied;
  - there is no output-device selection;
  - `echoCancellation` exists; `noiseSuppression` and `autoGainControl` do not.
- **The UI** has no media code and no media `web-sys` features. Trunk copies only `assets/fonts`
  and `assets/vendor` (`index.html`). `ui_scale::tauri_invoke()` bridges to the shell.

## 1. Overview

| Feature | Page ↔ gateway | Server mechanism | Loads |
|---|---|---|---|
| Dictation | `voice/warm` at press, `transcribe` at release | `proxy::transcribe*` | ASR at press |
| Read-aloud of a stored reply | `messages/{mid}/speak` (SSE) | speech pipeline, no writer | TTS at press |
| Read-aloud of a streaming reply | `send {speak: true}`, the text's own SSE | chat turn + speech tee | TTS beside the prefill |
| Realtime mode | `/v1/realtime?chat_thread=<id>` | realtime session; LLM step = chat turn | ASR, chat, TTS at entry |

Opening a chat loads nothing.

## 2. Settings and overrides

### 2.1 The Voice group (Settings → Chat)

| Key | Default | Empty means |
|---|---|---|
| `chat_stt_alias` (exists) | empty | `realtime.asr_alias` |
| `chat_tts_alias` | empty | `realtime.tts_alias` |
| `chat_voice` | empty | no voice named: realtime's chain decides (`realtime.default_voice`, then the row's default; realtime §5.3) |
| `chat_speech_style` | empty | `realtime.speech_instructions` |
| `chat_voice_language` | empty | the language the user speaks; none: the ASR detects (changed 2026-10-04 and 2026-10-05, below) |
| `chat_voice_reply_language` | empty | the language replies are in; empty: `chat_voice_language` as resolved — with neither, the reply follows the user (added 2026-10-05, below) |
| `chat_read_aloud` | `false` | — |
| `chat_turn_detection` | `semantic_vad` | — (`semantic_vad` / `server_vad` / `push_to_talk`) |

- **The keys** are flat `chat_*` keys, each through the seven-file path.
- **`chat_speech_style` is owner-wide, like `realtime.speech_instructions`** (decided after the
  WP4 review, m2): it stands back for a voice-design row that describes itself, as realtime's
  setting does, because a style is not a voice. A thread's own style is the owner's choice for
  that thread and wins over the row's description, as a session's does in realtime (§6.1).
- **Save checks are realtime's:** `asr::is_asr_alias`, `SPEECH_TASKS` for the TTS, and an ISO 639-1
  code for each language (`^[a-z]{2}$`). An empty alias or language is never refused.
- **Two languages: the one the user speaks and the one replies are in** (*changed 2026-10-05:*
  2026-10-04 made the one language authoritative for the ASR, the reply and the TTS at once — the
  bullets below — which misread the owner's need: an owner who speaks German to the ASR can want
  the model to answer, and the voice to speak, in English, and one shared value cannot say that). The
  stored keys and values stay; no migration.
  - **Spoken** — `chat_voice_language`, the thread's `voice.language` (the page's "I speak"):
    the thread's, then Settings'; none at either is none. It goes to the ASR only — dictation,
    the bound session's transcription (`bound::asr_now`, `thread::shape_session`), wherever the
    ASR got the language before — and the prompt states it as a fact ("The user speaks German").
  - **Reply** — `chat_voice_reply_language`, the thread's `voice.reply_language` ("Replies in"):
    what the model is asked to answer in (§8.5) *and* what the TTS is sent (the speech plan, the
    warm, the TTS note, `Announce::for_language`); the model and the voice always share it. It
    resolves thread → Settings → **the spoken language as resolved**
    (`web/chat_voice/resolve/languages.rs`), so one language set the old way still drives all
    three stages exactly as before. `voice_resolved.reply_language` is `{value, source}` with
    `source` `thread`, `chat`, or `speech_in` when it follows the spoken one.
  - **Notes** judge each stage on its own language: the ASR's on the spoken one, the TTS's on the
    reply's. While the two differ, each note opens with the language it is about (*"the reply
    language, English: text-to-speech model '…' …"*); with one language they read as before. The
    page shows the ASR's under "I speak" and the TTS's under "Replies in".
  - The rest of this bullet is the 2026-10-04 text: read "the language" as the spoken one for the
    ASR, and as the reply language for the reply and the TTS.
- **The language is the conversation language** (*changed 2026-10-04 after the owner's test:* it
  was a hint that reached neither the reply nor the voice — German speech got English replies, and
  Supertonic read German text with English pronunciation for its first words). The thread's
  language, else `chat_voice_language`, is authoritative for all three stages when set:
  - **ASR:** sent in the row's own spelling where the row declares a vocabulary — Nemotron ASR's
    prompts, read from its package's `processor_config.json` (`de` stays `de`, `id` goes up as
    `id-ID`); otherwise as it came, as before (`proxy/audio/asr_language.rs`, for every caller, on
    the route the gate admitted — a fallback gets its own spelling). A row that detects the
    language itself (Parakeet TDT) is noted: the setting does not reach it, so forcing German needs
    another ASR.
    - *Changed 2026-10-04 after the review:* a row with a known set gets a note when the language
      is outside it (`audio::language::asr_fit`, `families::hears_language`): its vocabulary, the
      spec's codes where the spec declares a `language` option, SenseVoice's query tokens (it
      detects anything else), Kroko ASR's package language. The note says the engine refuses every
      call where its source does (Nemotron, Canary, Cohere, Hviske, R2T2, Fun-ASR-Nano, Kroko), and
      only that the setting does not reach it elsewhere (Granite 5, Niagara read none). The code is
      still sent as it came: the engine is the judge.
    - *Changed 2026-10-04 after the review:* a bound session's transcription re-reads the thread's
      language when each call starts, as it re-reads the ASR alias (`bound::asr_now`) — a language
      changed since the bind applies from the next turn, as the reply's and the voice's do.
  - **The reply:** the turn's system message says the language (§8.5).
  - **TTS:** a *request*, not a hint (`audio::language::tts_fit`, `SpeechLanguage::Request`): sent
    wherever the row takes a language, in its spelling — Supertonic, Chatterbox and FireRedTTS3
    take exactly their spec's `languages` (`families::takes_language`), Qwen3-TTS its names. A row
    whose accepted set lacks the language is sent none (those engines answer a 500 per clause), a
    family that reads no language (Pocket TTS, Fish S2, …) none either, and Kokoro never: it speaks
    its voice's language and refuses a mismatch. A realtime session's own transcription language
    stays a hint, so stock `/v1/realtime` sessions synthesize as before.
    - *Changed 2026-10-04 after the review (the owner's policy):* `families::takes_language` is a
      complete table of every text-to-speech family of audio.cpp 94bd465 (46), each read off its
      source: the spec's exact set (Supertonic, Chatterbox, FireRedTTS3, Kitten, Piper, Inflect,
      MagpieTTS, Confucius4-TTS, FireRed Audio), the package's table (Qwen3-TTS), any ISO code
      (OmniVoice, Dots, Audio8, VoxCPM1), the English name (MOSS-TTS Local), `options.language`
      as a code (Sopro, Irodori, IndexTTS2) or as the English name (MOSS-TTS v1.5, MOSS-TTSD,
      MOSS-VoiceGen), the voice (Kokoro), the package's one language (Pocket TTS, SanoTTS), `auto`
      only (VieNeu-TTS v3 Turbo), or none (21 families). The language goes in the field the engine
      reads: audio.cpp hands a speech body's `language` to every engine as `text_input.language`,
      never as `options.language`, so the option-only families get `options.language`. A family
      not in the table (one audio.cpp adds later) is sent none, with the note *"lmgw does not know
      whether <family> takes a language; not sent"*; Qwen3-TTS whose `config.json` table could not
      be read is sent none, with a note. Pocket TTS and SanoTTS are noted only when their package
      speaks another language than the setting — Pocket's from its `language` load option, else
      the package's name (audio.cpp loads a package without the option as `english`, which only
      tunes defaults), SanoTTS's from its `config.json`. A stock session's hint still reaches
      FireRedTTS3 and MagpieTTS through their `language` option, as before the table knew them.
  - **Notes, not refusals:** where a stage's model does not take the language as set, the thread
    JSON's `voice_resolved.language_notes` (`[{stage, alias, message}]`) and Settings' own
    `chat_voice_language_notes` (for the saved Chat-level models) say why; the page shows them under
    the language field, and a speech plan logs the TTS's (`web/chat_voice/language.rs`).
    *Changed 2026-10-04 after the review:* the plan logs a note at `info` once per distinct
    (alias, language, note), then at `debug`, and judges the voice the engine speaks (the one sent,
    else its default), as the page does. `chat_voice_language_notes` is in the `SettingsFull` DTO
    (`lmgw_api_types::chat_voice::LanguageNote`), so OpenAPI lists it. A cloud text-to-speech alias
    gets no note: OpenAI's speech has no language field, and it speaks the (already German) text
    as written, so a "does not reach it" note would be true but only misleading.
  - Empty keeps today's behaviour exactly: the reply mirrors the user, the ASR detects, no TTS
    language; realtime's `DEFAULT_VOICE_INSTRUCTIONS` is byte-identical.
- **The UI** is `pages/settings/chat_voice.rs`.
  - It uses realtime's task-filtered alias pickers.
    *Fixed in the WP9 fixes (a WP10 note):* in a column of Settings a picker's source label
    ("audio.cpp") clipped the alias beside it ("audio/nemotron-as"), here and in Settings →
    Realtime alike. The alias now wins the room: the label gives way first, ellipsized to nothing
    in a narrow box (it is in its title, and in the pop), and the name is cut only when it alone is
    longer than the box. One rule for every model picker.
  - The voice picker's state machine moves out of `pages/settings/realtime/voice.rs` into
    `widgets/voice_picker.rs`, so the thread form can use it too.
- **The two ASR chains run in opposite directions, on purpose.** The Chat takes
  thread → `chat_stt_alias` → `realtime.asr_alias`. Realtime keeps `realtime.asr_alias` →
  `chat_stt_alias` (`realtime/asr.rs:20`). Each surface prefers its own key, and either one
  configured serves both.
- **Audio attachments** follow the Chat chain too, the thread's override included. These places
  move to `chat_voice::resolve`:
  - the send's `stt_set` check (`chat.rs:762`);
  - the attachment gate, render, retry and ingest;
  - the `chat_stt_alias` MCP description, which says "Send is then blocked".

### 2.2 Thread and folder overrides

- Migration `0057_chat_voice.sql` adds `chat_threads.voice TEXT NOT NULL DEFAULT '{}'`.
- `store/chat_voice.rs::ThreadVoice` has these fields, all optional (absent = inherit):
  - `asr_alias`, `tts_alias`, `voice`, `language`, `reply_language` (added 2026-10-05, §2.1),
    `read_aloud`, `turn_detection`;
  - `speech_style`, where `""` means none for this thread, as `session.lmgw.speech_instructions`;
  - `seed`, the thread's TTS seed (§6.1).
- **One JSON column, not eight.** Only the voice feature reads these fields. Input is strict.
  Stored values are read tolerantly by `ThreadVoice::from_stored`, which strips unknown keys.
- **Folder defaults.** `ThreadDefaults` gains `voice: Option<ThreadVoice>`.
  - `ThreadDefaults::from_stored` applies `ThreadVoice::from_stored` to the nested object. A newer
    build's unknown key inside `voice` must not fail the whole read; that would wipe every folder
    default through `unwrap_or_default()`.
  - It joins `apply` and `check_defaults` (the recipe in `store/chat_folders.rs`; its comment's
    `ThreadDefaultsForm` is stale, since the UI keeps folder defaults as JSON).
- **`voice.language: "auto"`** (*added 2026-10-04 after the review*): a thread (or a folder
  default) can override a language set in Settings → Chat → Voice with none — the reply follows the
  user, the ASR detects, no TTS language, as with no language anywhere. `voice_resolved.language` is
  then `{value: null, source: "thread"}`. The save check takes `auto` for the thread and folder
  levels (`lmgw_api_types::chat_voice::thread_language`); `chat_voice_language` keeps its rules
  (empty = none). The thread drawer's language box offers it.
  - *Changed 2026-10-05 (the split, §2.1):* `auto` on `voice.language` now means only "the ASR
    detects"; a reply language set in Settings still stands. **`voice.reply_language`** is new,
    with the same shapes (empty inherits, an ISO 639-1 code, or `auto`). Its `auto` passes over
    Settings' `chat_voice_reply_language` and takes the thread's spoken language as resolved —
    its own, Settings', or none — mirroring how `auto` on the spoken language passes over
    Settings. Both `auto` is no language anywhere: the reply follows the user, as before. A folder
    default may carry either.
- **Writes.** `POST /chat/api/threads/{id}/settings` takes `voice` as a whole object; a `null`
  field unsets it.
  - *Built (WP1):* the seed is the one exception to "whole object". It is drawn by the server
    (§6.1), so an object without a `seed` key keeps the stored seed; `"seed": null` clears it.
    `voice: null` clears every override but the seed. An alias is checked (task `asr`, or
    `tts`/`vdes`) only when it changes, so a model deleted since does not block saving the rest.
    The answer carries `voice` and `voice_resolved`.
  - *Built (WP1 review m2):* "keeps the stored seed" means the seed stored when the write lands
    (`store::SeedWrite::Keep`; one `UPDATE … RETURNING`, or under the temporary store's mutex),
    so a seed WP4 draws while the handler checks aliases survives any save; the answer's `voice`
    is what was stored. The page sends a seed only after "New voice" and leaves the server's own
    out of its unsaved check. WP4 draws with a conditional write (only where none is stored), so
    two first uses keep one seed.
  - *Built (WP1 review n1):* a folder's `voice` takes no `seed` (400): each thread draws its own.
  - Temporary threads hold the struct in memory.
  - Keep (`insert_kept_chat_thread`) copies `chat_threads.voice` and each message's `voice`.

### 2.3 Resolution

`web/chat_voice/resolve.rs::resolve(snap, thread) -> VoiceConfig` decides each field, in the order
thread → `chat_*` → `realtime.*` → none. Every route here and the bound session use it. The thread
JSON (single thread: get, create, settings, persist, move) gains `voice` (its own values) and
`voice_resolved`; the thread list carries `voice` only, so a list refresh resolves nothing:

```json
{"asr": {"alias": "…", "source": "chat", "inherited": "…", "local": true, "managed": true,
         "cpu": true, "fallback": null, "fallback_unusable": null},
 "tts": {"alias": "…", "source": "realtime", "inherited": "…", "local": true, "managed": true,
         "cpu": false, "fallback": {"alias": "openai/…", "local": false}, "fallback_unusable": null},
 "voice": {"name": null, "source": "realtime", "inherits": "alba", "note": null},
 "speech_style": {"text": "", "source": "realtime"},
 "language": {"value": null, "source": null},
 "reply_language": {"value": null, "source": null}, "language_notes": [],
 "read_aloud": {"value": false, "source": "chat"},
 "turn_detection": {"value": "semantic_vad", "source": "chat"}, "seed": 1234567,
 "problems": [],
 "realtime": {"ok": true, "code": null, "reason": null, "admin_tools": false}}
```

- `source` is `thread`, `chat` or `realtime`, and the page shows it beside the value.
  `speech_style` is the style the thread's speech uses, so its `source` may also be `row`: a
  voice-design row's own description, where the owner-wide style stands back for it (§6.1). Only
  the row's speech facts tell that, so the thread JSON reads them (`speech::resolve_shown`); the
  thread list resolves nothing, as before.
- `inherited` is what the levels below the thread give: the page's model for a box the owner has
  just emptied.
- **The voice belongs to one TTS model** (WP1 review M1). `voice.name` is the voice callers ask
  for: the thread's own always; `chat_voice` only when the thread speaks with the Chat's own model
  (no thread `tts_alias`, or one naming the same alias as `chat_tts_alias` → `realtime.tts_alias`).
  `realtime.default_voice` is never turned into a name: with no name, `source: realtime` and
  `inherits` report it, callers send no voice, and realtime's chain (realtime §5.3, rule 3) decides
  with its known-voice checks. `note` says why a skipped `chat_voice` does not apply ("chosen for
  the text-to-speech model '…'"). The settings page shows "inherits <realtime voice>" for an empty
  `chat_voice` only while `chat_tts_alias` is empty or equals `realtime.tts_alias`.
- `local: false` marks an alias served off this machine: a provider, or a server elsewhere (the
  page says "remote").
- `fallback` is the alias the GPU hold would answer with. The page reads the hold from the
  titlebar's `vram` frame. With the hold on, a GPU row that has a fallback shows "under the GPU hold
  this goes to openai/… (remote)" **before** anything is recorded or spoken.
  `fallback_unusable {alias, why}` names a fallback the row sets that cannot stand in (the hold
  then refuses, naming it).
- *Built (WP7):* with the hold on, the status line above the composer says it whenever it applies,
  before anything is pressed — "dictation: under the GPU hold this goes to openai/… (remote)", and
  "read-aloud: …" while the thread reads its replies aloud — and the microphone (and the toggle)
  turn amber; their tooltips say it too, and that a remote alias is remote.
- *Fixed after the WP7 review (M2):* **a benchmark run's lease is a block like the hold.** It swaps
  a request at resolve time exactly as the hold does (`resolve_for_request`, `FallbackReason::
  Benchmark`), and it blocks CPU rows too (`gpu_block_for`). So `fallback` / `fallback_unusable` are
  filled for every local row lmgw runs, CPU rows included; `cpu: true` still tells the page the
  hold leaves the row alone. `managed` marks a model lmgw runs in a container of its own (the only
  kind a block applies to: not an alias off this machine, not a local server lmgw does not run).
  The page reads the block from the titlebar's `vram` frame — `hold_active` (GPU rows) and
  `benchmark` (every local row) — as one `Block` (`pages/chat_voice/state.rs`): "while a benchmark
  run holds the GPU this goes to …", the same amber on the microphone, the voice menu and the
  speaker buttons, and a `gpu_benchmark` refusal is the amber hold-like chip, never a red error
  (review m1). Said up front too (review m5): a blocked row with no fallback at all ("… this is
  refused: it has no fallback"), and, before the first `vram` frame, what the hold *would* do, in
  the tooltip only ("if the GPU hold is on, this goes to …"; no amber, no status line, so a page
  load does not flicker). A warm's `fallback` frame for `asr` while the microphone is open (the
  outside-VRAM verdict) turns the microphone amber and adds "Esc discards the recording" to its
  note. A remote ASR gives the microphone a small cloud mark (review m6).
- `problems` lists what blocks a feature, such as a missing or vanished alias. Each entry is
  `{stage: "asr"|"tts", code: "not_configured"|"unresolved", message}` (built in WP1). The drawer
  shows `unresolved` as a warning and `not_configured` as one quiet line; WP7 shows the blocker
  where a voice feature is pressed.
  - *Added after the owner's OmniVoice test (2026-10-04):* a third code, `voice_needs_transcript`
    (stage `tts`, the thread JSON's `resolve_shown` only): the voice resolves to a voice-library
    clip with no `prompt_text` line, and the TTS row's engine cannot clone a clip without one.
    audio.cpp hands the engine the clip's transcript from that index, and OmniVoice answered 500
    "native voice clone currently requires reference_text" after the press. The catalog spec is
    no guide (OmniVoice and Qwen3-TTS declare no options, CosyVoice3 declares `reference_text`
    and speaks without), so the requirement is a family table read off audio.cpp's source
    (`audio::families::clone_requires_transcript`: OmniVoice, Fish Audio S2, Qwen3-TTS Base, and
    the other families that throw; not VoxCPM2, which clones its voice reference without text).
    The message names the clip, the model and the fix ("transcribe it in the Audio lab … or pick
    another voice"); the drawer shows it as a warning with an "Open the Audio lab" link, and
    read-aloud and voice mode refuse with it before the press. Nothing is transcribed on the
    owner's behalf, and no other voice stands in.
- `realtime` carries §8.1's refusal for the voice button, with `code: "chat_thread_admin"` (the
  bind's own 409 code), and `admin_tools` (the self-admin toolset attached while `self_admin` is
  `full`) for §9.4's chip.
- **The voice list is read on demand.** The Voice box asks `GET /v1/audio/voices` when it is
  focused, not when the drawer is drawn, and keeps each alias's list for the page's life: under
  the hold a local row with a provider fallback is answered by the provider.

### 2.4 Per-window device settings

Input, output and echo mode live in the window's `localStorage`, because device IDs belong to one
browser profile:
- `lmgw.voice.input` and `lmgw.voice.output` hold `{id, label}`;
- `lmgw.voice.echo` holds `device` | `browser` | `not_needed` | `none` (default `device`, §12).

A stored device is matched by id, then by label, else the system default is used and a note says
so.
- *Built (WP6):* `pages/chat_voice/audio/devices.rs` (`resolve`, tested natively). Before the first
  grant WebKitGTK hides ids as well as labels (§13.1), so nothing can be matched yet: the list says
  "Device names appear after the microphone is first used in this window", a stored input is asked
  for as `deviceId: {ideal}`, and once the grant shows the list the capture resolves it again and
  reopens on the stored device if a different one was opened. Chrome's `default` and
  `communications` entries are left to the page's own "System default" row.
- *WP6 fix:* a placeholder name the page numbered ("Microphone 2", a browser that hides labels) is
  stored as an empty label, so it never matches another unnamed device later (review n3). A choice
  another tab of the same browser profile stores is taken over at once (the `storage` event,
  review n11).

## 3. Storage of spoken turns

Migration 0057 also adds `chat_messages.voice TEXT`; NULL means a typed turn.
`store/chat_voice.rs::MessageVoice` holds:
- **on a user message:** `{via: "dictation"|"realtime", asr, asr_answered_by, asr_ms, audio_ms}`;
- **on a spoken reply:** `{via: "realtime", tts, tts_answered_by, voice, unheard, timing}`. The
  chat model's own fallback stays in the row's `answered_by`.

What the page and the store do with it:
- **Heard and unheard.** An interrupted reply keeps the heard part, as written, in `content`; that
  is what the model sees (§8.4). The rest is in `voice.unheard`, rendered after the content, greyed,
  behind a "not heard" marker.
- **Search** indexes `content` only, so unheard text is not found.
- **Markdown export** writes "## You · spoken", and the unheard rest as a quoted "(not heard)"
  paragraph. **JSON export** carries `voice`.
- **Edit, continue, regenerate.**
  - Editing a reply clears `voice.unheard` and `voice.timing`.
  - Editing a dictated user message so its text changes drops its `voice` (decided on WP1 review
    m8): the text is no longer what was spoken, so it is a typed turn, with no mic badge and
    no ASR timings. A resend with the text unchanged keeps it.
  - Continuing a cut reply clears `voice.unheard`; the continuation follows the heard text.
  - Regenerate deletes the row, as today.
- **Badges.** A spoken user turn shows a mic badge. A spoken reply shows a speaker badge and the
  timing line (§9.5). Transcripts are ordinary messages, so search and export include them.
- *Built (WP7):* `pages/chat_voice/badges.rs`, `spoken.rs`, `cues.rs`. The mic badge's tooltip
  names the ASR, a fallback that answered and the times; a spoken reply's unheard rest is rendered
  after its content behind a "not heard" marker, in the muted text colour; the timing line opens to
  every stage and the cold starts. **Delivery cues and sounds as chips** (the owner's call): in a
  reply spoken in voice mode (`via: realtime`) a `[laughing]` renders as a small chip, not raw
  brackets — the gateway's own tag grammar (`lmgw_api_types::realtime::is_tag_name`, which
  `audio::tags` now calls), outside code, links and math; a typed or read-aloud reply's brackets
  stay text. A message's `voice` is read tolerantly (a shape this build cannot read is a typed
  turn, not a thread that fails to load). Editing a dictated message so its text changes drops
  its badge at once; the thread read back after the turn has the stored `voice`.
- *Fixed after the WP7 review:* the unheard rest is the secondary text colour (`--text-2`, 6.3:1
  dark and 5.5:1 light, where `--text-3` was 3.4:1) in italics behind a dashed rule — muted, never
  pastel (m8). **A single letter in brackets is no tag** (m12): `is_tag_name` now takes
  `[a-z][a-z _'-]{1,30}`, so `[x]` (a task list's checkbox) and `[a]` (an enumeration) are spoken
  and shown as written, by the gateway and the chips alike — one rule, still in `lmgw_api_types`.
  `[1]` never was one. The other stage-direction shapes the gateway reads (`(laughs)`, `*sighs*`)
  stay text in the bubble: only the canonical `[tag]` is a chip.

## 4. Model loading on first use

### 4.1 What loads when

| Press | Loads | How |
|---|---|---|
| Mic, or hold-to-talk key down | ASR | `POST …/voice/warm {stages: ["asr"]}`, Admit |
| Speaker on a reply | TTS | the `speak` request itself |
| Send with read-aloud on | TTS | Background warm at turn start, beside the prefill |
| Enter realtime mode | ASR, chat, TTS | the bound session's connect warm, Admit as a group (§4.2) |
| `speech_started` inside a session | the same | realtime's Background warm; a bound session re-reads the thread for it (§8.2) |

### 4.2 An explicit press may evict

**Decision.** A press warms through the request admission (`gate::resolve(…).admit()`), so it may
evict idle models exactly as the request it announces would. It holds the claim while a lazy audio
row loads (realtime's load request, sent on that claim instead of `vram::join`), then drops it. A
Background warm keeps `check_background_start` and never evicts.

**Why.** The request follows the press within seconds, and it would evict the same models. A
non-evicting warm would skip, and then the eviction and the cold start both land inside the
user's wait.

**Several stages warm as one group, so they cannot evict each other:**
- the group's footprint is checked first, with `realtime_budget`'s sizing
  (`VramScheduler::footprint`) against what lmgw may use;
- if it fits, the stages are admitted side by side, and every claim is kept until all of them are
  up. A sibling cannot evict a stage that has just loaded, nor the chat model the turn is about to
  claim;
- if it does not fit, the group reports `skipped: does_not_fit` with the sizes and falls back to a
  Background warm. It never plays eviction ping-pong.

The read-aloud TTS warm stays Background: the chat turn beside it is the real request.

**Bounds.**
- Under the GPU hold the admission answers as for any request: the alias's fallback, or
  `gpu_hold`. A warm never starts a fallback's model. It reports `held` or `fallback`.
- The benchmark lease and `gpu_block` are the admission's own rules.
- A press that ends before admission (key released, Esc, thread left) drops the wait. A container
  start already in flight finishes, as realtime §9.1 says.
- `Warmer::warm` gains `WarmMode::{Admit, Background}`; there is no second warm module.
- *Built (WP3):* `realtime/warm.rs` with `warm/admit.rs`, `warm/background.rs`, `warm/load.rs`
  and `warm/outcome.rs`; `warm_group(state, label, mode, models, reporter)` is the one entry,
  and the realtime session's own warm stays Background.
  - **The group rule** applies to two or more *distinct local GPU* models (a candidate alias
    counts as its primary; a CPU audio row, a cloud alias and a model two stages share do not
    add). Their footprints plus `vram.headroom_mb` are compared with the capacity
    (`vram.budget_mb` or the devices' total) less the memory held outside lmgw on the card now,
    as `realtime_budget`'s `too_large` and `tight` verdicts do. The outside figure comes from
    the same ledger read: the devices' used bytes less the charges of lmgw's containers on the
    card (`VramScheduler::group_capacity`); it is left out when it cannot be told (no device
    reading, or a container with no figure). There is no group decision — every stage is
    admitted — under the hold or a lease, with admission off, with no capacity figure, or when
    a model has no footprint (nothing is guessed).
  - *Fixed after the WP3 review (B1):* the check alone cannot keep a group from waiting on
    itself. Footprints are lower bounds, and the card changes after the check (a game starts).
    A stage could then wait for room only its siblings' kept claims could free, and a waiting
    admission holds the global gate: every cold start in lmgw waited with it, until
    `vram.queue_timeout_seconds` (or for ever, with 0). **A stage never waits on its own
    group** (`realtime/warm/kept.rs`, `vram/beside.rs`):
    - once a sibling keeps a claim on the card, the waiting stage asks
      `VramScheduler::crowded_beside` whenever another sibling settles and every `vram::POLL`
      after. The question: do the free memory and every model it could evict (every one on the
      card but the kept ones, busy or not; busy ones finish) cover its footprint plus the
      headroom?
    - if they do not, it gives up as `skipped: does_not_fit`, naming the siblings and the sizes.
      The claims go with the group, and its request makes room as any request does;
    - a stage that waits for a busy model outside the group keeps waiting, as its request would;
    - nothing is dropped half-way: a start of its model in flight is awaited, and while a
      container is `stopping` (an eviction the admission may be making) the drop waits for it.
    - not "once every sibling has settled", the review's first sketch: with three GPU stages
      two can wait at once — one holding the gate, the other queued behind it — and neither
      settles; and a stage that waits for a busy model outside the group would give up for
      nothing.
    - letting the siblings' claims go instead re-allows the ping-pong this section rules out, so
      it is not taken.
  - **A press that ends** drops a stage's wait unless a start of its model is in flight (its
    registry entry is `starting`): then the admission is awaited to its end, so the registry's
    start is never dropped half-way, and its claim is let go without a load. A drop also waits
    out a container that is `stopping`. *(The WP3 build also counted `ready`; admission on a
    ready entry never parks, so that only kept a stage queued at the gate after the press
    ended.)*
  - **A bound session's connect warm (WP8)** uses the same path: `Warmer` owns a stop handle
    whose signal its `Reporter` carries (`Reporter::until`), so a session that ends drops its
    stages' waits the way a released key does. No path of the Admit warm can then hold the gate
    longer than the request it announces would.
  - **Background reports `fallback` too** when the hold refuses a start and the alias has a
    usable fallback, so both modes say what would answer.
  - The lazy row's load goes on the admission's claim (`proxy::transcribe::warm::load` and
    `proxy::synthesize::warm::load` take a borrowed claim); Background keeps `vram::join`.

### 4.3 Model state

`warm_one` returns a `WarmOutcome` instead of only logging it. The turn paths report the same shape
before each admission whose model is not resident (`state.runtime().contains`; for audio, also
`audio_model_loaded`). *Fixed after the WP3 review (m1):* resident means up and `ready`
(`Registry::ready_port`); a container `starting` for another caller or `stopping` is a wait,
and says `loading` too:

```json
{"stage": "asr", "alias": "…", "state": "loading", "ms": null}
```

| `state` | Carries |
|---|---|
| `loading` | — |
| `ready` | `ms`, the load time |
| `held` | `cause`: `gpu_hold` or `benchmark`, from the `gpu_block` reason |
| `fallback` | `answered_by` |
| `skipped` | `reason`: `full` (Background), `does_not_fit` (group), or `cannot_speak` (`refuse_route`) |
| `failed` | the message |

A CPU row is never held by the GPU hold, but a benchmark lease does hold it (realtime §9.2).

The shape appears as:
- the `state` frame of the chat SSE streams. A plain text `send` gains it too, which is a wire
  change; the old page ignores unknown events (`chat_stream.rs:77`);
- `lmgw.model.state` in a bound session.

*Built (WP3):*
- `ready` carries `ms: null` when nothing had to load: the model was up and loaded already, or
  it is served off this machine (a Background warm of a candidate alias, too).
- `held` and `skipped` also carry `message`; `skipped: does_not_fit` carries `needed_bytes` and
  `capacity_bytes` beside the sentence that names each model's size. `capacity_bytes` is what the
  group could have together: at the group check, what lmgw may use less what is held outside
  lmgw (when that can be told); for a stage crowded out later (§4.2), the free memory plus every
  model it could evict plus what its kept siblings hold.
- **`does_not_fit` from the group check is not a stage's last frame**: the group is then warmed
  in Background, so each of its GPU stages says `does_not_fit` and then its Background outcome
  (`skipped: full`, or `loading` → `ready`, …). A reader takes a stage's last frame as its state
  and `done` as the end of the warm. A stage crowded out later says `does_not_fit` last.
- A chat turn (plain and tool threads) says the `chat` stage only around an admission whose model
  is not up and ready (absent, starting, or stopping): `loading`, then `ready` with its time, `fallback` (the outside-VRAM
  verdict) or `held`/`failed` (its `error` frame follows as before). A route the hold already
  swapped, a cloud route and a candidate alias (its pick is made at admission) say nothing.
- *Fixed after the WP11 server review (M1):* the GPU hold and a benchmark's lease refuse a local
  model with no usable fallback at the route's resolve, before any admission; that refusal says
  `held` too (`cause` `gpu_hold` or `benchmark`), on plain and tool threads, so a hold is the amber
  chip on every path. Every turn `error` frame built from a gateway error carries `code`
  (`GatewayError::code`: `gpu_hold`, `gpu_benchmark`, `context_length_exceeded`,
  `vram_queue_timeout`, `key_budget`, `unknown_alias`, `upstream`, …), an additive wire change
  (the `upstream_error`, `tool_upstream_error` and `unknown_model` goldens show it). A frame with
  no gateway error behind it (a stream that broke mid-way, a thread with no usable tool) has
  none. A bound response fails with the turn's gateway error itself (§8.2), so its client gets
  the code, type and message a stock session sends for the same refusal.
- *Fixed after the WP11 UI review (m6):* the page words a refusal by its code
  (`state::turn_refusal`), what to do first and the gateway's message after it, on the
  composer's line under a text turn (the reply bubble keeps the message) and on voice mode's
  line: `gpu_hold` and `gpu_benchmark` the amber chip, `context_length_exceeded` "this
  conversation no longer fits the model's context: start a new conversation, or delete old
  turns", `vram_queue_timeout` a warning to say (or send) it again, `key_budget`,
  `unknown_alias`, `upstream`, and `asr_not_configured` on a failed transcription (it names the
  STT chip and Settings → Chat → Voice). The refusal is said under the chat stage's key, so it
  replaces that stage's `held` (or `failed`) note, and a new turn clears it. **One hold, one
  chip:** a line shows the newest of its hold notes (the connect warm says `held` for each GPU
  stage, a press's warm and its refusal for the ASR), its ✕ dismisses them all
  (`status::one_hold`).

The waiting control shows "loading nemotron-asr…". A hold is an amber "GPU hold: local voice
paused" chip, never an error, and it names the stages that still serve on the CPU.

### 4.4 Fallbacks are taken as configured

The mic's audio goes wherever the ASR alias's route goes, its GPU-hold and outside-VRAM fallbacks
included, like any request; so does the barge-in word check of a bound session.
- What served is always visible: before recording (§2.3), in `asr_answered_by`, and in the
  models event and badge.
- `transcribe_local_only` stays what it is: the rule for voice-library clips only.

## 5. Dictation

- **Control.** A mic button in the composer row: a short click toggles recording, press-and-hold
  records until release.
  - The hold-to-talk key is **Right Ctrl** (`code == "ControlRight"`), anywhere on the Chat page.
  - Key repeats are ignored. Another key pressed meanwhile cancels the recording, so Ctrl+C stays
    Ctrl+C.
  - Esc while recording discards the audio.
- **Press:**
  1. resume the playback context (this is the user gesture) and stop any read-aloud;
  2. open the mic with the window's device and the echo mode's constraints (§12.1);
  3. start the ASR warm (§4.1); its `state` frames drive a "loading …" note;
  4. capture 16 kHz mono PCM16 to memory (§11.2), showing a timer and a level meter.
- **Release:**
  1. stop the tracks;
  2. encode a WAV and `POST /chat/api/threads/{tid}/transcribe`. The body is `audio/wav`; the answer
     is `{text, alias, asr_answered_by, asr_ms, audio_ms, language}`;
  3. insert the text at the caret, focus the composer, and mark it as dictated. Enter sends, as
     today.
- **Body limit.** The route gets `DefaultBodyLimit::disable()` and reads its body through
  `read_upload_body`, bounded by `max_body_mb` (0 = no bound) like attachments. Without that,
  axum's 2 MiB default would cut dictation at about 65 s.
  - The page checks the size before uploading and names the setting: "41 MB recorded, over
    max_body_mb = 32 MB".
  - The page sets no recording cap of its own.
- **The send** carries `voice: {via: "dictation", asr, asr_answered_by, asr_ms, audio_ms}` while
  the composer holds dictated text, edited or not. Clearing the composer drops the mark.
- **Errors** show in the composer row:
  - `asr_not_configured` names Settings → Chat → Voice;
  - `gpu_hold` shows the hold chip;
  - anything else shows its message.
- **Admin Chat threads keep dictation**, as they keep read-aloud. Dictated text waits in the
  composer for Enter, exactly like typed text, and read-aloud only speaks text already shown. Ruling
  7 is about turns that run without that step.
- *Built (WP7), the page:* `pages/chat_voice/dictation.rs` (+ `dictation/upload.rs`), `keys.rs`,
  `controls.rs` (the microphone), `status.rs` (the status line above the composer).
  - A press held at least 350 ms ends at its release; a shorter one is a click that toggles (the
    keyboard's activation of the button toggles too).
  - **Right Ctrl's warm is armed** (`KEY_WARM_ARM_MS`, 300 ms): the microphone opens at once, the
    warm only if no other key came meanwhile, so Right Ctrl used as a modifier (Right Ctrl+C)
    loads nothing — the Admit warm may evict. A pointer press warms at once. Ctrl+wheel (the app's
    zoom) cancels like another key; the window losing focus while the key is held ends the
    recording as a release would (its key-up never comes). The key is the constant `DICTATE_KEY`.
    *(Superseded after the WP7 review: nothing opens until the arm time is over, below.)*
  - **What is not uploaded is said:** a capture that ended (`CaptureEvent::Ended`: the reason, "nothing
    was sent"); a recording of nothing but digital silence (a muted track); an empty one; one over
    `max_body_mb`, read from `/api/settings-full` at the release ("41.0 MB recorded, over
    max_body_mb = 32 MB (Settings → Network → Max request body): nothing was sent"). A track the
    system mutes is said while recording.
  - The text replaces the selection or lands at the caret, with a space where it would touch a
    word; the composer is focused with the caret after it. Several dictations into one message add
    up in the mark (`asr_ms` and `audio_ms` summed, `audio_ms` unknown once any part's is; a
    fallback that had any part stays named). The composer's tooltip carries the mark's facts
    (§9.5's "`asr_ms` on the inserted text's tooltip"), and a "dictated" chip on the status line
    drops the mark on ✕.
  - A transcription error shows its message, and with `asr_answered_by` (or the `x-lmgw-fallback`
    header) "the audio went to <alias>, which answered in its place"; `gpu_hold` is the amber hold
    chip. The warm's `state` frames are status notes ("loading …", "… loaded in 3.0 s", the hold, a
    fallback). Esc, another key, a thread switch and leaving the page discard; the microphone is
    released on every path.
- *Fixed after the WP7 review:*
  - **Right Ctrl opens nothing until it is armed** (M1, m3). The key is matched by position *and*
    meaning (`code == "ControlRight"` and `key == "Control"`; not during an IME composition), so a
    Right Ctrl the layout remaps — KDE's Compose key, a layout switch, a third-level shift — is not
    dictation; AltGr (`AltRight`/`AltGraph`, EurKEY) never was. A press is `Arming` for
    `KEY_WARM_ARM_MS` (300 ms): no microphone, no warm, no note, no playback context. Another key,
    a pointer press or drag, or Ctrl+wheel within it is a shortcut (Right Ctrl+C, Ctrl+click) and
    cancels without a word; a release within it records nothing and says "nothing was recorded:
    hold Right Ctrl until the red dot shows, then speak". Past it the microphone opens and the warm
    starts, and a later other key, pointer press or wheel discards with a note, as before. This
    replaces WP7's "the microphone opens at once, the warm only if no other key came": a stray tap
    uploaded a few milliseconds of noise, which whisper-family models turn into invented text.
  - **A release before the microphone opened uploads nothing** (M1): a held press let go while the
    microphone still opens (or a second click then) is discarded with "the microphone was not open
    yet: nothing was recorded — hold until the red dot shows"; the open that lands later is
    stopped.
  - **The playback context is made on a pointer press only** (and the button's keyboard
    activation), never on Right Ctrl: a modifier-only key press is no user activation in Chrome or
    Firefox, so it used to leave "the browser did not let lmgw's playback start" behind a
    dictation that played nothing (m4). Any read-aloud stops when the microphone opens.
  - **Sending or playing while dictating** (m2): Enter (or Send) with Right Ctrl held cancels the
    dictation and sends, as any other key keeps its effect; with a button-started recording Enter
    finishes the dictation (its text lands in the box) and does not send — "Enter again sends";
    while it still transcribes Enter waits with a note. A speaker button, a turn read aloud
    (`speak`) and the devices popover's test tone finish an open microphone first (its tracks stop
    at once) and discard one still opening, so nothing plays into it.
  - A transcript of a thread left in the moment before the switch was seen is discarded (m9).
    `max_body_mb` is read while recording, not at the release (n4). A held press says "release to
    transcribe" once 350 ms have passed (n3). The size note says MiB and "Settings → Network &
    access → Max request body" (n1); a failed fallback is "the audio went to X (in place of Y),
    which failed" (n2); "heard no words" names the recording's peak in dBFS, as information (n11).
    No space is put inside brackets and quotes (n5). Esc leaves an open `<dialog>` its own Esc
    (n10).
  - **Dictation targets the main composer** (n12), also while the in-place message editor has
    focus: the composer is the page's, and an edit is not where a new turn is written.
- *Changed after the WP11 UI review (m4, NIT 10):*
  - **Right Ctrl's warm waits for a dictation.** It is an Admit warm, which may evict an idle
    model, and the owner's Right Ctrl is a plain Ctrl too: held past the arm time while reaching
    for the second key (Right Ctrl+Shift+…), the microphone opened and the warm went out. The
    warm now goes once the recording holds speech (an RMS level of −40 dBFS for 120 ms,
    `SPEECH_DBFS`/`SPEECH_HOLD_MS`) or the microphone was open a further `KEY_WARM_ARM_MS` with
    no other key. A slow combination still opens the microphone (and the other key closes it,
    nothing sent), but warms nothing. A pointer press warms at once, as before.
  - **Esc while Right Ctrl arms** is a combination like any other: nothing was opened, so it
    cancels without a word and keeps its own effect (an open popover closes), where it used to
    say "dictation discarded" and swallow the Esc.
- *Built (WP3), the backend:*
  - `POST …/voice/warm` takes `{stages: […]}` of `asr`, `tts` and `chat` (the thread's own
    model; the page sends `asr` only, the rest serves the group rule and WP8). The answer is
    SSE: `state` frames, then `done {}`. Refusals before anything warms: `422
    asr_not_configured` / `tts_not_configured` (the resolution's problem message, which names
    Settings → Chat → Voice), `400 bad_request` for an empty or unknown stage list. A `tts` stage
    loads with the voice realtime's chain resolves for the thread's voice (or none named), the
    thread's language (its reply language since 2026-10-05, §2.1), and its stored seed, unpinned;
    no resolved voice starts the container only.
  - `POST …/transcribe` answers `language` as the upstream's own `language` when its JSON has
    one, else the thread's hint — the spoken language since 2026-10-05, §2.1 — (which goes up as
    the `language` field); `audio_ms` from the WAV
    header and data size, nothing decoded (`null` for any other container, or a body it does not
    read as PCM WAV — it is sent all the same); `asr_ms` is the whole call, admission included.
    `language` is whatever the upstream says: the hint is ISO 639-1 (`de`), while OpenAI's
    `verbose_json` names the language (`german`) — it is passed on as it came, not mapped.
    `400 empty_audio`; a transcription error keeps its own status and code (`503 gpu_hold`).
    *Fixed after the WP3 review (m2):* the body's `Content-Type` names its container — WAV (also
    no type, or `application/octet-stream`), WebM, Ogg, MP3, M4A or FLAC — and it goes up under
    that type with a matching file name (`dictation.webm`, …), since OpenAI's Whisper reads the
    format from the extension; any other type is `415 unsupported_media_type`. *Fixed after the
    WP3 review (m5):* the call runs in its own task with a stop the handler holds, so a page that
    aborts the upload stops it at its next await and its row is still written, `canceled` (the
    WP2-M1 rule). *Fixed after the WP3 review (M1):* an error body is
    `{code, message, asr_answered_by}`, and every answer carries the gate's `x-lmgw-fallback`
    headers: a fallback that had the audio is named even when it failed, since the audio may
    have left the machine with no text back. The request row has said so all along
    (`fallback_reason`). A body over `max_body_mb` is `413 body_limit`, whose message
    names the setting and its value. The row is the Chat's, labelled as the thread's model turns
    are (`chat`, or `admin` on an Admin Chat thread; see §6.1), and a warm writes none.

## 6. Read-aloud

### 6.1 Server-side speech, by realtime's pipeline

**Clauses are cut and voiced on the server**, by the ~7k tested lines in `realtime/`. A wasm copy
would drift on the first fix. `web/chat_voice/speak.rs` drives `realtime::responder::speech`, with
these changes:

- **No writer.** `Speech.progress` becomes `Option<Progress>`, and read-aloud passes `None` with
  `ahead: None`.
  - There is no back-pressure and no stall rule (`Room::wait` would count a writer with no playing
    window as stalled). Nothing holds synthesis back, and the TTS claim ends with the last clause.
    It still gathers sentences into batches by its own estimate of the page's playback (realtime
    design §8.2, 2026-10-05).
  - Memory is bounded by the reply itself, which the no-hidden-caps rule allows.
- **Labels.** `synthesize::open` and its `log` take a `ClientProto`. Read-aloud and dictation rows
  carry the Chat's in-process label, not `realtime`.
  - *Changed after the WP3 review:* that label is `chat` (`ClientProto::Chat`), the one the Chat's
    model turns carry, not `openai` (`ClientProto::OpenaiChat`, what `transcribe_as` uses for an
    attachment's transcript). Logs tell the Chat's voice traffic from `openai` clients, and its
    cost lands on the same internal identity (`internal:chat`) as its turns.
  - *Changed after the WP4 review:* an Admin Chat thread's speech and dictation rows say `admin`
    (`ClientProto::AdminChat`), as that thread's model turns do, so they are charged to
    `internal:admin-chat` with them (`chat_voice::speech_proto`).
  - *Changed after the WP11 server review (n5):* an attachment's transcript (at upload, at
    render, on retry) is labelled the same way (`proxy::transcribe_for` with `speech_proto`), not
    `openai`.
- **The seed.** A row that designs its voice, or draws a speaker per request, changes voice from one
  clause to the next without a seed (`synthesize::sends_seed`).
  - The thread's `voice.seed` is drawn and stored on first use, unpinned in `sends_seed`'s sense.
  - Read-aloud and the bound session use it, so one thread keeps one voice. The Voice section
    shows it with a "new voice" button that draws another.
- **The rest is the session's resolution.** The voice goes through realtime's chain (realtime §5.3) as if a
  session had asked for it. The style becomes `instructions`, and the language is passed on.
  *Changed 2026-10-04 after the owner's test:* the language is a request (§2.1): every clause is
  sent it wherever the TTS row takes one, in the row's spelling, the press's warm too.
  *Changed 2026-10-05:* it is the thread's reply language, not the spoken one (§2.1).
- **Visibility.** `Speech`, `Splitter`, `speak`, `Msg` and `Written` become `pub(crate)`. Their log
  lines take a caller label instead of a session id.
- *Built (WP4):* `web/chat_voice/speech/plan.rs` and `speech/run.rs`.
  - `Speech` gains `label` (the log prefix: `realtime <sid>` or `chat thread <id>`, also for
    `Synthesis`, settle, the room and `voice::facts`) and `proto`, under which the speaker's
    per-call key check and its row run. `Room` with no `Progress` always has room.
  - The speaker reports its route as `Msg::Tts(TtsEvent::Opening | Opened {answered_by,
    voice})`; a realtime session ignores it, the Chat turns it into `state` and `voice` frames.
  - Refused before speaking, as a `speech_error` frame: `tts_not_configured`,
    `voice_not_configured`, `voice_not_found`, and `instructions_required` (a voice-design row
    with no description anywhere). The voice facts carry `designs`, as a session's do.
  - *Added 2026-10-04:* and `voice_needs_transcript` (§2.3), from the voice facts'
    `untranscribed` clips (`speech/clip.rs`); a bound turn's plan refuses with the same code.
    Below the Chat, every TTS route that opens for a voice checks it before admission
    (`synthesize::refuse_route` with the clause's voice, so a realtime response and either warm
    start nothing for it), `POST /v1/audio/speech` in its preflight, and `GET /v1/audio/voices`
    says `lmgw.needs_transcript`, which the Voice box uses to mark such clips. When audio.cpp
    still answers its own 500 for it (a family the table does not list yet), the error is worded
    as `voice_needs_transcript` too (`audio::engine_errors`), as is "Could not load eSpeak-ng"
    (`audio_image_lacks_espeak`: the image predates the `runtime-espeak` build edit; rebuild it
    on the Backends page). The image's labels do not record its build's edits, so the eSpeak case
    is said when it happens, not before. The page words both codes with a lead and a link to
    where they are fixed (`state::speech_lead`, `refusal_link`).
  - *Changed after the WP4 review (m2):* the style is realtime's precedence with the thread's own
    style as the session level and the owner-wide style (`chat_speech_style`, else
    `realtime.speech_instructions`) as the setting (`speech::style_of`). The owner-wide style
    therefore stands back for a voice-design row that describes itself, in the Chat as in
    realtime; the thread's own style wins over the row's description. It was Settings → Chat's
    style at the session level, which redesigned such a row's voice in the Chat only.
  - The seed: `store::draw_chat_thread_seed` is one `UPDATE … WHERE` no `u32` seed is held
    `… RETURNING`, then a read of the one that is; the temporary store draws under its lock.
  - WP3's `voice/warm` `tts` stage now resolves its voice and style through the same
    `voice_of` / `style_of`.
  - *Changed after the WP4 review (m7):* it also draws the thread's seed on first use
    (`speech::thread_seed`, the same conditional write), so a row whose voice comes from its seed
    warms the voice the read-aloud then speaks with.
  - *Changed after the WP4 review (n4):* an announcement's info word is said as words (`c++` is
    "c plus plus", `c#` "c sharp"): the announcement goes to the TTS past the speakable pass.

### 6.2 Code blocks and tables

- **Tables are skipped.** `realtime/clauses/blocks.rs` gains table blocks.
  - A line that starts with `|` (after up to three spaces) is a table row, judged once the line is
    whole. The rows are skipped like fenced code.
  - *Changed after the WP4 review (m1):* a table starts only at a `|` line that also ends with `|`
    or is a delimiter row, or at one followed by a GFM delimiter row (a header without its closing
    pipe); every `|` line after it continues the table. Any other `|` line ("|x| ist der Betrag",
    "|| true") is text: it waits for the line after it, one line of latency for such a line alone,
    and is judged at the end of the stream if that line never comes.
  - A table without a leading pipe is not detected; the module doc says so. Nor is one inside a
    list item indented four columns or more (it reads as indented code).
  - *Built (WP4):* the row test lives in `realtime/clauses.rs`'s `find_boundary` (the scan) with
    the line tests in `blocks.rs` (`row_closed`, `delimiter_row`), not in a module of its own.
  - This applies to every speaking response, including stock `/v1/realtime` sessions. It is a
    behaviour change and gets a DocRoute line and a release note.
- **Chat voice announces what it skips**, internally: there is no public knob.
  - The Chat callers (read-aloud and bound sessions) pass `Splitter::new(…, Some(Announce))`, which
    emits one clause where a skipped block starts: "Code block." or "Code block, rust." for a fence
    with an info string, and "Table.".
  - German gets "Codeblock." and "Tabelle.". The text comes from a small visible table keyed by
    the voice's language, with English as the fallback.
  - The clause's `written` is marked an announcement and holds only what was written before its
    block; the clause after it carries the block. Heard whole, the history is what it would be
    without the announcement. A cut is §8.4's rule.
    *Changed after the WP4 review (M2):* it was an empty `written`, which left a cut inside an
    announcement writing its words into the history.
- *Built (WP4):* `realtime/clauses/announce.rs` (`Block`, `Item`, `Announce`) and
  `ClauseAggregator::push_items` / `flush_items`, which report a fence where it opens (its info
  string's first word) and a table at its first row.
  - A row the stream ends inside is a row. A blank line, text, a list item or a fence ends a
    table; the next row starts a new one.
  - The words exist for `en` (the fallback), `de`, `fr`, `es` and `it`, keyed by the thread's
    language hint.

### 6.3 A stored reply

`POST /chat/api/threads/{tid}/messages/{mid}/speak` answers SSE:

| Frame | Data |
|---|---|
| `state` | §4.3, only while the TTS is not resident |
| `voice` | `{tts, voice, tts_answered_by}` once the route is open |
| `speech` | `{seq, text, pcm}`: the clause as said, and base64 PCM16-LE 24 kHz mono |
| `speech_error` | `{code, message}` (`tts_not_configured`, `voice_not_found`, `voice_not_configured`, `gpu_hold`, …) |
| `speech_done` | `{chars, audio_ms, first_audio_ms, tts, tts_answered_by}` |

- It speaks the text as shown: `content`, then `voice.unheard`.
- Aborting the fetch raises the speaker's stop (§6.4's closure rule).
- It writes one TTS `request_logs` row per speak.
- *Built (WP4):* `web/chat_voice/speak.rs`.
  - `seq` counts from 0. A clause the TTS had nothing to say for (only inline tags) sends no
    `speech` frame.
  - `speech_done` also carries `stopped` (`speech/stop`, or the reader gone). `speech_error`
    ends the speech: no `speech_done` follows it.
  - `state` frames come at the route's opening, for a local row that is not up and loaded and
    not held off it (as a chat turn's own, §4.3); the same holds in a `speak: true` turn.
  - *Changed after the WP4 review (m3):* one `loading` and one end per stage. In a `speak: true`
    turn the Background warm may say `loading` before the route's opening finds the row still
    loading: whoever says `loading` first owns it, the opening says no second one, and the
    route's opening ends it (with the time since that `loading`) if the warm has not. A warm
    that ended without loading (`held`, `skipped`) leaves the load to the opening, which then
    says its own pair; the warm's frames after the route opened are not said.
  - *Changed after the WP4 review (m4, n3, m6):* the read-aloud plans in its own task
    (`speech::start`): the voice facts and the seed's first draw no longer hold up the first
    byte of the turn's text. It registers with the thread's speech at once and stays registered
    while it speaks, so `speech/stop`'s `stopped` counts the read-alouds still speaking (not one
    refused before it spoke, or ended by a failed voice). The page going away stops it through
    its reader (the channel closes). A `speech` frame's audio waits raw and is base64-encoded at
    the SSE edge; a page that reads slower than real-time playback would consume (a frame older
    than the newest still waiting, and the page out of the audio it took) is logged once as a
    WARN with how much audio waits, and the end logs the most that ever waited. Nothing is
    dropped and nothing is capped (`speech/out.rs`).
  - `404` for a thread or message that is not there, `400 bad_request` for a message that is not
    a reply, `400 empty_message` for one with no text. A thread that cannot speak still gets a
    `200` stream, with its `speech_error`.

### 6.4 A streaming reply

`send`, edit-user, regenerate and continue accept `speak: true`. Regenerate and continue take no
body today, so they gain an optional body that still accepts an empty request. The page sends
`speak` when the thread's resolved `read_aloud` is on.

With it, `start_turn` starts a Background TTS warm and puts a **speech tee** between the worker and
the SSE:
- **What it forwards.** The tee forwards every frame, feeds `delta` text to a `Splitter`, calls
  `flush` at a `tool` start, and interleaves the speaker's `speech*` frames into the stream.
  - Speech starts at the first clause, while the text still streams.
  - A failed voice sends `speech_error`, and the text carries on.
  - These turns do **not** get the voice prompt (§8.5); they are text turns that are also read.
- **Closure passes upstream.** The tee races `downstream.closed()`. When the page goes (Stop,
  reload, leaving the thread), it drops the worker's receiver and raises the speaker's stop. The
  worker then sees `ClientGone`, as today, and saves its partial reply. Without this, a tee holding
  the receiver would keep generation and synthesis running.
- **Stopping speech only.** `POST /chat/api/threads/{tid}/speech/stop` stops the speech of the
  thread's live turn; the live-turn slot holds that turn's speech stop. The text keeps streaming.
  The page's Stop button stops both.
- *Built (WP4):* `web/chat_voice/tee.rs`, a stream adapter with no task of its own; the
  read-aloud runs in its own task (`speech::start`) so its row is written however it ends.
  - The turn's `turn` frame is read before any of the speech's, so it stays first; after it,
    whichever is there is read (review n6), so a burst of deltas does not hold the speech back.
  - The Background warm is not started for a read-aloud whose speech was stopped, or whose page
    went away, before its plan was ready (review n9). Once started it runs to its end, never
    evicting.
  - The Background warm's `state` frames are interleaved; a `ready` with `ms: null` (nothing had
    to load) is dropped, as a turn's own admission says nothing for a model that is up.
  - `speech/stop` stops every read-aloud of the thread, a stored reply's included, from any
    window: the slot keeps a list of stops (`web/chat_live/speech.rs`), each registration a guard
    the read-aloud's task holds while it speaks. It answers `{ok, stopped}`.
  - The optional bodies: an empty or blank body is `{}`; anything else is read as JSON whatever
    its content type. `null` and a body that is not JSON are refused with `400`; regenerate and
    continue ignored a body before, and no client sent one (the UI sends `{}`).
  - *Decided after the WP4 review (m5):* **a continue is read from the clause it finishes.** The
    read-aloud is fed the stored reply's unfinished clause first (`speech::lead`: what the clause
    splitter has not cut off by the reply's end), so the first clause said is whole ("Dann heizt
    der Ofen schon vor.", not "n vor."), and nothing said before is said again. A reply that
    broke off right after a sentence end is not known to be finished until the next character
    comes (a decimal, an abbreviation), so its last sentence is said again; one that broke off
    inside a code block or a table starts at the block. The `speech` frames carry the clause as
    said, so a continuation's first `speech.text` begins before its first `delta`.

### 6.5 UI

- **Speaker button** in each reply's action row: play, and stop while playing. There is one
  playback per page; starting a new one stops the old.
- **Obligations for WP7** (WP4 review m9), so the page does not break the closure rule:
  - **Read a `speak: true` turn's stream to its end**: `speech_done`, `speech_error`, or EOF —
    not to `done`. The speech goes on after the text's `done`; aborting the fetch at `done` is the
    page going away, and stops the speech (§6.4's closure).
  - **One playback per page means one speech stream per page.** Starting a new playback (a
    speaker button, or a new turn sent with `speak`) aborts the fetch of the one before, which
    stops its speech; otherwise two streams play over each other. The composer's "busy" state
    must let a new send through while the previous turn's speech still plays: the text is done.
  - **The page's Stop** aborts the fetch (text and speech); a "stop speaking" control calls
    `speech/stop` and keeps reading the stream to its `speech_done`.
- **"Read replies aloud" toggle** in the composer row. It shows the resolved value and its source,
  and writes `voice.read_aloud` on the thread.
- **Playback** goes through the page's one player (§11.1), on the window's output device.
- *Built (WP7):* `pages/chat_voice/read_aloud.rs` (the page's one read-aloud), `controls.rs` (the
  speaker button, the toggle), `chat_turn.rs` (the turn's side).
  - A turn asks for `speak` when the open thread's resolved `read_aloud` is on — send, edit,
    regenerate and continue alike, decided in `run_turn`. The turn settles at the text's `done`
    (the composer takes the next send); the rest of its stream is read on in its own task, to its
    end, for the speech.
  - **Superseding.** A new playback ends the one before: its audio is flushed (`Player::begin`), and
    its fetch aborted — except a reply whose text still streams, whose speech is stopped with
    `speech/stop` so the text goes on; a speaker button waits for that answer before it asks for
    its own speech, since `speech/stop` ends every read-aloud of the thread.
  - **Stop.** The speaker button of the reply that plays stops it; a reply read as it streams has
    "Stop speaking" on the status line. Opening another thread stops a read-aloud of the thread
    left (its text, if still streaming, goes on); leaving the page stops it.
  - The player is held awake (`Player::hold_awake`) from the read-aloud's first moment to its end,
    a stored reply's included; audio that comes before the player is ready waits and is pushed
    once it is.
  - The speaker's tooltip names the thread's TTS, says under the hold where the text would go, and
    after a read the first audio's time and a fallback that spoke. `speech_error` is a status note
    (`gpu_hold` the hold chip); what was said before it plays out.
  - **The toggle** writes the thread's whole `voice` object with `read_aloud` flipped (the seed left
    to the server, §2.2) and keeps the settings drawer's draft in step, so the write is no unsaved
    change there. Turning it off stops what reads.
  - A turn's `state` frames (its own model loading, §4.3) are status notes too.
- *Changed after the WP7 review (M3):* **the composer's voice controls are one split control** —
  the microphone, and beside it a voice menu whose popover holds "Read replies aloud" (a switch with
  its value, source, text-to-speech and block line) above the window's audio devices. The toggle
  is therefore no longer its own button in the composer row (a deviation from the bullet above);
  the menu's button shows it instead — the speaker in the accent colour while the thread reads its
  replies aloud, amber when the devices warn or the replies read aloud would go to a fallback now
  — and its title says which. Attach and Knowledge sit together as square icon buttons, and where
  the composer is narrower than 560 px (a container query) the text box takes a row of its own
  with the buttons under it. Measured with `scripts/ui-matrix.py`, which now fails a composer
  whose text box is under half its width (`composer_input`): at 1280×800 with the thread settings
  open the box went from 59 of 484 px to the whole 484 (its own row), at 1440×900 from 180 to
  360 of 605, at 900×1200 from 130 to 310 of 555, at 1024×700 from 241 to 421 of 666. WP9's voice
  button joins the group (§9).
- *Fixed after the WP7 review:* **a read-aloud is told when something else takes the player**
  (m7): `Player::begin_owned` calls the owner of the item that played when another `begin` takes
  it (the devices popover's test tone, a realtime response), so its fetch is aborted (or its
  speech stopped), its buttons go back to idle and a note says "read-aloud stopped: another
  playback took the speakers". The speaker button turns amber while its text would go to a
  fallback (the hold, a benchmark run), as the composer's controls do (m6).
- *Changed after the WP11 UI review (m3):* **the speaker buttons rest in voice mode.** A stored
  reply read aloud there would take the session's speakers and play into its open microphone,
  where server VAD would hear it as the user's turn (only `device` echo mode with the default
  output, or headphones, cancel it). The button stays focusable (`aria-disabled`, the reason in
  its title and an `aria-describedby` element), and a press says "leave voice mode to read a
  reply aloud" on the panel's line. Routing the read-aloud through the session's player was the
  other option; it would still play into the microphone in `none` and `browser` modes. The
  thread's text-to-speech facts every speaker button shows are made once per page
  (`PageVoice::tts_*`, NIT 9), not per reply.

## 7. The turn seam and the agent-loop fixes

### 7.1 `TurnFrame`

`chat_turn::Events` becomes `mpsc::Sender<TurnFrame>`, where
`TurnFrame { event: &'static str, data: String }` lives in `web/chat_turn/out.rs`.
- **The SSE routes** map frames 1:1 with a `Stream::map` adapter. The wire is unchanged, and
  `ClientGone` keeps its meaning.
- **Golden transcripts first.** WP2 captures golden SSE transcripts *before* the refactor (send,
  edit, regenerate, continue, admin, temporary, KB auto, a tool thread) and compares them after it.
- **The entry point** splits into `start_turn_into(state, repo, thread, mode, caps, out, opts) ->
  Result<(), Response>` and the SSE wrapper.
  - `opts` carries the caller's stop (a realtime response's), the voice-turn flag (§8.5) and the
    speech plan (§6.4).
  - The caller's stop is a third arm of `Turn::stopped`, `Stopped::Interrupted`. It saves the
    partial reply as `ClientGone` does, and the channel stays open so `done` still arrives.
  - A stop raised before admission ends through `report_stop` with `done {aborted}` and no
    `message_id`. Callers accept that as "nothing saved".
  - A continue stopped before its first token answers `done` with the continued row's id and
    `saved: true`: the row is as it was, and nothing was added. A caller must not read that as a
    saved partial (WP2 review nit 8).
  - `out` must have room for the opening `turn` frame, which is written before `start_turn_into`
    returns. A full channel is refused with a 500 rather than waited on, since a caller that reads
    only after the call returned would deadlock (nit 5).
  - One deliberate wire change (WP2 implementation note): a stopped tool turn no longer sends the
    loop's `error` "the run stopped early: canceled (raise it under Settings …)". The turn reports
    its own stop, and there is no budget to raise. The `tool_superseded` golden shows it.
  - A second one (WP2 review M2): a tool turn whose upstream fails before it said or ran anything
    saves no reply, and ends `error`, then `done {aborted}` with no `message_id`, as the plain path
    does. It used to save an empty assistant row, which the page showed as an empty bubble, later
    requests replayed, and which kept §7.4's merge from engaging on tool threads. The
    `tool_upstream_error` golden is now byte-identical to `upstream_error`. The plain path does the
    same for a stream that breaks before its first token. A failure after partial text or tool
    calls still saves what there is.

### 7.2 `agent::run` relays live

Without this, a spoken tool thread waits for the whole generation, and text tool threads do not
stream.
- `run_turn`'s sink is synchronous (`DeltaSink::on_delta`). It forwards every delta into an
  unbounded channel. One poll of the turn bounds it, not the turn: the loop drains the channel
  before it polls the turn again, and does not poll the turn while an emit waits on a slow reader,
  so a stalled client holds the upstream back rather than filling memory (WP2 review nit 10).
- The loop `select!`s between the turn future and that channel and emits `LoopEvent::Text`,
  `Reasoning`, `CallStarted` and `CallArgs` as they arrive. Global call indices are computed live,
  so arrival order is kept, as today's post-turn replay keeps it.
- **When an emit fails mid-turn**, or the run's cancel lands, the turn is stopped cooperatively,
  awaited, then dropped (WP2 review M1). The turn's sink carries a `StopSignal`; a runner that takes
  it (`DeltaSink::stop`, as `stream_once_on` does) ends at its next await and still writes its
  `request_logs` row: status 200, `canceled`, the cost so far (`proxy/stop.rs`). A dropped call would
  write no row, and a client could take a streamed `/v1/responses` answer budget-free by hanging up.
  A runner that never takes the stop (the unary `sample_once` ones) is dropped as before. `emit!`
  cannot move `ir.messages` while the turn borrows `ir`, so the loop is restructured for that.
- **The cancel is raced against every emit** and checked before every delta, so a reader that stopped
  reading cannot hold a cancelled turn (and its runner's GPU admission) in place, and nothing queued
  after the cancel is relayed. `Cancel::signal` is the event-driven cancel (no sampling interval);
  `Cancel::flag` keeps the jobs' 250 ms sampling.
- **A stopped tool thread** (Stop, supersede, the caller's `Interrupted`) raises its loop's
  `Cancel::signal` at once; its select hears the stop before it polls the loop again. From then on
  its sink relays and keeps no more text, so the partial reply is what the reader was sent, and it
  sends the loop's frames without waiting for the reader (a frame with no room is dropped). The
  runner and its GPU admission are released before the turn's last frames (`error`, `done`), which
  are the only ones that wait for the reader.
- **The unary-runner check** (no deltas → surface the completion) stays.
- **`/v1/responses` streams live too**, and tool threads' `ttfb_ms` becomes truthful. The other
  `agent::run` callers only collect events and are unaffected.

### 7.3 An abandoned call gets its result

Today a cancel during tool execution returns a record ending in an assistant tool call with no
result (`agent.rs:608-637, 924-930`), and `build_messages` replays it as it is. `canceled!` now
pushes a `Role::Tool` message with `ABANDONED_CALL` for each call in flight before
it returns, as `agents/batch.rs:986` already does. The stored record stays replayable. Today a Stop
during a tool can wedge a thread for strict upstreams; in voice mode, "never mind" makes that
routine.

A cancel raised before the batch started made none of its calls: their results say
`not run: the turn ended before this call was made` instead (WP2 review nit 1). A cancel while a
resume's approved calls run reports one result per resume entry, at its `CallReady` index, each
denial kept (nit 3).

A turn that **fails** after tools ran keeps their record (WP2 review M3): `agent::run` returns
`RunError { error, messages, usage }`, where `messages` holds every finished turn with its results.
Without it, the next request would show the model plain text, and it might make a side-effecting
call again. A tool thread stores that record with the reply's partial text; its next request
replays the calls with their real results, then the partial text.

A record that ends in results is followed directly by the next user message when the stopped turn
said nothing after its calls (Stop mid-tool, a budget stop, a failure after tools). Kept as it is
(WP2 review m4; the owner may decide otherwise):
- OpenAI accepts `… assistant(tool_calls), tool, user`.
- The Anthropic and Gemini egresses fold the results and the new text into one user turn
  (`chat_replay_shapes`).
- ChatML (Qwen-class) and Llama templates render it.
- Mistral's API (mistral-common's validator) and Mistral-style Jinja templates that count
  user/assistant alternation refuse it.

The fix for those would be an assistant message between the results and the user text. An empty
one does not satisfy Mistral's API, and a non-empty one puts words in the model's mouth that it
never said.

Two more ways to the same wedge, closed in WP2 (implementation note):
- The loop records a batch's results **before** it reports them, so a client gone mid-report
  leaves every call answered.
- A chat turn that ends before its calls were made (the tool-call budget ran out, or the page left
  while the calls were announced) stores `not run: the turn ended before this call was made` for
  each of them (`agent::close_trailing_calls`). A thread's loop never hands a call back to a
  client, so a trailing call there was never made. A call to a tool the thread does not have (the
  model invented the name) says `not run: this thread has no tool named '…'` instead, so the model
  can recover (WP2 review nit 7). `/v1/responses` keeps its hand-back semantics: a stored response
  whose run stopped with server-side calls unmade (the budget, a client gone while they were
  announced) closes those with the same result before it is stored, so a chained request is
  well-formed; a client's calls, and the calls of a client-tool or approval hand-back, stay open
  for the next request (WP2 review m5).

### 7.4 Adjacent user messages are merged when a request is built

`build_messages` merges consecutive user messages into one: parts in order, texts joined by a blank
line. Realtime's renderer does the same (realtime §7.2), because strict templates refuse two user
turns in a row.

This fixes the text path too: a failed send followed by another send. It is also what keeps a
voice thread answerable after a failed or deleted reply (§8.3).

A user message's knowledge context block (auto mode) stays a part of its own, never joined onto a
typed text, and a merged message keeps only its run's last block (WP2 review m3). Each block
numbers its excerpts from 1, so two would make a citation `[1]` ambiguous, and the page maps
citations to the latest retrieval. The failed send's text stays; its retrieval gives way to the
retry's. The merge is pinned on the OpenAI, Anthropic and Gemini request shapes
(`chat_agent_live`, `chat_replay_shapes`).

### 7.5 `DeltaSink::flush`

`DeltaSink` gains `fn flush(&mut self) {}`. `Splitter` implements it by closing the clause in
progress, as at a tool call or the end of the stream, so a preamble is spoken before the tool runs. It
is called only at a tool call or a turn boundary: `Splitter` resets its whole reading state there
(code fence, list numbering, cues), so a flush inside an open code block would make the rest of it
speakable (WP2 review nit 4).

## 8. Realtime mode bound to the thread (server)

### 8.1 The binding

**At the handshake.** The binding is the query `GET /v1/realtime?chat_thread=<id>`, negative for a
temporary thread. It is decided before the 101, so a bound session starts bound.
- `session.created` carries the thread's resolution.
- Realtime's default-model resolution and its policy check for the defaults are skipped. A stale
  `realtime.default_model` cannot fail a bound session.
- The connect warm is the thread's group Admit warm (§4.2). An unbound session warms and resolves
  exactly as today.

**Refusals before the 101** (flat JSON, as realtime §10.2):

| Status | Code | Why |
|---|---|---|
| 403 | `chat_thread_not_allowed` | the principal does not pass `Cap::Admin`, the capability of every `/chat/api` route, checked by the same function (dashboard cookie or owner key) |
| 404 | `chat_thread_not_found` | |
| 409 | `chat_thread_admin` | §8.1 below |
| 400 | `owned_by_thread` | `?model=` beside `chat_thread` |

**No Admin Chat (ruling 7).** The bind is refused when `thread.kind == "admin"`, and only then.
- The same check runs again **before each response**, against the thread as re-read. A failing
  check refuses the response with `error {code: "chat_thread_admin"}` before `response.created`;
  the page leaves the mode and shows the reason.
- *Built (WP8):* a thread's kind never changes after it is created (no write path sets it), so
  the bind's check holds for the session's life; the check before each response is the turn's
  re-read of the thread (`realtime/thread/turn.rs`), which refuses `chat_thread_admin` — and
  `chat_thread_not_found` for a thread that went away — before the model is called, after
  `response.created`.
- **A plain thread with the self-admin toolset attached by hand is allowed.** The label is
  dispatched for any thread (`mcp/exec.rs::builtin_label`, `agentchat.rs:443`). Attaching it is the
  owner's explicit choice, and in a developer tool user intent wins (owner, 2026-10-03). The panel
  shows it instead of blocking it (§9.4).

**One bound session per thread.** A second bind of the same thread takes over: the older session is
closed with a reason ("voice mode moved to another window"). Two journals and two item maps never
write one history.

**Ownership.**
- **The thread owns** the chat model, instructions, tools, transcription model, `tts_model`, voice,
  speech instructions, `tag_hint`, language, seed and the conversation.
  - A `session.update` that *changes* one of them, or sets it to `null`, is refused with
    `owned_by_thread`. Clients echo the whole session through realtime's deep merge, so an equal
    value is accepted.
  - So are `conversation.item.create` and `.delete`, and the overrides on `response.create`
    (`instructions`, `tools`, `tool_choice`, `lmgw.speech_instructions`, voice).
  - The page changes them through the thread's settings route. The session re-reads the thread
    before each commit and response, so a chip change applies from the next turn.
- **The client owns** turn detection (`null` = push-to-talk), `half_duplex`, `echo_tail_ms`, the
  barge-in knobs, `output_modalities`, and the `truncate`, `response.cancel`, `response.create` and
  `input_audio_buffer.*` events.
- `session.lmgw.resolved` gains `chat_thread: {id, title, temporary, admin_tools}`.
  *Fixed after the WP8 review (m10):* `admin_tools` (the panel's flag, §9.4) is in it from the
  bind, and each response re-reads it with the thread: a thread that gains the self-admin
  toolset mid-session (its settings route, another window) or whose `self_admin` flips is said
  before the turn's frames, as `lmgw.chat.thread` (§8.7), and so is the title the first spoken
  turn names. The session object takes it too.

Keep (temporary → stored) is refused with `409 voice_session_active` while the temporary thread is
bound; the page disables Keep in voice mode.

*Built (WP8, the binding):* `realtime/thread.rs` (`thread/bind.rs`, `thread/owned.rs`,
`thread/hooks.rs`), `web/chat_live/voice.rs` (one binding per thread) and `web/chat_voice/bound.rs`
(the seam into the Chat's code).
- The refusals are checked in the order 403, 400, 404, 409: a key that may not bind learns
  nothing about the threads.
- The session starts with the thread's choices as its own, merged through realtime's
  `session.update` path (`thread::shape_session`): the transcription model and language, the
  voice (or `merge::DEFAULT_VOICE` when none is named, as the Chat's speech plan asks), the TTS,
  the thread's own speech style, and the thread's resolved turn detection (`push_to_talk` →
  `null`) as the starting point the client may change.
- **Takeover:** the older session gets `error {code: "chat_thread_taken_over"}`, then the close
  `4000` with the reason "voice mode moved to another window".
  - *Fixed after the WP8 review (m7):* the older session drains its journal after the newer one is
    live, and that drain may still write (its partial reply's save and cut, a user message whose
    entry waited behind an open slot, the turns it writes as it ends, §8.6). The newer session
    gets the older binding's fence — raised once that binding is gone, which its session drops
    only after its journal drained — and its journal writes nothing before it. So the two
    journals write one history one after the other, never interleaved: the newer session's first
    turn waits for the older session's last writes (milliseconds, or a last transcript).
- **Ownership** also covers `reasoning`, `max_output_tokens` and `parallel_tool_calls` (session and
  `response.create`): the turn is the Chat's, with the thread's reasoning and output settings, so
  a session's would be accepted and silently not applied. The instructions are refused only when
  an update names a different value — the merge itself rewrites them when the output modality
  (the client's) changes.
- The connect warm is the thread's Admit group whatever `realtime.warm_on_connect` says: entering
  voice mode is the press (§4.1).

### 8.2 A bound turn, end to end

1. **Commit.** As realtime §4.2: a detected or manual commit, ASR (fallbacks as configured), the
   transcript.
2. **Response start.** The responder awaits the journal's barrier (§8.3), then queues the user
   message for this response's turns and emits `lmgw.chat.user`.
3. **The LLM step is the chat turn.** It runs `start_turn_into` with:
   - `TurnMode::Fresh {user_message_id}`;
   - a `TurnFrame` channel, kept until `done`;
   - the response's stop;
   - the voice-turn flag.

   `thread_caps` is computed for this response, so images in history become placeholders for a
   model without vision, as in text. The thread's model, prompt (with §8.5's voice block),
   sampling, reasoning (§8.5), attachments, knowledge bases and MCP tools apply through the same
   code as text.
4. **Frames.**
   - `delta` text goes to the session's `Splitter` (speaking) or `Forward` (text output) as
     `TextDelta`. Everything after that is realtime unchanged: clauses, TTS on one route, pacing,
     transcript deltas, the heard table, barge-in.
   - A `tool` start calls `flush`.
   - Every frame is relayed as `lmgw.chat.frame`. Tool calls never become `function_call` items,
     because the client must not run them.
   - `done` gives `message_id` (0 or absent: nothing saved) and the turn's generation, which go
     straight to the journal (§8.3).
5. **Finish.** The responder returns a `Completion` (text and usage, no tool calls). The core's
   response lifecycle is unchanged.

**`empty_turn`.** A response that answers no owed turn and has no new words is refused before
`response.created` with `error {code: "empty_turn"}`. Realtime decides over every owed turn
(`lifecycle/pending.rs`), so a cough that cuts a reply nobody heard still re-answers the question
it cut.

*Built (WP8, the turn):* `realtime/thread/turn.rs` (the bound responder), `realtime/lifecycle/bound.rs`
(launch, `empty_turn`, the end of a response), `web/chat_voice/bound.rs` (the seam: the thread, its
plan, `start_turn_into`, the history writes).
- **`empty_turn` before `response.created`** when the response's transcripts are in by then; a
  push-to-talk `commit` + `response.create` is created while its transcript is still being made,
  so it is refused when the transcript comes in, as `error` and `response.done {failed}` — as
  `transcription_failed` is.
- **The chips apply from the next turn:** the turn re-reads the thread (model, prompt, sampling,
  tools) and plans its speech as the read-aloud does (TTS, voice, style, language, announcements,
  seed), and each ASR call reads the thread's ASR alias when it starts. The session object itself
  keeps what the bind set; an echo of it is accepted. A speech refusal (`tts_not_configured`,
  `voice_not_found`, `instructions_required`, …) fails the response after `response.created`
  (a code the session has no name for is `speech_unavailable`, its own code in the message).
  *Fixed after the WP8 review (m4):* the session's own speech check before `response.created`
  no longer judges a bound session's TTS and voice — they were the bind's, so a thread bound
  without a usable TTS refused every response even after its chip was fixed. The plan judges
  them, as it judges `instructions_required` (the thread's style chain is the Chat's).
- *Fixed after the WP8 review (m5, NIT 9):* a bound session's `speech_started` warm re-reads the
  thread (`bound::connect_stages`, Background), so it warms the chat model, ASR and TTS the next
  turn uses, not the bind's, plus the word check's own alias when the client named one; and the
  barge-in word check, when it has no alias of its own, transcribes with the thread's ASR alias
  as re-read, as the turn does.
- **Rows:** the TTS row says `realtime` (`ClientProto::Realtime`), the ASR row too; the chat row
  is the chat engine's (`chat`).
- A text-output bound session's turns are plain chat turns: no voice block, the thread's
  reasoning; the deltas go to the client as text. *Fixed in the WP8 fixes:* the WP8 build dropped
  the speaker's handle on the chat's stop for a text-output turn, and a dropped handle is a
  raised stop, so every such turn was stopped before its first token (`canceled`); nothing
  tested it until review m8's IT.
- *Fixed after the WP11 reviews:*
  - The turn's refusal or failure is the response's error as the gateway error it was (server
    M1): `TurnFrame::error` carries it to the responder, so `gpu_hold`, `gpu_benchmark`,
    `context_length_exceeded` (`invalid_request_error`), `vram_queue_timeout` come out with the
    code, type and message a stock session sends. Only a frame with no gateway error behind it is
    `upstream`; `superseded` and `not_saved` still stop the speech.
  - The bound refusals (`chat_thread_not_found`, `chat_thread_admin`, `superseded`, `not_saved`,
    the speech plan's `tts_not_configured`, `voice_*`, `instructions_required`,
    `speech_unavailable`) are `invalid_request_error`, as the stock session types
    `tts_not_configured`, not `permission_error`, and name no `param`: a bound client sets no
    voice (binding NIT 2). `chat_history_write_failed` stays `server_error`.
  - A tool thread's `response.done.usage` carries the counts of the turn's `done` frame, which is
    where the tool loop says them (server m1).
  - The ASR alias is the thread's both ways (binding NIT 1): a session bound while the thread
    named none takes commits, and each turn reads the thread's ASR when its call starts — one
    named since is used, and a thread that names none (its chip cleared since the bind) fails
    the turn `asr_not_configured` instead of keeping the bind's alias. A thread that is gone
    keeps it: its response says `chat_thread_not_found`.

### 8.3 The journal: what is written, in which order

`realtime/thread/journal.rs` is one FIFO task per bound session. It is named so as not to be
confused with `realtime/writer.rs`, the socket writer.

- **User entry.** It writes one user message for the response's turns that have no message yet.
  Realtime item ids map to message ids, so turns owed again after a cut are never written twice.
  - It sets `voice.via = "realtime"` and names an untitled thread, as `send` does.
  - The write is a history write (`append_user_message` → `chat_live.write`), like a text send.
- **Reply slot.** The responder opens a slot when its response starts. The slot finalizes once both
  of its inputs are in:
  - from the **responder**: the turn's `message_id` and generation, sent straight to the journal,
    past the core's generation filter, so a cancelled response's `done` still lands;
  - from the **core**: the heard cut, at drain or cancel.
- **Late truncates.** The page's `truncate` follows a barge-in by a round trip. When it arrives
  after the finalize, it queues a re-cut, applied only under the guard below. *Stated (WP8 review
  NIT 1):* the next response's user message moves the generation too, so a truncate that arrives
  after it — rare: the page truncates before its next commit — is skipped like one after another
  window's turn, though the only turn since is the session's own.
- **The order is fixed:** turn N saved → finalize N → user message N+1 → `begin` N+1. The barrier at
  the start of response N+1 waits for every earlier entry, so N+1's history write can never refuse
  N's save.
- **Finalize:**

  | Case | Write |
  |---|---|
  | nothing saved (failed, superseded, aborted) | none; the next user message is merged at build time (§7.4) |
  | heard whole | annotate only: `voice` with timing and served models, no generation move |
  | heard in part | cut (§8.4) |
  | heard none, no tool record | delete the reply |
  | heard none, with a tool record | never deleted: `content` = the record's own text, the final answer to `unheard` (§8.4) |

- **The guard.** A cut, a delete or a re-cut is a *conditional* history write. It proceeds under
  the thread's lock only while the generation is still the one the voice turn left behind.
  Otherwise (another turn started meanwhile, in any window) it is skipped with a WARN line and
  `lmgw.chat.reply {skipped: "another turn started"}`. A voice finalize therefore never cancels a
  live text turn. *Fixed after the WP8 review (m3):* a finalize whose cut or delete is skipped
  still writes the reply's `voice` (how it was spoken, its timing and models, no `unheard`) —
  which moves nothing — so the reply keeps its badge and timing, and its text stays whole, as the
  turn that started meanwhile answered it; the event then carries the `voice` too. A reply is
  said skipped once: the generation that moved never comes back, so a later re-cut of it has
  nothing to try and is dropped silently (NIT 14).
- **Text and voice turns exclude each other** as two text turns do today: the newer one supersedes
  the older, which is stopped and not saved. A superseded voice response fails with
  `error {code: "superseded"}`, and the session stays open. *Fixed after the WP8 review (m1):*
  its speech stops at the turn's `superseded` (or `not_saved`) error: the speaker has a stop of
  its own, so no clause is synthesized after it, and only what had already left within the lead
  plays.
- **A deleted thread** fails the next response with `chat_thread_not_found`. The page leaves the
  mode. *Fixed after the WP8 review (m11):* a user message the store refuses (SQLite busy, I/O)
  is `chat_history_write_failed` (`server_error`), not `chat_thread_not_found`: the thread is
  still there, and the page stays in the mode. The turns it held are kept by the journal and
  lead its next user entry (the next response's, or the turns written as the session ends);
  any still unwritten when the journal drains are logged with their words.
- *Built (WP8):* `realtime/thread/journal.rs` (with `journal/user.rs` and `journal/finalize.rs`
  since the WP8 fixes) and `realtime/thread/reply.rs` (the decision, unit tested).
  - A response is one entry queued at its launch, in order: its user entry, then its slot. The
    user entry's answer (the message id) is the turn's barrier. A response with no new words
    writes no user message (`TurnMode::Fresh` then names none).
  - The turn's generation reaches the journal through `TurnOpts::began` (the `done` frame is
    unchanged on the wire). The conditional write (`LiveTurns::write_if`) does not move the
    generation: no turn started since, so none is live to cancel; the session's binding keeps the
    thread's slot, and a slot that went refuses the write. A guarded write that is refused writes
    nothing at all (`lmgw.chat.reply {skipped: "another turn started"}` and a WARN).
  - A cut inside a clause said otherwise than written cuts after the clauses heard whole
    (§8.4); a reply whose start is neither is annotated uncut, with a WARN. `unheard` is
    trimmed.
  - Every response that opened a slot gets its cut from the core (`lifecycle/bound.rs`, at the
    timing line: drain, cancel, failure, or the session's end), so no slot waits for ever.

### 8.4 Heard and unheard

The core knows, per assistant item, what was heard as written: realtime §7.3's heard table holds
whole clauses with the text left out before them, plus the heard part of the clause the cut falls
in. *Changed in the WP9 fixes (review B):* that part ends at the last word heard whole, never
inside one ("… bestimmte Lichtw" is stored as "… bestimmte", and "Lichtwellenlängen" goes to
`unheard`), by realtime §7.3's one rule for stock and bound sessions; a cut inside the clause's
first word leaves nothing of it. A cut stores:
- `content` = that text;
- `voice.unheard` = the stored reply with the heard prefix removed.
- *Fixed after the WP8 review (M1):* the heard part of the clause the cut falls in is that clause
  *as it was said*, and the speakable pass rewrites a clause before it is said (markdown
  stripped, "z. B." spelled out, parentheses turned into commas, whitespace collapsed, a tag made
  canonical or left out of what was said). A cut inside such a clause — nearly every cut falls
  inside one — is then no prefix of the stored reply. The cut falls back to the clauses heard
  whole (`HeardTable::written_whole`: what was written up to the clause the cut fell in, always
  the model's own text), and the partial clause goes to `unheard` whole. When not even one clause
  was heard whole, the nobody-heard rule applies (deleted, or the tool record kept). Only a reply
  that starts with neither is left uncut, with a WARN line.
- *Changed after the WP11 binding review:*
  - A clause heard to its last word — the cut in its last character or its trailing silence,
    the punctuation absorbed — is heard whole (m2): `written()` gives the model's own text for
    it, so "Das ist *laughs* lustig." heard to "lustig" stays in `content`. `end_sample` and
    `clipped` still say where the audio was cut; an announcement heard to its last word keeps
    its block, as a cut right after it does.
  - Before falling back to the clauses heard whole, the cut keeps the words of the partial
    clause that were written as said (NIT 5): the heard text is trimmed back — its trailing
    punctuation first, then a word at a time — to the longest start of the reply that ends at a
    word there, never shorter than the clauses heard whole. "Das ist **wichtig**." cut after
    "wichtig" stores "Das ist"; "Der Wert (etwa zehn)" cut after "Wert," stores "Der Wert". Every
    word kept is byte for byte the model's own.
  - A clock time ("10:30") and a URL (a run without whitespace holding `://`) are one word, as a
    hyphenated compound is (NIT 4): a cut inside them stored "10:" or "https://" as heard.

**Announcements** (§6.2; WP4 review M2). A bound session passes
`Splitter::new(…, Some(Announce::for_language(<the thread's reply language>)))` (the reply
language since 2026-10-05, §2.1); a stock session passes
`None` and announces nothing. Then:
- **An announcement clause is marked** (`Written::announcement`). Its words are never written to the
  history or to `content`, whether it was heard whole or in part (`HeardTable::written`).
- **A block counts as heard when its announcement was heard whole.** An announcement's `before` is
  what was written up to its block, and the clause after it carries the block in its `before`:
  - a cut inside the announcement keeps what came before the block, and neither its words nor the
    block (since the WP9 fixes, a cut inside its first word is a cut before it: what was written
    before it goes with it, as for any clause of which no word was heard);
  - a cut exactly after it keeps the block (`HeardTable::keep` gives it to the announcement), and a
    cut inside the next clause keeps the block and that clause's heard part;
  - of two blocks in a row, only the one whose announcement was heard whole is kept;
  - a trailing block reaches the history as the tail, unless audio was cut away before it.
- **`voice.unheard` is computed from the written text**: the stored reply minus `written()`. Never
  from the transcript (`text()`, `heard()`, `output_audio_transcript.delta`, the item's
  transcript), which carries the announcement words. Those are for captions only; nothing derived
  from the transcript is stored.
- **The timing line** (§8.7): `Mark::FirstClause` may be an announcement, so `first_clause_ms` then
  measures it; the event says so (`first_clause: "announcement"`).
- *Built in the WP4 fixes:* the marker, the heard table's rule and their unit tests
  (`realtime/heard/written.rs`, `responder/speech/tests/announce.rs`). WP8 adds the `Announce` for
  bound sessions, `unheard` from the written text, the timing note, and ITs: a cut inside "Codeblock,
  rust."; a cut exactly between an announcement and the next clause; `content` and `unheard` equal
  to the stored reply minus `written()`.

**With a tool record** (`ir_messages`) the record stays whole: the calls happened.
- A cut inside the final answer cuts only that.
- A cut earlier, in the preamble or while a tool ran, sets `content` to the record's own text and
  moves the final answer to `unheard`. `chat_turn::final_answer` then finds an empty answer, where
  a shorter `content` would be replayed whole after the record. The preamble then counts as heard
  whole; this is stated, not hidden.
- *Fixed in the WP8 fixes:* the whitespace the model wrote between its preamble and the answer
  after the tool call reached the heard table only when it held more than whitespace, so a cut
  inside the final answer read "Moment.Es sind", no prefix of the stored reply (it fell back to
  the preamble rule). A stock session's history lost that space too. Whitespace-only unspoken
  text is now kept once a message is open. An IT cuts a tool reply in its answer and in its
  preamble.

### 8.5 Prompt, reasoning, tools

**The prompt of a voice turn** (ruling 5). The built-in chat prompt (`config/chat_prompt.rs`) asks
for Markdown with tables and code and says "Today is {{date}}"; `DEFAULT_VOICE_INSTRUCTIONS`
(`config/settings_classes.rs:495`) says "You are a voice assistant", forbids Markdown, lists,
tables and code, and says the model does not know the date. It is built per turn in `start_turn_into`'s request
builder and never stored in `system_prompt`. Only bound turns with audio output get it; a
text-output session and `speak: true` turns do not (they get the language alone, below). The
system message is, in order:

1. **The thread prompt**, expanded (`{{model}}`, `{{date}}`), exactly as in text turns.
2. **The voice block.**
   - When the thread prompt is not empty, it opens with the bridge: *"This reply is spoken aloud.
     For it, the instructions below replace any guidance above about formatting, Markdown, tables
     and code."*
   - Then the voice instructions:
     - `realtime.default_instructions` unset → the built-in text, assembled from three constants.
       `VOICE_PERSONA` ("You are a voice assistant.") is left out, because the thread prompt says
       who the model is. `VOICE_STYLE` ("Your replies are spoken aloud … never use markdown, lists,
       tables or code. Answer in the language the user speaks.") is kept. `VOICE_NO_DATE` ("You do
       not know the current date or time …") is kept only when the thread prompt has no
       `{{date}}`.
       `DEFAULT_VOICE_INSTRUCTIONS` becomes the concatenation of the three, byte-identical, so
       realtime is unchanged.
     - Set to a text → that text, verbatim. They are the owner's words; nothing is dropped.
     - `""` → no voice block, and no bridge either.
   - *Changed 2026-10-04 after the owner's test:* with a conversation language (§2.1),
     `VOICE_STYLE`'s "Answer in the language the user speaks." is replaced, in the built-in text,
     by the language sentence — *"The user speaks German and hears your reply in a German voice, so
     answer in German unless the user asks for another language."* (the name from
     `audio::language` NAMES; a code without one is named as the code). `VOICE_STYLE` is now
     `VOICE_FORM` + `VOICE_FOLLOWS_USER`, `DEFAULT_VOICE_INSTRUCTIONS` still byte-identical. With
     the owner's own text, or `""`, the sentence is its own paragraph after the voice block: it is
     a setting, not part of the owner's wording. Per turn, never stored, like the block. Every
     bound turn gets it — a text-output session's too (the user spoke; *"The user speaks German, so
     answer in German unless the user asks for another language."*, alone after the thread
     prompt, reasoning the thread's) — and so does a `speak: true` turn (the spoken sentence,
     alone). Without a language nothing changes: the reply follows the user, whose transcript a
     language-detecting ASR may have heard as English.
   - *Changed 2026-10-05 (the split, §2.1):* the sentence names the reply language as what to
     answer in, and the spoken language — when one is set — as what the user speaks
     (`web/chat_voice/prompt/sentence.rs`). With both the same, or only the spoken one set (the
     reply follows it), it is the 2026-10-04 sentence word for word. The combinations, spoken /
     read:
     - both, different: *"The user speaks German and hears your reply in an English voice, so
       answer in English unless the user asks for another language."* / *"The user speaks German,
       but answer in English unless the user asks for another language."*
     - only a reply language: *"The user hears your reply in an English voice, so answer in
       English unless …"* / *"Answer in English unless …"* — nothing claimed about what the user
       speaks.
     - neither: nothing, as before.
     "a" before the voice's language is "an" before a name with a vowel sound ("an English
     voice"); a German sentence is unchanged.
   - *Changed 2026-10-04 after the review:* a `speak: true` turn's sentence waits for its
     read-aloud's speech plan (`TurnOpts::heard`; the plan still runs in its own task, so the
     `turn` frame waits on none of it, review m4) and is the spoken one only when the plan stands;
     a refused plan (no text-to-speech model, a voice not found) reads nothing aloud, so the turn
     gets the text-output sentence.
3. **The tag/cue hint**, `speech_hint_text(…)` for the session's primary TTS, as its own paragraph,
   when `realtime.tag_hint` is on (realtime §7.2).

**Which lines win:**
- The voice block wins on **form**: length, no Markdown, lists, tables or code, the user's spoken
  language.
- The thread prompt wins on **identity and facts**: who is speaking, the alias, the date,
  attachments, tools.

The model gets one instruction set with no contradiction left in it.

**Costs, stated:**
- The system message differs between text and voice turns, so llama.cpp re-prefills the history
  at each switch between them.
- Tags and cues a voice reply writes are kept as written: in the history, and in its text bubble.

**Reasoning (implied by ruling 5).** An explicit thread setting (`reasoning_enabled` set) is
honoured. Where the thread leaves it at the route default, a voice turn asks for reasoning off,
through the existing reasoning control, as realtime §7.6 does: voice turns already differ from text
by ruling 5, and thinking delays the first word. Reasoning is never spoken. Its frames are relayed
so the bubble shows the trace.

*Fixed 2026-10-04 (found live, older than this branch):* a cloud model that cannot take the
protocol's off failed every such turn — `gpt-4.1-nano` refuses `reasoning_effort` outright,
`gemini-flash-lite-latest` refuses `thinkingBudget: 0` — and so did a text turn with reasoning off,
and a cloud fallback under the GPU hold. The off is now fitted to the model before it goes out
(model-capabilities design §5.6): no control where the model has none, its lowest level where it
cannot stop, the off where it can; decided from the model's capabilities, else retried once on a
refusal that names the control and remembered. The turn's `done` reports `enabled` among
`reasoning_ignored` when the off went out in another form.

*Ruling 2026-10-04:* a model that cannot run without reasoning, cloud or local, is to run with
reasoning enabled instead of erroring — whether it is used for voice mode is the user's call — and
reasoning is not spoken but shown in the thread. *Built:*
- **No off ends in an error.** A refused off is retried with what the refusal names, then with
  no reasoning control at all, so the model reasons as it does by default (model-capabilities
  §5.6). A local llama-server row gets the template's `enable_thinking: false`, which no
  template refuses; one that does not read it reasons anyway, and that is observed.
- **Never spoken.** A bound turn's `reasoning` frames are relayed as `lmgw.chat.frame` and timed,
  never handed to the clause splitter: the speakable pass, the TTS and the heard table see the
  reply's text alone, whatever the model reasons at and when the thread switches reasoning on.
- **Shown and stored** exactly as a text turn's: the bubble's collapsed "thinking" block, live
  and after the read-back, and the reply's `reasoning` column. The OpenAI-shaped decoder now also
  reads the trace under OpenRouter's name (`reasoning`, a string), which Kilo sends and lmgw
  dropped before; `reasoning_content` wins when both come. Providers that return no trace
  (OpenAI's chat completions, Gemini without thought summaries) have nothing to show. A cut rewrites `content` alone, so
  the reasoning stays whole; the next turn gets it beside the heard text, as a text turn's trace.
  A reply nobody heard is still deleted whole (§8.4), its reasoning with it.
- **Said once.** `done.reasoning_note` says it in a sentence when the model reasoned although off
  was asked ("… cannot switch reasoning off; it reasons at its lowest level (minimal)", "…
  refused every way lmgw has to switch reasoning off; it reasons as it does by default", "… did
  not switch reasoning off; it reasoned anyway"), and adds `enabled` to `reasoning_ignored`. The
  panel's status line and, in text chat, the composer's line show it as one note, replaced by the
  next turn's. `lmgw.response.timing` gains `reasoning_ms`, the part of `first_token_ms` the
  model reasoned, which the timing readout shows.

**Tools** run server-side through `agentchat`, live by §7.2. There is no spoken filler (realtime
§7.4). The page shows the running tool, and the visualisation stays in `thinking`.

*Built (WP8):* `config/settings_classes.rs` (`VOICE_PERSONA`, `VOICE_STYLE`, `VOICE_NO_DATE`;
`DEFAULT_VOICE_INSTRUCTIONS` their `concat!`, byte-identical, unit tested) and
`web/chat_voice/prompt.rs`. The voice block is one paragraph (the bridge, a space, the voice
text) after a blank line, the hint another; `TurnOpts::voice` carries the hint, and
`chat_turn::request` builds the block and asks `reasoning_enabled: false` when the thread sets
no reasoning override at all (an effort or a budget alone counts as the thread's setting) —
through `chat_voice::prompt::voice_request` since the WP8 fixes (NIT 13: `chat_turn.rs` keeps
the call site only). *Tested after the WP8 review (m8):* a unit test and an IT pin the ruling —
off when the thread leaves it, the thread's explicit setting otherwise — and that a text-output
session's turn has no voice block and asks no reasoning.

### 8.6 Ends and disconnects

- A socket that drops ends the session. Its stops are raised, the running chat turn saves its
  partial reply (`Interrupted`), and the open reply slot finalizes with the core's last heard cut,
  which is what was sent and played within the lead (realtime §7.3).
- The journal **drains before the session's task ends**. No write is lost or left half done.
- The page shows the close reason (takeover, thread gone, admin, network) and offers **Re-enter**.
  Nothing reconnects by itself.
- *Built (WP8):* `Core::end_bound` — the active response's call is stopped and its slot cut to
  what was sent, then the journal is closed on the core's side and awaited: it ends once every
  responder said what its turn saved and every entry is written.
- *Fixed after the WP8 review (m9):* a turn committed but answered by no launched response — one a
  response still awaiting its transcript answers, one committed while a response ran, a
  push-to-talk commit not asked to be answered — used to be lost ("thanks, bye", then Esc). The
  session's end now awaits the transcripts still being made (each ASR call ends on its own, as a
  turn's does; the session's stop goes with the core, after this), and queues one user message
  for those turns with words, after every earlier entry. Nothing answers it then; the thread's
  next turn does (§7.4 merges adjacent user messages). The turn the user was still speaking
  when the session ended was never committed, and is not written.
- *Fixed after the WP11 binding review (M1):* the page's own close is answered only after the
  drain, whatever the writer still had to send. A peer's close makes every later send fail; the
  writer used to end there and drop the socket, so a page leaving mid-reply saw the connection
  drop (1006) at once, before the reply's cut and the last transcript were written, and Keep's
  first try got `409 voice_session_active`. Now a failed send quiets the writer: it sends
  nothing more and drops what reaches it (paced audio too; what left stays recorded for the
  heard cut), keeps the socket until the session lets go of it after the drain, and then closes
  it, which flushes the close reply queued when the page's close came in.
- *Fixed after the WP11 binding review (m1):* the wait for the last transcripts is bounded by
  `realtime.ping_interval_s`, which bounds the writer's last events too (0: no bound, the
  owner's explicit choice). Past it the ASR calls are stopped, with a WARN naming how many turns
  lose their words; the session then drains and lets the thread go. Without it, a hung or
  slow-loading ASR call held the binding: Keep refused, the page's close not answered, and a
  re-entered window's first turn waiting behind the takeover fence with nothing said. The
  fence's wait is logged at INFO at both ends, and so is the end's drain, with its time.

A bound session sends these events; an unbound session never does, so no stock client sees them.
The binding is the opt-in.

| Event | Data |
|---|---|
| `lmgw.chat.frame` | `{event, data}`: a chat-turn frame verbatim (`turn`, `retrieval`, `delta`, `reasoning`, `tool`, `usage`, `stats`, `stop`, `state`, `error`, `done`) |
| `lmgw.chat.user` | `{message_id, content, voice}` |
| `lmgw.chat.reply` | `{message_id, content, unheard, voice}`, `{message_id, removed: true}` or `{message_id, skipped, voice}` (`voice` when the finalize wrote it, §8.3) |
| `lmgw.model.state` | §4.3 |
| `lmgw.chat.thread` | `{chat_thread: {id, title, temporary, admin_tools}}`: the thread as a response re-read it, when that differs from `session.lmgw.resolved.chat_thread` (WP8 review m10) |
| `lmgw.response.timing` | below |

`lmgw.response.timing` is realtime §11's timing line as data:

```json
{"response_id": "resp_…", "message_id": 812, "end_of_turn_ms": 412, "asr_ms": 31,
 "first_token_ms": 208, "first_clause_ms": 95, "first_audio_ms": 36, "total_ms": 4120,
 "to_first_audio_ms": 782, "cold": ["tts"],
 "models": {"asr": {"alias": "…", "answered_by": null}, "chat": {"alias": "…", "answered_by": "…"},
            "tts": {"alias": "…", "answered_by": null, "voice": "…"}}}
```

- Each stage is measured from the one before. `to_first_audio_ms` is their sum from the end of
  speech, and `cold` names the stages that loaded during the turn.
- The finalize stores this object in `voice.timing`.

The page parses `lmgw.chat.frame` into its existing `ChatEvent` (`pages/chat_stream.rs`), so the
same bubble code as a text send renders it.

*Built (WP8):*
- `lmgw.chat.frame` also carries `response_id` (`{response_id, event, data}`), so a frame of a
  response that was cancelled meanwhile is told from the next one's; frames are relayed whatever
  became of the response (past the generation filter), through `done`.
- `lmgw.model.state`: the connect warm's outcomes (each stage, `ready` with `ms: null` included —
  the page's loading state ends there), a turn's own `state` frames (once each — the WP8 build
  sent two per frame, review m2 — and relayed as frames too), and
  the TTS route's opening for a model that has to load (`loading`, then `ready` with its time or
  `fallback`).
- `lmgw.response.timing` is the journal's, sent once the reply slot finalized, so it names the
  reply's `message_id` (`null`: nothing saved); its numbers come from the same computation as the
  log line (`Timing::stages`), measured to the same instant. `first_clause: "announcement"` is
  `VoiceTiming::first_clause`, stored with the rest.

### 8.8 Usage and docs

- **Rows.**
  - Chat calls are recorded by the chat engine under the thread's labels, like its text turns.
  - ASR and TTS rows carry `realtime`, one TTS row per response.
  - The per-call key checks pass trivially, since a bound session is admin-only. The concurrency
    slot is taken as today.
  - *Fixed after the WP11 server review (M2):* a stopped plain (no-tools) turn — the page's Stop,
    a barge-in or `response.cancel`, the truncate-first stop, a leave mid-reply, a takeover —
    writes its row as the stock path's cooperative stop does: status 200, `canceled`, the
    upstream's usage or llama.cpp's counters where it sent them, else ~4 characters a token,
    named in the row's message (`proxy::stopped_usage`); a stop after the request went out and
    before the upstream answered counts the prompt (`unanswered_usage`). The stream is still
    dropped where it stands (`Turn::or_stop`), so `done {message_id, saved}` goes out at once;
    the relay keeps what it read outside the dropped future (`web/chat/stopped.rs`). The reply's
    own token counts and `done` keep what the upstream reported.
- **Docs.** The `/v1/realtime` `DocRoute` gains `chat_thread` (dashboard only), the extension
  events and table skipping. The new `/chat/api` routes are `DASHBOARD_BACKEND` exclusions.
  - *Built (WP8):* the query parameter is documented from `RealtimeQuery::chat_thread`'s own doc
    (the schema the DocRoute derives), and the description names the binding's refusals, what the
    thread owns, the takeover, the announcements and each `lmgw.*` event's shape.
- *Stated after the WP8 review (NITs left as they are):*
  - The takeover happens at the bind, before the 101 (NIT 2): an upgrade that then fails, or a
    client that drops mid-handshake, has already closed the other window's session. Binding
    after the 101 would let a refused bind leak a session; the owner re-enters.
  - `voice_bound` is read outside the slot lock (NIT 3): a send racing a bind gets
    `chat_thread_not_found` on its next response at worst.
  - `session.created` of a bound session shows realtime's `instructions` and the session's own
    seed (NIT 8); a bound turn uses neither — the thread's prompt and seed are the turn's.
  - For WP9 (NIT 10): a push-to-talk `response.cancel` of a reply nobody heard does not owe its
    turn again (only a barge-in's `interrupt` does), so a tap then gets `empty_turn` until new
    words are said; server VAD re-answers by itself.
  - For WP9 (NIT 11): a re-answer (a response with no new words, owed turns only) sends no
    `turn` frame — its `user_message_id` is none — so the bubble code must not wait for one.
  - The binding's refusals (403/400/404/409) write no request row (NIT 12), unlike realtime's
    anonymous cross-origin refusal; they are the dashboard's own requests.
  - Fixed in the same batch: a cancelled response's late `done` and plan reach the stored timing
    through the responder's save (NIT 6), and a user write lets go of the text of the replies it
    made un-re-cuttable (NIT 7).

## 9. Realtime mode (UI)

### 9.1 Layout

A **voice** button in the composer row swaps the composer for the realtime panel. It is disabled,
with a one-line reason, while a reply streams, when `voice_resolved.problems` lists a missing ASR
or TTS, or when `voice_resolved.realtime.ok` is false ("Voice mode is not available in Admin
Chat").

The message list stays above the panel, and turns appear there as they are written. The panel
(`pages/chat_voice/realtime/panel.rs`) has, from the top:
- the visualisation (§10);
- a captions line: the user's transcript, then the reply's spoken transcript from
  `response.output_audio_transcript.delta`. Those deltas are paced with the audio, so the
  captions follow the voice;
- the controls: mute, push-to-talk / automatic, stop talking, the chips, leave;
- a status line for loading, hold, fallback and errors.
- *Built (WP9):* `pages/chat_voice/realtime.rs` (+ `realtime/{live,machine,protocol,socket,
  bubbles,keys,panel,chips}.rs`) and `pages/chat_voice/viz.rs`. The panel's rows, from the top:
  the visualisation with its variant menu and the focus toggle in a corner, the captions line,
  the chips, the controls (state chip, session clock, mute, Auto | Push to talk with a hold-to-talk
  Talk button for the pointer, stop talking, leave), and the status line, which also carries the
  last turn's timing readout and an end's reason with **Re-enter**. The composer and what belongs
  to it (its status line, draft chips) are hidden, never dropped, so its text and dictation mark
  wait (`composer-area`, `display: contents | none`). **Focus view** (the owner's ruling: the orb
  there): the panel takes the column and the transcript and stats row step aside
  (`.chat-main:has(.rt-dock.focus)`); it resets to the chat panel on leaving. The disabled
  button keeps its focus and tooltip (`aria-disabled`, `data-disabled-reason`, the reason as
  `aria-description`); a press says the reason on the composer's status line. It is also
  disabled before the thread's resolution is read, and outside a secure context.
- *WP9:* **a refused handshake is invisible to the page**: the WebSocket API hides the HTTP status
  of a failed upgrade (§8.1's 403/400/404/409), so a socket that closes before it opened makes the
  panel read the thread again — gone (404): "this conversation no longer exists"; `kind ==
  "admin"`: the Admin Chat reason (both leave voice mode with a note on the composer) — else it
  says the session could not be opened and offers Re-enter. *Fixed in the WP9 fixes:* the 404 is
  told by the error's `code` (`not_found`), not by "404" in its message, which never held it; a
  re-read that lands after the user left or entered again changes nothing (review m6).
- *WP9:* the stage behind the visualisation is `--rt-bg` (`#121518`) in both themes (in the
  light theme a dark screen inside the light panel), so the glow reads as in the sample.

### 9.2 Controls and keys

| Control | Key | Effect |
|---|---|---|
| Push-to-talk | Space, held | `input_audio_buffer.clear`, then the pre-roll and live chunks while held; on release `commit` + `response.create`. While the assistant speaks, it first stops it. |
| Automatic | — | `turn_detection` from the thread's resolved value |
| Mute | M | `track.enabled = false`: silence flows, so an open turn ends naturally |
| Stop talking | Space in automatic mode, while it speaks | flush the player; `truncate {audio_end_ms}` with the samples heard, then `response.cancel` only when the truncate did not stop the response (below) |
| Leave | Esc, with no dialog open | close the socket, release the mic, restore the composer |

- Space and M act only when focus is on the panel or the page body, never in an input. Panel
  buttons do not keep focus after a pointer click, so Space never re-activates one.
- On `speech_started` while audio plays (a server barge-in), the page does what `@openai/agents`
  does: it flushes the player and sends `truncate` with the samples played. The cut is then exact,
  not pacing's estimate.
- *Built (WP9):* the keys are `realtime/keys.rs` (unit tested). Esc too acts only with the focus
  on the panel or the page, never in a text field (the thread settings drawer keeps its Esc), and
  "a dialog open" counts only a `<dialog open>` or `:popover-open` element that is shown — the
  composer's voice menu left open when voice mode came stays `:popover-open` inside the hidden
  composer in WebKitGTK (found by the WebKit probe). A focused element that is not shown is the
  page: WebKitGTK keeps the hidden composer's text box as `document.activeElement`; entering also
  blurs it. A held Space's auto-repeat is swallowed (it never presses a focused button), the window
  losing focus ends a held push-to-talk as Space's key-up would, and in automatic mode Space acts
  only while the voice speaks. Push-to-talk over the voice stops it first (flush, truncate) and
  then listens. The truncate carries `heard` (§11.1), and stop talking cancels only a
  response that is still open (one that is done only plays out what it said).
- *Changed in the WP9 fixes (review NIT 13):* a stop sends the **truncate first**. While the
  reply's item is still being produced, the gateway's truncate cancels the rest of the response in
  the same step (`lifecycle/truncate.rs`), so the reply's slot gets the exact cut in one write and
  one `lmgw.chat.reply`, where cancel-then-truncate made the late truncate's re-cut the normal
  path (two writes, two replies). `response.cancel` follows only for a response the truncate does
  not stop: nothing of it heard yet (no truncate), or its item already complete while it is still
  open (`machine::cancel_after_truncate`, unit tested). A `response_cancel_not_active` answer (the
  response ended by itself meanwhile) is not shown. **Leaving** (Esc, Leave, another thread, the
  page going) sends the same truncate for the reply still playing before the socket closes, so
  the thread keeps what was heard, not what was sent (review m9; the gateway reads the frames
  before a close first, IT `a_truncate_just_before_the_close_cuts_the_reply`).
- *Fixed in the WP9 fixes:* AltGr + Space (EurKEY's Right Alt) is no Space (NIT 1); the session
  echo's turn detection is applied only when it answers the last `session.update` sent —
  `session.created` comes before the page's first update and said nothing the page chose, so
  push-to-talk chosen while connecting used to be undone (NIT 4); Space pressed while the
  microphone still opens opens its gate when it lands, and a release before it landed commits
  nothing (NIT 5); `input_audio_buffer.cleared` ends a turn that switching to push-to-talk ended
  without a commit, so the panel goes idle instead of staying in `thinking` (review m4), as does
  `turn_detection_unavailable`; Stop is offered only while a response exists (NIT 8).
- *WP9:* the automatic mode's turn detection is the thread's resolved value (`server_vad` or
  `semantic_vad`; a thread resolved to `push_to_talk` uses `semantic_vad` when switched to
  automatic); the session echo's `turn_detection` decides which mode the panel shows. Push-to-talk's
  pre-roll is the owner's `realtime.prefix_padding_ms` (read from `/api/settings-full`), or the
  session's `server_vad.prefix_padding_ms` when it echoes one. A change of the window's microphone
  or echo mode while a session runs reopens the capture and sends `half_duplex` again.

- *Fixed after the WP11 UI review:* **entering focuses the panel** (`tabindex="-1"`, so Space, M
  and Esc act there) and its live region says "voice mode on: M mutes, Esc leaves; …" a frame
  after the mount, where the region is already in the page (m5); a dictation that was recording
  is said discarded on the panel's line, the one that shows. The composer's open popovers close
  on entry (the voice menu's kept a running Test microphone beside the session, NIT 5). The Talk
  button takes the pointer's capture, so drifting off it no longer ends the utterance (NIT 6).
  The end's note is an alert beside the status region, not inside it (NIT 7); the disabled
  voice-mode button and the microphone outside a secure context (now `aria-disabled`, so its
  reason is reachable) name their reason with `aria-describedby`.

### 9.3 States

| State | Entered on | Left on |
|---|---|---|
| `idle` | open, drained, or after `interrupted` with no speech | speech, PTT down |
| `listening` | `speech_started`, PTT down | `speech_stopped`, `committed`, PTT up |
| `thinking` | commit: ASR, LLM, first clause, tool runs | the response's first audio delta |
| `speaking` | the player holds the current response's samples | drained, cancel |
| `interrupted` | a barge-in or stop that cut heard audio | after `INTERRUPTED_MS` (600, visual) → `listening` or `idle` |

Flags beside the state: `muted`, `loading` (an alias), `held`. The panel root carries
`data-voice-state` for the drive tests.
- *Built (WP9):* the state is **derived** from a few facts (`realtime/machine.rs`, unit tested):
  interrupted (for `INTERRUPTED_MS` after a cut that cut heard audio) → listening (the server
  hears speech, or push-to-talk held) → speaking (a response's audio is in the player) → thinking
  (a turn committed or its speech stopped, a response open) → idle. An event that comes late or
  twice cannot strand the panel. **Speaking ends when the response's item has played out**
  (`Player::end` of the item after `response.output_audio.done`, or after `response.done` for a
  cancelled one, which gets no audio `done`), never at an underrun; and a response whose audio
  played out no longer counts as thinking — measured live: a bound response is done only once
  pacing's modelled playback drained, after the page's own end, which showed a blip of `thinking`
  between `speaking` and `idle`. `loading` is a busy model note on the panel's status line,
  `held` a block on the thread's ASR or TTS. The root also carries `data-voice-phase`, `-mic`,
  `-ptt`, `-muted` and the session's counters (`-chunks`, `-audio-bytes`, `-played`,
  `-truncated` (heard, ms), `-cut-played`, `-truncated-ack` (the server's
  `conversation.item.truncated`), `-truncates`, `-commits`, `-heard`, `-spoken`).
- *Fixed in the WP9 fixes (review m3):* a tool turn is one audio item, so its spoken preamble made
  it `speaking` for the whole tool call. The player now tells the item's owner when its audio ran
  dry before its end (the worklet's `underrun`, `player/starve.rs`) and when audio came again;
  while a tool runs and the item is dry, the state is `thinking`. A gap between two clauses with
  no tool running stays `speaking` (the reply is mid-sentence, and a slow voice would flicker).
- *Accessibility (review m8):* the status line is `role="status"` (its timing readout excluded),
  an end's note `role="alert"`; the captions line is not announced (it changes with every paced
  word): a hidden polite region says the user's final words and a cut ("Interrupted").

### 9.4 Chips

- **Chat model, ASR, TTS + voice**, from the thread JSON. The model chip opens the thread's model
  picker; the voice chip opens the TTS and voice picker. Both write thread settings, which apply
  from the next turn.
- **Echo**: the window's mode, with §12.2's warning when it applies.
- **Tools**, when the thread has tools; the running tool shows in the status line. When the
  thread carries the self-admin toolset and `self_admin` is `full`, the chip is amber and says
  "admin tools: can change lmgw". It informs and does not block.
- **Cloud mark** on any alias that is not local. In amber, the alias that actually served, when a
  fallback answered.
- *Built (WP9):* `realtime/chips.rs`. The model chip opens a `ModelPicker` on the page's own
  model choice (`model_sel`), which the page saves on the thread exactly as the header's picker
  does; its cloud mark comes from the catalog's `local`. The TTS chip shows the voice (the
  thread's, else what realtime's chain starts from) and opens the text-to-speech and voice picker,
  which writes the thread's whole `voice` (the seed left to the server) and keeps the settings
  drawer's draft in step. The STT and TTS chips turn amber when a block swaps them now
  (`Block::blocks`) and name the alias that answered when a fallback did (`lmgw.model.state
  {fallback}`, the timing's `answered_by`). The echo chip opens the four modes in a popover
  (`EchoChip` gained `expanded`). The admin chip reads `session.lmgw.resolved.chat_thread.
  admin_tools` once the session spoke (refreshed by `lmgw.chat.thread`), the thread's
  `voice_resolved.realtime.admin_tools` before.

### 9.5 Bubbles and timing

- `lmgw.chat.user` adds the user bubble with its mic badge.
- `lmgw.chat.frame` frames stream the reply as a text send does: the generated text, faster than
  speech.
- `lmgw.chat.reply` re-renders the reply from the stored row: heard text, greyed rest, timing.
  `removed` takes the bubble out; `skipped` adds a small note.

**The timing readout** sits under each spoken reply as one collapsed line: "ASR 31 ms · first token
208 ms · first audio 782 ms · nemotron → gemma4-e4b-voice → pocket-tts", with the fallbacks named.
It expands to every stage and the cold starts. Dictation shows `asr_ms` on the inserted text's
tooltip, and read-aloud shows `first_audio_ms` on the speaker button's.
- *Built (WP9):* `realtime/bubbles.rs`. `lmgw.chat.frame` is parsed by `chat_stream::frame` (the
  SSE parser's own table, now shared) into the page's `ChatEvent`; the reply bubble is made at a
  response's first frame with the provisional `voice {via: realtime}` (§9.7.4); a `state` frame
  goes to the panel's status line; the running tool shows there too. `skipped` keeps the text
  whole and shows "not cut to what was heard: another turn started" beside the speaker badge (a
  page-only `MsgVoice::note`). When a session ends (left, or ended by itself) the page reads the
  thread's messages back from the server, so the transcript is exactly the stored rows. The
  panel's own readout is the last turn's timing line, one line that expands to every stage.
- *Changed in the WP9 fixes (review m5):* the read-back used to come at once and 1.5 s later — a
  guess at the drain, too short when the last transcript is made on the CPU — and it replaced the
  messages of a session re-entered meanwhile. Now, on leaving, it comes at once and when the
  browser says the socket closed: the gateway answers the page's close only once its journal
  drained (IT `the_server_s_close_comes_once_the_journal_drained`). A session that ended by itself
  (no close of the page's own marks the drain) is read again each second until two reads agree.
  A read lands only while nothing newer owns the transcript: the same thread, no text reply
  streaming, no voice session entered since (`realtime/reload.rs`). The logic left `chat.rs`
  (NIT 11), which passes only its `load_msgs`.
- *Changed after the WP11 reviews (UI m1, m2, NIT 3, NIT 4; server: the close comes after the
  drain mid-reply too):* the read at once is gone: a session the page closes (leaving, and an end
  of its own — the microphone ended, the page hidden) is read **once, at the gateway's close**,
  for the session count taken when it ended, so a re-entered session's messages are never
  replaced by its predecessor's read. A close that never comes (the network) is waited for two
  `realtime.ping_interval_s` (the gateway's own bound on its end) and 10 s more, or 60 s with no
  ping bound; then the thread is read anyway, Keep stops waiting and the line says so, and a
  close that still comes reads again. A session the gateway closed is read until two reads agree
  and once more after that wait (a last transcript made on the CPU). The rows are **patched in
  place** (`reload::plan`): matched to the bubbles by id, a matched bubble takes the row's text,
  voice, reasoning, tokens and model where they differ, a bubble the store lacks goes (a refused
  turn's), rows past the last one shown are added (a turn transcribed during the drain); only a
  different order loads afresh. A read that changes nothing re-mounts nothing: an open
  `<details>` stays open, the scroll stays. A text reply streaming in another thread no longer
  drops the read (NIT 3). A turn whose transcript the gateway dropped at its end (§8.6) leaves no
  bubble behind.
- *Fixed in the WP9 fixes:* a chat turn's `state` frame comes twice (`lmgw.model.state` and
  `lmgw.chat.frame {state}`, `thread/hooks.rs`); the page applies the former only (NIT 12).
- *Fixed in the WP9 fixes (a WP10 note):* the timing line breaks only between its items, never
  inside a model's name ("audio/supertonic-" / "3-q8-0"); a name too long for the line is
  ellipsized, its full text in the title (`VoiceTiming::line_items`).

### 9.6 Leaving, temporary chats

- Leaving the thread, the page or the window ends the mode: socket closed, mic released, player
  flushed.
- There is one realtime session per page.
- Temporary chats get the whole feature, unsaved.
- *Built (WP9):* the temporary chat's Keep is disabled in voice mode, saying "leave voice mode
  first". *Fixed in the WP9 fixes (NIT 9):* it stays disabled after leaving until the gateway
  closed its side of the session (the thread is bound until its journal drained), saying "the
  voice session is still closing"; a Keep refused with `409 voice_session_active` says the same. **Nothing reconnects by itself** (§8.6): a takeover (`error chat_thread_taken_over`,
  close 4000), the network, the gateway ending the session or a microphone that ended (§11.2's
  `CaptureEvent::Ended`) release everything at once and leave the panel with the reason and
  Re-enter; `chat_thread_admin` and `chat_thread_not_found` leave voice mode with the reason on the
  composer's status line.

### 9.7 What the page's dictation and read-aloud ask of voice mode (WP7 review notes)

WP7 built dictation and read-aloud into the same page; voice mode must live beside them:
1. **The page-wide Right Ctrl listeners are off in voice mode.** `page.rs::install_keys` (keys,
   wheel, pointer, blur) would otherwise open a second capture beside the session: gate it on a
   "voice mode active" signal (or give `Dictation` an `enabled` flag). Entering voice mode calls
   `dictation.cancel(..)`.
2. **Entering voice mode stops read-aloud explicitly** (`read_aloud.stop()`), and a realtime
   response takes the player with `Player::begin_owned`, so whatever else takes it later (the test
   tone) is noticed by the session as by a read-aloud (§11.1).
3. **The voice button joins the composer's voice group** (§6.5, WP7 review M3): the split control
   beside the microphone, not a sixth icon in the row; `scripts/ui-matrix.py`'s `composer_input`
   check stays green at every size, thread settings open and closed.
   *Built (WP9):* the group is microphone · voice mode · voice menu. The composer's narrow
   breakpoint moved from 560 to 640 px so the text box keeps half the composer beside one more
   button: `composer_input` is green at all seven sizes, thread settings closed and open (the
   tightest: 324 of 605 px at 1440×900 with the drawer open).
4. **Chips while streaming.** A voice-mode reply renders its cues as chips only when the message
   carries `voice.via == "realtime"`: set a provisional `MsgVoice {via: "realtime"}` on the reply
   bubble at the first `lmgw.chat.frame`, or its brackets show raw while it streams and jump to
   chips at `lmgw.chat.reply`.
5. **`MsgVoice::is_spoken_reply` is made role-aware** (`spoken.rs`) before any user text is
   rendered with chips: a user turn spoken in voice mode has `via: "realtime"` too.
6. **The block helper is shared.** §9.4's cloud mark and amber use `state.rs`'s `Block`,
   `under_block` and `Block::blocks` (the GPU hold and a benchmark run's lease, CPU rows under a
   lease included), as the composer's controls do.
7. **The realtime status line keeps its own notes.** `VoiceStatus::clear_info()` runs on every
   turn and press; a realtime line sharing the page's `VoiceStatus` would lose its Info notes on
   each. Give it its own keys or its own instance.
8. **No `speak` in voice mode.** `PageVoice::for_turn` adds `speak: true` from the thread's
   resolved `read_aloud`; a text turn sent while voice mode is on must not ask for it, or the reply
   is spoken twice.

## 10. The visualisation component contract

Each variant is one ES module, `crates/lmgw-ui/assets/voice/viz/<variant>.js`:

```js
export function mount(canvas, inputs) // → handle
//   inputs.output         AnalyserNode          TTS playback tap (post-gain, pre-destination)
//   inputs.input          AnalyserNode | null   mic tap; null while the mic is closed
//   inputs.palette        {bg, fg, accent, accent2, warn, muted}  colours from app.css tokens
//   inputs.reducedMotion  boolean
handle.setState(state, info)  // "idle"|"listening"|"thinking"|"speaking"|"interrupted"
                              // info: {since: DOMHighResTimeStamp, muted, loading: string|null, held}
handle.setTiming(timing)      // optional: the lmgw.response.timing object
handle.resize(cssWidth, cssHeight, devicePixelRatio)
handle.destroy()
```

The rules:
- **Its own loop.** The variant owns its `requestAnimationFrame` loop, reads the analysers itself,
  and pauses while `document.hidden` or out of view. It may set `fftSize` and
  `smoothingTimeConstant`; only one variant runs at a time.
- **Rendering.** WebGL2 is allowed, with a 2D fallback. Nothing may need
  `WEBKIT_DISABLE_DMABUF_RENDERER`; the app window renders through the shell's NVIDIA rule.
- **Reduced motion** gets a calm, near-static rendering that still shows state and level.
- **Colours** are graphite and blue, never pastel.
- **Budget:** under 1 ms of main-thread work per frame on the dev box.

`pages/chat_voice/viz.rs` creates the canvas, imports the module named by `VIZ_VARIANT` (the
owner's pick), feeds `setState` from §9.3, and calls `destroy` on cleanup. Only the chosen variant
ships.
- *Changed by the owner's ruling (2026-10-03):* **all three variants ship and stay selectable**,
  a setting of the window (`localStorage` `lmgw.voice.viz` = `{panel, focus}`), the ribbon in the
  in-chat panel and the orb in the focus view by default; the variant menu sits over the
  visualisation's corner. There is no `VIZ_VARIANT`.
- *Built (WP9):* `assets/voice/viz/{engine,ribbon,orb,ring}.js`, served under `/voice/viz/` by
  Trunk's copy-dir (IT `ui_cache::the_voice_visualisation_modules_are_served_as_javascript`).
  Ported from the sample (`target/voice-viz/src/app.js` §1–§6) without changing a number:
  the analyser taps, the five-state machine with its eased 320 ms crossfades and the 120 ms
  barge-in cut, the colour mix, each renderer's tunables. `engine.js` is the shared part
  (`run(canvas, inputs, factory)`); the page decides when `interrupted` ends, so the sample's
  own `interruptHold` is gone. The import is the browser's `import()` through a wasm-bindgen
  snippet (no `eval`, so a future `script-src 'self'` keeps working; the snippet keeps its name
  across builds and revalidates).
- *Additions to the contract (WP9):* `inputs.palette` entries may be a `{deep, base, hi}` triple
  (the sample's look needs all three shades; a plain colour gets its shades mixed): the adapter
  passes the `--rt-accent*`, `--rt-think*`, `--rt-user*`, `--rt-idle*` tokens of app.css (since
  the WP11 UI fixes `assets/voice.css`, where the Chat's voice styles moved, review NIT 11) as
  `accent`, `accent2`, `warn`, `muted`. `handle.setInputs({output, input})` takes the analysers as
  they come and go (the microphone opens after the mount and reopens on a device change);
  `handle.stats()` answers `{kind, frames, ms, running}` for the probes. A variant sets the
  analysers' `fftSize` and smoothing while mounted and gives them back at `destroy`, which also
  cancels its loop, removes its listeners and observers, drops the backing store (`width = 0`) and
  loses the orb's WebGL2 context. The orb falls back to Canvas2D on a fresh canvas when WebGL2 is
  missing, a shader fails, or the context is lost mid-run. *Fixed in the WP9 fixes (NIT 10):* a
  shader that fails loses its canvas's context at once (not at the next garbage collection),
  `handle.kind` names the fallback from the swap on, and a pixel ratio that changes with no size
  change (another monitor, the page's zoom) re-sizes the backing store through a media query on
  the ratio in effect, while the host follows the window's ratio.
- *Measured (WP9):* headless Chrome, SwiftShader, 640×184: the ribbon 0.24 ms, the ring 0.07 ms,
  the orb (WebGL2) 0.03 ms of main-thread work per frame.

- *Changed after the WP11 UI review (NIT 8):* idle with both taps quiet for 1.5 s, and in reduced
  motion, the engine draws at 15 fps (`IDLE_FPS`); a state change, a voice on either tap, new
  inputs or a resize bring the display's rate back at once (measured by the viz harness: 15
  frames a second calm). `stats()` says `calm`. The panel follows a change of
  `prefers-reduced-motion` while it is open (the variant mounts afresh with it), and a module that
  does not load or mount is said on the stage, not only in the console.

## 11. Audio in the page

### 11.1 Playback

- **One context.** One `AudioContext({sampleRate: 24000})` per page, created at the first voice
  press and kept for the page's life. It is suspended after 10 s with nothing to play and resumed on
  demand. One context is one PipeWire stream to route.
- **The player.** `assets/voice/player-worklet.js` plays queued PCM16 chunks per item:
  - `push {item, pcm}`;
  - `flush`, answered with the samples played per item, which gives `truncate`'s `audio_end_ms`;
  - `progress` about every 50 ms, and `drained`.
- **The graph:** player → gain → `AnalyserNode` (the output tap) → destination.
- **Read-aloud streams** (WP4 review m9, §6.5): the player takes `speech` frames of one read-aloud
  at a time. Starting a playback first aborts the fetch of the previous one and flushes its item;
  the page reads each speech stream until `speech_done`, `speech_error` or EOF, never stopping at
  the text's `done`. Reading slower than playback is logged by the gateway (§6.3), so the page
  reads frames as they come and queues the audio in the player, not in the fetch.
- *Built (WP6):* `pages/chat_voice/audio/player.rs` (+ `player/{ledger,route}.rs`) and
  `assets/voice/player-worklet.js`.
  - The protocol gained an end: `push {item, pcm}` (an ArrayBuffer, transferred), `end {item}`,
    `flush {id, item?}` (one item or all); out come `progress {item, played, queued}` about every
    50 ms, `underrun {items}` when the queue runs dry while an item still waits for audio (a clause
    synthesised slower than real time), `drained` when it runs dry with nothing waiting (every item
    ended or flushed), `ended {item, pushed, played}` when an ended item has played out, and
    `flushed {id, items}`. Counts are samples at 24 kHz. The queue keeps the PCM16 as it came (no
    float copies) and has no cap. *WP9:* leave `speaking` on `ended` of the response's item, or on
    `drained`; an `underrun` is not the end (review m9).
  - **One playback per page** is enforced in one place: `Player::begin` is the only way to get an
    item, and it flushes whatever plays first (its `end` answers `None`, its later pushes are
    dropped). The test tone goes through it like a read-aloud or a realtime response (review m11).
    *WP7 fix (m7):* `Player::begin_owned(superseded)` also registers a callback that the next
    `begin` calls (cleared by `release` of the item): an owner that streams more audio — a
    read-aloud, WP9's realtime response — learns it lost the player.
  - `Player::push` takes PCM16-LE bytes on any boundary (an odd byte waits for the item's next
    chunk), `end` resolves when the item has played out (`None` only when a flush really dropped
    audio of it), `flush` answers the counts (waiting up to 1 s for the worklet, else the last
    progress).
  - **Barge-in accounting** (review M4, `player/ledger.rs`, tested natively; the worklet side in
    `scripts/worklet-check.mjs`): an item that ended or was flushed is closed — a push for it (the
    deltas still in flight when a barge-in flushed the response) is dropped and counted
    (`Player::late`), never played again; the final count of a closed item stays readable
    (`played(item) -> Option<u64>`) until the owner releases it or a later item closes; a flush of
    an item that has just played out answers its final count, and its `end` waiter gets the count.
  - **Heard, not rendered** (review m2): each count also carries `heard`, `played` less the output
    latency the browser reports (`baseLatency + outputLatency`, where it has them; for an item that
    played out, only what had not left the output yet). *WP9:* truncate's `audio_end_ms` is
    `heard`. The audio still in the device's buffer at a flush plays out and is counted as unheard
    (the user is already talking over it). Residual: buffering below the browser that it does not
    report — measured: WebKitGTK reports 128 samples (5 ms, `baseLatency` only) while its
    `pulsesink` and PipeWire's quantum add tens of ms; Chromium reports ~960 (40 ms).
  - The context is made synchronously by `player()`, so a press's handler makes it inside its
    gesture; its future resolves only once the context runs and its first route has been applied
    (review M1). A browser that holds it (no gesture reached the page) fails the press after 5 s
    with "the browser did not let lmgw's playback start", on the popover and the composer's
    devices button; a failed creation is retried on the next press. Its state follows the browser's
    own suspensions (`statechange`); a suspended → running change re-applies the route.
  - **Idle suspend** after 10 s with nothing queued and no push, unless held: `Player::hold_awake`
    returns a guard, which WP9 holds for a session and WP7 for a streaming read-aloud (review m8).

### 11.2 Capture

- **Opening.** `getUserMedia` with §12.1's constraints feeds a source in a capture context at the
  device's native rate. A 24 kHz capture context is avoided, because some browsers refuse a source
  whose rate differs from its context's.
- **The worklet.** `assets/voice/capture-worklet.js` resamples to 24 kHz (realtime) or 16 kHz
  (dictation) with a windowed-sinc low-pass and posts 40 ms PCM16 chunks.
  - It keeps a push-to-talk pre-roll as long as the session's `prefix_padding_ms`.
  - It is connected to the destination through a zero gain. WebKit renders by pulling from the
    destination, and a worklet that is not connected may never run.
- **The level tap.** A mic `AnalyserNode` on the source feeds the level meter and the
  visualisation.
- **Release** runs on every exit path (§9.6), on `pagehide`, and in the Leptos cleanup:
  `track.stop()` on every track, then the source, the worklet and the context closed.
- *Built (WP6):* `pages/chat_voice/audio/capture.rs` and `assets/voice/capture-worklet.js`.
  - The capture context runs at the track's `sampleRate` (`getSettings`), else the default rate.
    The graph is source → mic analyser → worklet → zero gain → destination, so the analyser is
    pulled too.
  - The resampler is a Blackman-windowed sinc, 24 zero crossings each side, cut off at 0.92 of the
    lower Nyquist frequency, normalised per output sample (DC gain 1). Measured offline
    (`scripts/worklet-check.mjs`, run by `ci/check.sh`): THD+N at 1 kHz −91 dB or better from 48
    and 44.1 kHz into 24 and 16 kHz, with 127-, 128- and 480-frame blocks; −0.0 dB at 10 kHz (into
    24 kHz), −0.9 dB at 7 kHz (into 16 kHz); every alias at least 50 dB down (12.1 kHz → 11.9 kHz,
    the worst), 72 dB from 300 Hz past the output's Nyquist frequency; the output count exactly the
    ideal after 20 min of 44.1 → 16 kHz. About 100 taps per output sample at 48 → 24 kHz and 290
    at 44.1 → 16 kHz, a delay of 1.1 and 3.3 ms (review n1: 8 crossings let 12.3 kHz alias at
    −16 dB).
  - The worklet also takes `flush` (the partial chunk, then `flushed`): `Capture::finish` stops
    the tracks first, then hands the last partial chunk over (dictation's release). That order is
    privacy first: audio still buffered in the browser when the tracks stop is not delivered; a
    worklet that does not answer within 500 ms is logged (review n5). `stop` and a drop stop the
    tracks at once.
  - **Push-to-talk's release** keeps the microphone open: closing the gate posts the partial
    chunk first, then answers `gated`, so the utterance ends to the sample and nothing of it moves
    into the next pre-roll; `Capture::close_gate().await` resolves after that (review m1).
  - **Ends nobody asked for** (review M3, `capture/watch.rs`) reach the owner as a
    `CaptureEvent`: `Muted`/`Unmuted` when the user agent mutes the track (it delivers silence; the
    capture keeps running and the owner says so), and `Ended(reason)` — the track ended (unplugged,
    failed, its node removed), the permission was taken back (`permissions.query` where the browser
    has it), the page was hidden (`pagehide`, the back/forward cache included: a page shown again
    starts with every capture closed), or the capture context stopped running. An ended capture is
    stopped first (every track, the graph), then reported once. The capture context must reach
    `running` within 3 s before the capture is handed over, else the open fails with "the
    microphone's audio context did not start". *WP7/WP9* show the reason and never upload or stream
    after an end.
  - A stored input whose device went away between the list and the open falls back to the system
    default with a note: "the chosen microphone could not be opened; using the system default"
    (review m3).
  - *Found by the WP6 probe:* `web_sys::MediaStream` has an inherent `clone()` — the JS
    `MediaStream.clone()`, a new stream of new live tracks — which shadows `Clone::clone`. Holding
    `stream.clone()` and stopping its tracks left the microphone open; the capture holds
    `Clone::clone(&stream)`.

### 11.3 Build

- **`web-sys` features**, in commented groups:
  - WebSocket: `WebSocket`, `MessageEvent`, `CloseEvent`, `BinaryType`;
  - media: `MediaDevices`, `MediaDeviceInfo`, `MediaDeviceKind`, `MediaStream`,
    `MediaStreamTrack`, `MediaStreamConstraints`, `MediaTrackConstraints`;
  - Web Audio: `AudioContext`, `AudioContextOptions`, `AudioContextState`, `BaseAudioContext`,
    `AudioNode`, `AudioParam`, `AudioDestinationNode`, `AudioWorklet`, `Worklet`,
    `AudioWorkletNode`, `AudioWorkletNodeOptions`, `MessagePort`, `AnalyserNode`, `GainNode`,
    `MediaStreamAudioSourceNode`;
  - and `HtmlCanvasElement`.
- **Assets.** `index.html` gains `<link data-trunk rel="copy-dir" href="assets/voice">`, so the
  worklets and the variant are served under `/voice/…`.
- **CSP.** `DASHBOARD_CSP` sets no `script-src`, `worker-src` or `connect-src`, so worklets and the
  same-origin WebSocket need no change. A future `script-src` must keep `'self'`.
- *Built (WP6):* also `MediaTrackSettings` (the track's rate), and `wasm-bindgen-futures` as a
  direct dependency (already in the tree) to await the media promises. The worklets are served
  unhashed with `no-cache` and a JavaScript type (IT `ui_cache::the_voice_worklets_…`).

### 11.4 Browser and app

- **Mic permission.** The app grants it without a prompt (§13.1); a browser shows its own prompt.
- **Secure context.** `http://127.0.0.1` and `localhost` are secure contexts; a LAN address over
  http is not. The voice controls say "voice needs https or localhost" there.
- **Output.** In the app, the shell routes playback (§12.3); in a browser, `AudioContext.setSinkId`
  where it exists (§12.4).

## 12. Echo modes and output devices

### 12.1 Echo modes

| Mode (`lmgw.voice.echo`) | Capture constraints | Session | Barge-in |
|---|---|---|---|
| `device`: the input device cancels echo (default) | `echoCancellation: false`; in Chrome also `noiseSuppression` and `autoGainControl` off | full duplex | yes |
| `browser`: browser echo cancellation | `echoCancellation: true` | full duplex | yes |
| `not_needed`: headphones or a headset | `echoCancellation: false` | full duplex | yes |
| `none`: no echo handling | `echoCancellation: false` (the browser's other processing keeps its defaults) | `half_duplex: true`, `echo_tail_ms` from settings | no; stop talking still works |

- `device` turns the browser's canceller off, because two cancellers in series hurt.
- `not_needed` uses the same constraints as `device`. It only skips §12.2's warning.
- `none` is a plain microphone with no processing of its own, so only the canceller is turned off
  (WP6 review n4: the code had turned noise suppression and gain control off too).
- WebKitGTK's `echoCancellation` was unverified, so `browser` was labelled "unverified in the app
  window" until a live check. *Verified (2026-10-04):* the owner used `browser` in the app window
  with their speakers and could barge in, so the label is gone; `label` keeps its `in_app` argument.

### 12.2 The device's echo reference

An input device that cancels echo itself takes a far-end reference. A PipeWire echo-cancel filter
chain typically takes the monitor of the default output (`default.audio.sink`). Some link that
reference only while something records from them, about 50 ms after a capture starts.
- **In `device` mode, lmgw must play on the system default output.** Otherwise barge-in fires on
  lmgw's own voice: realtime §6.4 measured ~111 false barge-ins per minute without cancellation.
  The default setup ("System default" output) needs no routing.
- **The warning.** In `device` mode, an output other than the system default shows an echo-chip
  warning: "your input cancels echo from the default output; lmgw plays on Headphones". It offers
  half duplex. There is no graph tracing.

### 12.3 Output selection in the app

WebKitGTK cannot choose an output, so the shell routes lmgw's own playback streams.

- **The commands are `async`** and run off the GTK main thread:
  - `audio_outputs` lists `Audio/Sink` nodes from one `pw-dump` run as
    `{outputs: [{name, description, serial, default}], note}`: `name` is `node.name` (stable, what
    the page stores as the device id), `description` falls back to `node.nick`, then the name;
    `default` marks `default.audio.sink`. Only `Audio/Sink` nodes are offered; an `Audio/Duplex`
    node is not;
  - `audio_output_set {sink: name | null}` finds lmgw's streams and sets `target.object` on them
    through the `default` metadata (`pw-metadata -n default -- <id> target.object <serial>
    Spa:Id`, the form WirePlumber writes itself), using the named sink's current serial. `null` writes
    `-1`, WirePlumber 0.5's "no defined target" (`find-defined-target.lua`): the stream follows the
    default sink, and `state-stream.lua` stores no target for it. *Changed in the WP6 fix:* `null`
    first deleted the key, which left a stream on the old sink when the stream carries a target of
    its own — WebKitGTK's `pulsesink` re-opens a resumed context's stream with an explicit
    `target.object`, the sink it last played on, and only the metadata overrides that (measured by
    `scripts/shell-check.py`'s popover check). The values go after `--`, since getopt would read
    `-1` as an option. It answers `{streams, sink, serial}`; an unknown name is refused ("output '<name>'
    is not present").
- **Which streams are lmgw's.** Every `Stream/Output/Audio` node whose process descends from the
  shell's pid **and** that is WebKit's (`application.process.binary` starting with `WebKit`, on the
  node or its client) or carries lmgw's stream id: a browser `xdg-open` started as the shell's
  child descends from it too, and is not lmgw's audio (review m2). The kernel-verified
  `pipewire.sec.pid` is used where it names the stream's own process: on a native stream's node, or
  else its client. A pipewire-pulse stream's `pipewire.sec.pid` is the pulse server's own pid
  (measured on the client; on the node too, should a release copy it there — review n1), and
  WebKitGTK plays through `pulsesink`, so its streams are told by the `application.process.id`
  libpulse reports. WebKit may open a stream for the capture context too, and every one of them
  moves.
- **Routing does not rely on WirePlumber remembering it.** The page calls `audio_output_set` each
  time it creates or resumes its playback context. One 2 s deadline bounds the whole call, its
  `pw-dump` and `pw-metadata` runs included (review m8): a stream that has not appeared is looked
  for again while another look fits before it, then the call fails with "output routing failed: no
  playback stream found (waited 1.9 s)", the time it actually waited. A write that fails after
  others went through says which moved ("output routing moved streams [60, 61], then failed on
  stream 65: …"). The metadata subject is the node's global id, which PipeWire recycles: a stream
  that closes between the dump and the write could pass its id on; that window, one `pw-metadata`
  start long, is accepted (review n2). WirePlumber does remember the target, under lmgw's key: a
  context opened later is linked to the remembered sink before the page asks again, and `null`
  clears the remembered target too. A choice made while no context exists is applied before the
  new context's first sample (§11.1), so a first use never plays its start on the old sink
  (review M1: WirePlumber has nothing remembered then).
- **Own identity, decided up front.** At the top of `main()`, with the NVIDIA variable and before
  any thread starts, the shell gives lmgw's streams `application.name = lmgw` and
  `application.id = lmgw` (the desktop file's name). A debug build uses `lmgw-dev` for both, so an
  output chosen in a dev window is never remembered for, or applied to, the installed app's streams
  (review m3). WirePlumber's restore-stream keys a stream by
  `application.id`, else `application.name` (`state-stream.lua`), so the id is what keeps the key
  lmgw's whatever a library sets. libpulse applies `PULSE_PROP_<key>` only to keys the application
  has not set, and GStreamer sets `application.name`, so the shell sets
  `PULSE_PROP_OVERRIDE_application.name` and `…_application.id`; a native sink reads
  `PIPEWIRE_PROPS`. An explicit `PULSE_PROP` (a whole proplist), `PULSE_PROP_[OVERRIDE_]application.id`
  or `…application.name`, or `PIPEWIRE_PROPS`, in the environment leaves that side alone; another
  `PULSE_PROP_<key>` (a session-wide `media.role`, say) does not (WP11 review n2). `xdg-open` (the
  browser it may start) and the `pw-*` runs do not inherit them. Every other child does, the
  gateway's included (an MCP stdio server, podman, git, the PDF tools): one that plays audio itself
  descends from the shell and carries lmgw's id, so it is routed and remembered as lmgw's. WebKit
  spawns its web processes on demand, so the variables cannot be dropped once it runs (WP11 review
  n1: said, not scrubbed). The installed app's
  streams were keyed `Output/Audio:application.name:lmgw` before; that entry only holds a volume,
  so lmgw's volume in the desktop mixer resets once after the update that brings this (review n10:
  say so in the release notes).
- **Without pipewire-utils** the list says "system default only: pipewire-utils not found". There
  is no hard RPM dependency. The page then routes nothing: "System default" plays there quietly,
  and a chosen output says once, as the route's status, that it cannot be used (WP6 review m4).
- **Verified in WP5** (`scripts/shell-check.py`, a private PipeWire graph with two null sinks):
  the list and default; the refusal with no stream; the identity on a silent context's stream;
  every stream moved and relinked; the remembered target under `Output/Audio:application.id:lmgw`
  (`lmgw-dev` in the debug shell the check runs);
  a later context following it; another WebKitGTK app started afterwards keyed apart and left on
  the default; `null` (writing `-1` since the WP6 fix) sending every stream back and clearing the
  memory; an unknown sink refused.

### 12.4 Output selection in a browser

The browser path uses `enumerateDevices()` `audiooutput` entries and `AudioContext.setSinkId`
(Chromium). Without it (Firefox), there is one entry: "System default (this browser cannot choose
an output)".
- *Built (WP6):* support is `"setSinkId" in AudioContext.prototype`. The context is made with the
  stored output's id as `sinkId` in `AudioContextOptions`, so its first sample plays there (review
  M1); a refused id makes it on the default, and the first route says so. A stored output the
  browser refuses (gone since) falls back to `setSinkId("")` and says so ("the chosen output could
  not be used …; playing on the system default"). A plain WebKitGTK view has no `setSinkId`: one
  entry. The entry Chrome's `default` alias stands for (by its label "Default - …", else the one
  entry sharing its `groupId`) is marked as the system default, so choosing it does not raise
  §12.2's warning (review m6).

## 13. The Tauri shell

### 13.1 Microphone permission

`src-tauri/src/media.rs` is Linux only and wired in `main.rs` after the window is built, through
`WebviewWindow::with_webview` → `webkit2gtk::WebView::connect_permission_request`. Each decision
is logged with the origin (`media permission: … granted|denied for <origin>`).
- **`UserMediaPermissionRequest`** is allowed only when both hold:
  - it is for audio and not for video or display capture
    (`webkit_user_media_permission_is_for_audio_device` / `…_video_device`; `…_display_device`
    through `webkit2gtk-sys`, which the safe 2.0.2 binding lacks);
  - the window's document is at the gateway: the running document's URI **and** the webview's
    active URI both have **exactly** the gateway's scheme, host and port. This is not
    `navigation_allowed`, which admits agent-app hosts. *Changed in the WP11 fix (review m3):*
    the gateway's origin is the one this process serves at that moment
    (`src-tauri/src/gateway.rs`), read at every decision, by the navigation filter and the
    command gate too: none while its bind failed or a restart is under way, the new one once a
    restart moved it. It used to be the origin the window was built for, which after a
    `bind_addr` change and "Restart gateway" named a port lmgw no longer owned.

  Anything else is denied.
- **Which document** (review M1). `webkit_web_view_get_uri` is the *active* URI: from the start of a
  main-frame load it is the provisional URL, while the old document runs until the new one commits
  — and keeps running when it never does (a download, a 204, `window.stop()`). An agent app's
  document (the window may show one top-level) could navigate to the gateway and ask in that
  window. The running document's URI is therefore tracked from `load-changed` (committed, or
  finished with no other load under way; at install, the view's URI when it is idle), and a request
  whose two URIs differ is denied with "navigation in progress". Unit-tested as a decision table
  (`media::tests`), live by `shell-check.py` (§13.3).
- **What remains** (not measured): WebKitGTK's request carries no origin and no frame, so the
  handler judges the window's state when the UI process handles the request. Across a process swap
  the old document's request and the new document's commit come from different web processes, and
  their order there is not guaranteed; a request the old document sent just before a gateway
  document committed could be judged after it. The old document is gone by then (suspended at
  most), so what the grant reaches is unclear; WebKit's per-document memory allows one try.
- **`DeviceInfoPermissionRequest`** is allowed on the same origin, so device labels and stable ids
  appear.
- **Every other request** returns `false`, WebKit's default (deny), as today.
- **Frames.** Agent app frames and the sandboxed HTML preview frame run inside the window, and the
  handler sees only the top-level URI. WebKit's Permissions Policy keeps cross-origin and opaque
  frames off the mic unless the parent grants `allow="microphone"`. The UI never does, and a unit
  test (`media::tests::the_dashboard_never_delegates_a_frame_permission`) reads the UI's sources
  and assets for any `allow` attribute — case-blind, and covering `setAttribute`/`setAttributeNS`,
  d3's `.attr`, bracketed properties (`f["allow"]`) and object keys (`{allow: …}`). Measured on 2.54
  in the shell: both frame kinds are denied before the handler is asked, and a cross-origin frame
  given `allow="microphone"` reaches the handler under the gateway's URI and is granted — the test
  is what stands between them.
- **Permissions-Policy header** (review m1). The dashboard's HTML carries
  `Permissions-Policy: microphone=(self), camera=(), display-capture=()` beside its CSP
  (`web/ui.rs`). Measured: the page's own capture still works with it in WebKitGTK and Chromium
  (`webkit-check.py --media` 36/36 and `media-probe-chrome.sh` 37/37, as before), and WebKitGTK 2.54
  still grants the `allow="microphone"` frame with it. So in the app the source scan remains the
  only guard against delegation; the header covers a browser that applies it (not measured here).
- **No same-origin frame may show untrusted content.** The default allowlist is `self`, so a
  same-origin frame needs no `allow` to reach the microphone. Today attachments are served with
  `CSP: sandbox`, the preview frame is sandboxed without `allow-same-origin`, markdown escapes raw
  HTML, and Mermaid runs `securityLevel: strict` with DOMPurify forbidding `iframe`.
- **What WebKitGTK 2.54 does around the handler** (measured, WP5):
  - mock capture devices still go through the handler (`MockCaptureDevicesPrompt` is on);
  - it never emitted a `DeviceInfoPermissionRequest`: `enumerateDevices` hides labels and ids until
    a capture has been granted in the document, then shows them. The handler still grants that
    request on the gateway origin, should a later WebKit send it. WP6's device list therefore names
    the devices only after the first grant, and says so before it;
  - it remembers a frame's denials: once a request including video is denied, a later request
    including the same kind is denied without asking. The UI asks for audio only;
  - `getDisplayMedia` needs a user gesture before it reaches the handler, which denies it.
- **Dependencies:** `webkit2gtk = "2.0"` (locked 2.0.2, features `v2_38` as wry), under the Linux
  target. The display-device check is the C function through the crate's `ffi` re-export
  (webkit2gtk-sys), so there is no second dependency.
- **On a Tauri bump**, check whether tauri-runtime-wry now installs a permission handler of its
  own; 2.12 does. If it does, use its API instead of a second handler.

### 13.2 Audio commands

`audio_outputs` and `audio_output_set` live in `src-tauri/src/audio_out.rs` (the `pw-dump`
reading in `audio_out/graph.rs`).
- They are declared in `build.rs`'s app manifest (`tauri_build::AppManifest::commands`). The
  generated `allow-*`/`deny-*` permission files land in `src-tauri/permissions/autogenerated`,
  which is gitignored.
- Their `allow-*` permissions join `capabilities/remote-ui.json`.
- The page calls them through `ui_scale::tauri_invoke()`: `invoke("audio_outputs")` and
  `invoke("audio_output_set", {sink})`. Errors arrive as the rejection's string.
- **The same origin rule as the microphone** (review M1). The capability's check reads only the
  webview's active URI, the provisional one during a navigation, and Tauri gives every top-level
  document of any origin `__TAURI_INTERNALS__` and the invoke key. So the shell's invoke handler
  (`main.rs`) asks `media::command_allowed` — §13.1's two-URI rule — before either command and
  rejects with `refused: <why>`. Tauri calls it on the GTK main thread as each message arrives (wry's
  script-message and IPC-scheme handlers both run there), the thread `load-changed` runs on, so no
  commit falls between the message and the answer; off that thread the answer is no. What remains:
  the cross-process ordering of §13.1 (an invoke the old document sent just before a gateway
  document committed is judged after the commit; it runs, and its reply goes to the new document),
  and Tauri's own and plugin commands (window controls, zoom, `dialog:allow-open`), which only the
  capability's active-URI check guards, as before WP5: their reach is the window itself and a file
  dialog the user still has to confirm.

### 13.3 Dev and test launches

Added in WP5, so a dev shell cannot disturb the installed app and capture can be tested without a
microphone.
- **A debug build keeps away from the installed app** (`src-tauri/src/dev_guard.rs`). It registers
  its single-instance name as `<identifier>.dev.d<hash of its data dir>`, so it neither hands over
  to nor raises the installed window, and a second dev shell on another data dir runs beside the
  first; it needs `LMGW_DATA_DIR` and refuses the installed app's data dir (also as
  `~/.local/share/lmgw` when `XDG_DATA_HOME` points elsewhere); it refuses a data dir still on the
  production container prefix (seed one with `scripts/dev-instance.sh` or
  `scripts/dev-copy.sh copy`); it runs no updater (the updater downloads to the installed app's path
  and installs over it; the tray item reads "Updates: off in a debug build"); and it keeps WebKit's
  store (cookies, local storage, cache) in `<data dir>/webview`. The shared store was found the
  hard way: a cookie ignores the port, so a dev window's login rewrote the installed window's
  `lmgw_session` in the shared jar. Its tray writes the icon's files into `<data dir>/tray-icon`
  and its tooltip reads `lmgw (dev) — <data dir>` (WP11 review m2): tray-icon's default dir
  (`$XDG_RUNTIME_DIR/tray-icon`) and the icon id are the installed app's, and every icon change
  deletes the previous file, so a dev quit or hold toggle deleted the installed tray's icon. A
  release build behaves as before. `cfg!(debug_assertions)` is the switch, so the release profile
  must keep debug assertions off (`Cargo.toml` says so). The installed-dir refusal is
  `lmgw_core::state::is_installed_data_dir`, which the headless runner uses too (WP11 review m4),
  and an empty `LMGW_DATA_DIR` reads as unset in both.
- **The window is built only on a port this process holds** (WP11 review m3). `server::bind`
  reports the bind before anything is served. When it fails, a debug build refuses to start
  (beside `dev-copy.sh start` on the same dir, or on a hand-made copy that kept production's
  `bind_addr`, it would otherwise show that gateway, with the microphone granted to it); a release
  build keeps its tray, builds no window and says why in a dialog. "Restart gateway" stops the
  server — from then on the old origin is nobody's — waits for its listener to close
  (`server::serve`'s release signal, so the same port binds again at once), and binds `bind_addr`
  again: the window stays when the origin is the same, is sent to a fresh login (a new nonce) on
  the new origin when it moved, and is closed with the reason when nothing could be bound. The
  window's host is loopback for a wildcard bind and the bound address otherwise. Tested: the
  decision (`gateway::tests`, `media::tests`), `server::bind`/`serve` and the release
  (`tests/it/dev_boot_prefix.rs`), and live, `scripts/shell-check.py` holding the run's port with a
  listener of its own while it starts the debug shell (exit with the refusal, no window, no podman
  call).
- **No container is touched before the dev prefix applies** (review B1). `AppState::init_with`
  lists and removes nothing; `server::run` spawns every pass that does (model, benchmark and agent
  reconciliation, the run-dir sweep), after the entry point's step, and refuses a dev instance still
  on the production prefix. `tests/it/dev_boot_prefix.rs` proves it on a fresh data dir with a
  podman holding the installed app's containers. The settings write refuses to move a dev instance
  onto that prefix too, with the same message (WP11 review m1: its next model start would
  `--replace` the installed app's container of the same name).
- **A dev instance writes into no models dir outside its own data dir** (WP11 review m6, the
  owner's ruling B of 2026-10-04, built on main after the merge). A dev copy keeps production's
  models dirs so it can run the owner's models (mounted read-only), so one predicate,
  `config::dev_models_dir_refusal` (resolved paths, beside `dev_prefix_refusal`), is asked at every
  write lmgw makes there: Hugging Face downloads (`hf_add`, retry, update and re-download, recipe and
  audio catalog installs, and every transfer, so a row boot resumes too), the delete of a tracked
  download (asked before the row goes), and the Audio lab's clip upload, delete, transcript and
  transcription. Each answers `400 dev_shared_models_dir` with the rule and the way out (point the
  copy's models dir at a folder inside its data dir); the string-typed ops carry the code as the
  message's last word, which `ops_result` maps. The image class is not refused: saving it creates
  no LoRA or upscaler dir there and says so in the save's notes, and a start goes ahead without them
  (`AcquireSpec::may_write_models_dir`), logging once. A row's own extra run args and re-enabling a
  copied agent stay open, because each is a deliberate step; `dev-copy.sh copy` names both. Tested
  in `tests/it/dev_models_dir.rs` and `tests/it/vram_admission/dev_image_start.rs`.
- **`LMGW_MOCK_CAPTURE=1`** (debug builds only; a release build logs that it ignores it) turns on
  WebKit's mock capture devices: four "Mock audio device"s and a mock camera. Requests still go
  through the handler. Later WPs use it to open "the microphone" in the real shell.
- **`scripts/shell-check.py`** runs the real debug shell with nothing reaching the desktop: a
  fresh data dir under a work dir of its own (`target/shell-check-<pid>`, so a second run never
  removes the first one's podman stub) seeded by the headless runner on a container prefix of its
  own (`lmgw-shellcheck-<pid>`), a `podman` stub first on `PATH` that records every call and runs none,
  its own runtime, cache, config (Downloads inside it) and data dirs, a private D-Bus bus without
  service activation, a private GTK3 broadwayd, mock capture, WebKit's remote inspector on loopback
  (`WEBKIT_INSPECTOR_HTTP_SERVER`, unauthenticated while it runs) to script the page and its frames,
  and a private PipeWire graph (pipewire, wireplumber's policy-only profile, pipewire-pulse, their
  own config and state dirs, two null sinks). It runs §13.1's and §12.3's checks, the M1 document
  checks below, and reads the podman record back: every call the seed and the shell made named the
  run's prefix and nothing else, whatever its verb (a listing filters on the run's label, anything
  that changes something names the run's containers; WP11 review n5). It cleans up on exit, SIGTERM
  and SIGHUP, and every child dies with it (`PR_SET_PDEATHSIG`). The WP11 UI fix run took 353 s
  for 148 checks (25 after WP5; the WP6, WP7 and WP9 checks live in `scripts/shell_check_voice.py`),
  and the unauthenticated inspector and broadwayd's HTTP port are open on loopback that long. The
  realtime phase sends its mock and its probe as two messages: WebKit's inspector server resets a
  connection whose message is over 128 KiB, and the two together passed that in the WP11 UI fixes.
- **The M1 document checks, live.** An agent app's top-level document is denied the microphone by
  the handler ("not the gateway's origin") and the commands by the capability. The provisional
  window was **not reproduced** on WebKitGTK 2.54: a `getUserMedia` made once a navigation is
  pending is rejected by WebKit itself (`TypeError`) without asking the handler, one made just
  before the navigation is decided before the provisional load starts (12 attempts, both orders,
  0–40 ms apart), and command floods from navigating agent documents never reached the shell's
  gate. The two-URI rule stays as defence in depth and for a later WebKit; the sweep is the
  regression check.

## 14. Routes

| Route | Cap | Docs |
|---|---|---|
| `POST /chat/api/threads/{id}/voice/warm` | Admin | exclusion `DASHBOARD_BACKEND` |
| `POST /chat/api/threads/{id}/transcribe` (no default body limit) | Admin | exclusion `DASHBOARD_BACKEND` |
| `POST /chat/api/threads/{id}/messages/{mid}/speak` | Admin | exclusion `DASHBOARD_BACKEND` |
| `POST /chat/api/threads/{id}/speech/stop` | Admin | exclusion `DASHBOARD_BACKEND` |
| `GET /v1/realtime?chat_thread=` | Inference; the binding needs Admin | `DocRoute` updated |

Existing routes that gain fields:
- `send`, edit, regenerate and continue: `speak`, plus `voice` on send;
- thread settings and folder defaults: `voice`;
- the thread JSON: `voice`, `voice_resolved`, and `voice` per message;
- persist (Keep): `409 voice_session_active`.

All are thread-scoped, so temporary threads dispatch on the id's sign. There is no new `/api/op`
and no new `x-lmgw` header. The settings are §2.1's table.

## 15. Work packages (build order)

Each WP ends green on `cargo fmt --check --all`, `cargo test -p <crates touched>`, and a fresh
`trunk build` when the UI changed, and is committed with explicit paths. The ITs are modules of the
one `tests/it` binary, with mock upstreams. Speech fixtures are TTS-generated. WP5 (shell) does not
depend on the core WPs and can go in any order, in this one checkout.

**WP1 Settings, overrides, storage** (§2, §3).
- Scope:
  - migration 0057, `store/chat_voice.rs` (`ThreadVoice` with seed, `MessageVoice`), the nested
    tolerant reads;
  - `ChatRepo` operations for `Db` and `Temp`, and Keep copying `voice`;
  - `ThreadDefaults.voice`, the six keys, and the `chat_stt_alias` places of §2.1;
  - `resolve.rs` and the thread JSON (including `fallback` and `realtime`), export, and the
    edit/continue clearing;
  - UI: `pages/settings/chat_voice.rs`, `widgets/voice_picker.rs`, the thread Voice section, the
    folder JSON form.
- Tests:
  - unit: strict and tolerant parsing (a newer nested key keeps every folder default), the
    resolution chain and `source` per field;
  - IT `chat_voice_settings.rs`: both save paths and the MCP schema; the thread override; the
    folder copy; temporary threads and Keep; an attachment transcript through
    `realtime.asr_alias` and through the thread's override; unheard text absent from search.
- Live: `scripts/ui-matrix.py` plus a `scripts/drive/` check of the group and the thread form.

**WP2 Turn seam and agent fixes** (§7).
- Scope: the golden SSE transcripts first; `TurnFrame`, `start_turn_into`, `Stopped::Interrupted`;
  live relay in `agent::run`; `ABANDONED_CALL` results; the adjacent-user merge;
  `DeltaSink::flush`.
- Tests:
  - the golden transcripts are byte-equal after the refactor, and every chat IT passes;
  - agent unit tests: a slow scripted runner yields `Text` before the turn ends; arrival order is
    kept across text and calls; a cancel mid-tool leaves a record with results;
  - IT: a tool thread's first `delta` arrives before the mock finishes; Stop during a slow tool,
    then send again, succeeds; a failed send followed by a send yields one merged user message
    upstream; `/v1/responses` streaming still passes.
- Live: an MCP-tool thread streams token by token in the app.

**WP3 Load on first use, dictation backend** (§4, §5).
- Scope: `WarmOutcome`, `WarmMode::Admit` with the group rule and the load on the admission's claim;
  `state` frames; `voice/warm`; `transcribe` with its own body limit; the `ClientProto` parameter of
  `synthesize`.
- Tests: IT `chat_voice_dictation.rs`:
  - transcribe with a mock ASR, and a WAV over 2 MiB accepted;
  - under the GPU hold, a GPU ASR row with a cloud fallback **is** answered by the fallback's mock,
    and `asr_answered_by` names it; a CPU row serves itself;
  - the warm reports `loading` → `ready`, and `held` with its cause;
  - an Admit warm evicts an idle model where a Background warm reports `skipped: full`;
  - two stages that do not fit together report `does_not_fit` and evict nothing;
  - an aborted warm drops its admission wait.
- Live: dictation by `curl` with a TTS-generated German WAV against `scripts/dev-instance.sh`; a
  cold ASR shows `loading`, then `ready` with its time.

**WP4 Speech for the Chat** (§6).
- Scope: table blocks and internal announcements; the speech pipeline without a writer; the thread
  seed; `speak`; the speech tee with closure; `speech/stop`; optional bodies on regenerate and
  continue.
- Tests:
  - clause unit tests: a table is skipped; announcements come only with `Announce`; a stock session
    skips tables and announces nothing;
  - IT `chat_voice_speak.rs` with a mock TTS:
    - frame order for a stored reply;
    - `speak: true` interleaves `speech` and `delta`;
    - `speech/stop` stops the speech, not the text;
    - aborting a `speak: true` send drops the upstream mock and saves the partial reply;
    - an aborted `speak` stops synthesis and writes one row labelled as the Chat;
    - a tool preamble is spoken before the result;
    - two speaks of one thread send the same seed.
- Live: read-aloud of a German reply with a code block and a table on the local TTS. The
  announcements are heard, and the first audio plays while the text streams.

**WP5 Shell** (§12.3, §13).
- Scope: the permission handler, the two async commands, the manifest and capability, the stream
  identity; added: the dev-launch guard, `LMGW_MOCK_CAPTURE`, `scripts/shell-check.py` (§13.3).
- Tests: unit tests for the exact-origin check and for sink and stream detection on synthetic
  `pw-dump` fixtures.
- Done in WP5 by `scripts/shell-check.py` (mock devices, private graph): everything below except
  what needs real devices — the owner's microphone, sinks and filter chains, the installed
  app's WirePlumber memory, and the KDE mic indicator.
- Live, in the app:
  - an audio request is granted without a prompt; video is denied; a probe from an agent-origin
    frame and from the HTML preview frame is denied;
  - `enumerateDevices` shows labels;
  - choosing headphones moves every lmgw stream, and "System default" clears it;
  - restore-stream keys lmgw apart from another WebKitGTK app.

**WP6 Page audio foundation** (§11, §12.1, §12.4).
- Scope: the `web-sys` features, the Trunk copy-dir, both worklets, the player and capture modules,
  the device popover (level meter, test tone, echo chip), the shell bridge, `setSinkId`.
- Tests:
  - native unit tests: WAV encoding, base64, device matching by id then label, the echo warning
    rule;
  - **WebKit:** `scripts/webkit-check.py --backend broadway` gains a media probe. It sets
    `enable-mock-capture-devices` and a permission handler on its own view, opens the mic, runs the
    capture worklet, counts 24 kHz chunks, and plays the test tone through the player while
    reading the output analyser;
  - Chrome (`--use-fake-ui-for-media-stream --use-file-for-fake-audio-capture=<synthetic.wav>`):
    the same assertions, and the played count equals the pushed count.
- Live: in the app, the device list, the meter, and the test tone on the chosen sink.
- *Built (WP6):*
  - **The popover** is the composer's audio devices button (`pages/chat_voice/devices.rs` +
    `devices/{panel,meter,echo}.rs`), beside Attach and Knowledge. The Chat page provides the
    window's choice (`VoiceDevices`: input, output, echo mode, the last listing); every change is
    stored at once and an output change re-routes a playing context. The button turns amber with
    §12.2's warning. *Test microphone* opens the capture exactly as dictation and realtime will (the
    window's input, the echo mode's constraints, the 24 kHz worklet); a different input or mode
    reopens it, Stop and closing the popover release it. *Play test tone* is a three-note chime at
    −12 dBFS through the page's player. The echo chip (`EchoChip`, for §9.4 too) lists the four
    modes, the default marked, and offers half duplex under the warning. The root carries `data-*`
    counters (mic state, chunks, samples, peaks, tone pushed/played, player and route state) for
    the probes.
  - **The probes** share `scripts/media-probe.js` (the worklets alone, then the page's own code
    through the popover). `scripts/webkit-check.py --backend broadway --media` runs it in a view
    with mock capture devices, the shell's permission rule, and autoplay allowed (its clicks are
    script clicks, not gestures); playback goes to a private PipeWire graph whose only sink is a
    null sink. That graph is the view's own `XDG_RUNTIME_DIR`, not `PULSE_SERVER`: WebKit 6.0's
    bubblewrap sandbox aborts the UI process when `PULSE_SERVER` is set (measured, 2.54), and
    libpipewire clients hang on a private bus with no services, so the desktop's bus is kept.
    `scripts/media-probe-chrome.sh` runs it in Chromium in the Playwright image (fake capture from
    `tests/fixtures/realtime/audio/en_two_sentences_pause.wav`, `--mute-audio`, no audio server in
    the container) against a dev copy. Both pass every check (WebKit 35, Chromium 37: the Chromium
    run also lists two outputs, so it covers the warning and its clearing).
  - *WP6 fix (review of 0e4c814):* the composer's devices button is the one place that says
    something is off with lmgw's audio — the playback did not start, the output route failed, the
    chosen output is gone, or §12.2's warning — amber, in its title and `aria-description`, with
    `aria-haspopup="dialog"` (review m5, n7). It reads the device list on mount only when an
    output is chosen (in the app each read is a pw-dump, review n10). While the Chat page lives, a
    `devicechange` re-reads the lists and re-applies the route, not only with the popover open
    (review m7). Test microphone shows a microphone the system muted, and one that ended on its
    own, with the reason.
  - *The probes since the fix* (`target/chat-voice/wp6fix/`): the shared probe also dispatches a
    track's `mute`, `unmute` and `ended` and a `pagehide` into the back/forward cache, checks
    heard ≤ played, the warning on the composer's button, a choice stored by another tab (a
    same-origin frame), and in Chromium a chosen output that is gone; a reload then opens the
    stored input in a fresh document (`reopen`, review m10.2: WebKitGTK and Chromium both open it
    with one `getUserMedia`). The Chromium runner takes the microphone permission back over CDP
    (the capture ends, "the microphone permission was taken back"), and runs a second Chromium
    without autoplay allowed (review m10.1): a tone clicked from script fails visibly, a trusted
    CDP click plays it. WebKit 48/48, Chromium 55/55.
  - *In the app* (`scripts/shell-check.py`, review m10.3/m10.4, private graph, mock capture, no
    window): the popover lists the shell's outputs; with an output chosen while no context
    existed, the first tone, recorded off both null sinks' monitors, arrives whole on the chosen
    one from its first note and nothing on the other; every lmgw stream, the capture context's
    too, is on it; one route application at creation and one per resume (review M2); "System
    default" sends them back — which found the shell's `null` leaving a resumed stream on the old
    sink (§12.3, fixed). 37/37.
  - Unit tests: the ledger's traps, the routing's one-at-a-time rule, the no-utils rule, Chrome's
    default entry, placeholder labels, `none` mode's constraints. `clippy.toml` refuses
    `web_sys::MediaStream::clone` and `MediaStreamTrack::clone` (the JS clones, review n9).
  - *Deferred:* the shell targets a sink by serial, which changes on replug; a page-level
    `devicechange` re-applies the route, but WebKitGTK may not fire one for a sink, so in the app
    a replugged output is picked up at the next resume. `target.object = node.name` (WirePlumber
    takes names) would survive a replug; it needs WP5's memory checks re-run (review m7). Chunks
    carry no capture frame yet (review n8): WP9 adds it if it wants a client-side latency figure.

**WP7 Dictation and read-aloud UI** (§5, §6.5, §3, §2.3).
- Scope: the mic button, Right Ctrl, composer insertion, `voice` on send, badges, greyed unheard
  text, speaker buttons, the read-aloud toggle, the tooltips, and the fallback-before-recording
  note.
- Tests:
  - Chrome drive with a fake capture file (a TTS-generated sentence) against a dev instance: the
    text lands in the composer, Enter sends `voice`, and the badge shows; the speaker plays; live
    read-aloud starts before `done`; a seeded row renders greyed;
  - the WebKit probe runs a dictation round against a mock ASR.
- Live, in the app with the default setup: dictation, read-aloud on the speakers, Right Ctrl. The
  mic indicator goes off at release.
- *From the WP6 review:* a streaming read-aloud holds `Player::hold_awake` and starts with
  `Player::begin`; dictation handles `CaptureEvent::Ended` (nothing is uploaded after an end) and
  says `Muted`. Whether a modifier-only `keydown` (Right Ctrl) counts as user activation in Chrome
  and Firefox is not settled (review n12): a dictation started by key may meet the player's "did
  not let lmgw's playback start" until a click; the app allows audio without a gesture.

- *Built (WP7):* the notes in §2.3, §3, §5 and §6.5. Tests:
  - native: the tag rule for chips, insertion at the caret (UTF-16 offsets), the mark's sums, the
    key rules, the `state` notes and the hold lines, the transcription failure's words, the size
    check;
  - Chrome (`scripts/drive/chat-voice-dictation.json`, `ui-drive.py --fake-mic <WAV>`: Chrome's fake
    microphone playing a TTS-generated German recording once, the browser muted) against a dev
    copy with parakeet on the CPU, supertonic and gemma4-e4b: Right Ctrl held records, the
    transcript lands in the composer marked, Enter stores `voice`, the badge shows, the speaker
    plays a stored reply to its end (played == pushed), read-aloud's first audio comes before the
    text's `done`, the next send is let through while it speaks, Stop speaking ends it;
  - `scripts/drive/chat-voice-render.json` on rows `scripts/chat-voice-seed.py` writes into a
    scratch database (in `scripts/chat-drives.sh`): greyed unheard rest, chips, badges, timing;
  - WebKit (`webkit-check.py --media`, phase `dictation` of `scripts/media-probe.js`) and the real
    shell (`scripts/shell-check.py`, the same phase): a dictation round with mock capture against
    a mock ASR the page answers itself (`fetch` of `voice/warm`, `transcribe` and the thread's
    JSON, so it runs on a gateway with no speech model): the WAV (16 kHz mono PCM16, as long as the
    press), every track ended, the text at the caret, Esc, Right Ctrl held and Right Ctrl + C, the
    hold, a failed fallback named, the mark dropped with an emptied composer. The mock is in the
    page rather than an upstream: the server's half has its ITs (WP3).
  - Not settled (review n12): in a browser, a dictation whose first gesture is Right Ctrl alone
    may leave the player's context unstarted until a click; the dictation itself does not need it.
    *Settled by the WP7 fix (m4):* Right Ctrl never makes the context; a pointer press does.
- *Fixed after the WP7 review* (notes in §2.3, §3, §5, §6.5, §9.7, §11.1). Tests:
  - native: the key rule with a remapped Right Ctrl (Compose, a layout switch, AltGraph) and an
    IME composition, `Block`/`under_block` for the hold, a benchmark run, both, a CPU row, no
    fallback, an unknown hold and an unmanaged or remote alias, the benchmark refusal as the hold
    chip, the one-letter tag rule (gateway and chips), spacing inside brackets and quotes, the
    failure and size wording;
  - IT `chat_voice_resolve`: a CPU row reports its fallback (for a lease), `managed`;
  - WebKit (`webkit-check.py --media`, phase `dictation`): a Right Ctrl tap and Right Ctrl + C
    call no `getUserMedia`, warm and upload nothing (the tap says so), a remapped Right Ctrl
    (`key: "Compose"`) is not dictation, a release while the microphone opens uploads nothing and
    releases it, press-and-hold by pointer events (the hint says "release"), Enter while recording
    finishes the dictation without sending, a `gpu_benchmark` refusal is the hold chip, the live
    region is mounted from the start and never says the clock;
  - Chrome (`chat-voice-dictation.json`, trusted CDP input): the tap and Right Ctrl+C open nothing,
    a held mouse press records and transcribes, the voice menu holds read replies aloud, and under
    the dev copy's GPU hold the CPU speech-to-text keeps serving while read-aloud is said refused
    (no fallback) on the status line, the voice menu and every speaker button, before anything is
    spoken; `chat-voice-render.json`: the unheard rest at ≥ 4.5:1 in italics, `[a]`/`[b]` as text;
  - `scripts/ui-matrix.py` fails a squeezed composer (`composer_input`, §6.5's numbers).

**WP8 Realtime binding** (§8).
- Scope:
  - the handshake binding and its refusals, takeover, ownership and merge strictness;
  - the per-response admin check;
  - the journal with its slots, re-cuts and conditional writes;
  - the voice prompt assembly and the reasoning default;
  - `empty_turn`, the extension events, the drain at disconnect, Keep refused, docs.
- Tests: IT `realtime_chat_thread.rs` (tokio-tungstenite, the dashboard cookie, manual commits with
  synthetic PCM, mock ASR/chat/TTS):
  - **binding:** accepted for the cookie; 403 for a non-admin key; 409 `chat_thread_admin` for an
    admin thread; a plain thread with the `lmgw` label attached binds, and its thread JSON flags
    the admin tools; a second bind takes over;
  - **a turn:** the user row carries `voice`; the reply comes through the chat engine (the mock sees
    the thread's model and sampling, and the system message is thread prompt → bridge → voice text
    without the persona sentence, with the date line kept, → hint); KB context lands on the user
    row; MCP tool frames are relayed, with no `function_call` item;
  - **B1 order:** a barge-in during generation and an immediate next commit → reply N is saved and
    cut, and the next user message follows it;
  - **cuts:** a late truncate re-cuts; a cut with no heard text deletes, but keeps a reply with a
    tool record; owed turns are not written twice; a failed turn plus the next one reach the mock
    as one user message;
  - **text in a second window** during voice playback: the text reply is saved, and the finalize is
    skipped with a note;
  - `empty_turn`; supersede; a temporary thread; Keep refused while bound;
  - no `lmgw.*` event in an unbound session; the timing event matches the log line;
  - under the hold, the ASR fallback serves and is named in `lmgw.model.state` and in the timing;
  - the connect warm Admits the thread's models; a disconnect mid-reply drains the journal.

**WP9 Realtime mode UI** (§9, §10).
- Scope: the panel, the WebSocket client and state machine, PTT and automatic, mute, stop talking,
  the keys and focus rules, the chips, captions, hold/loading/fallback status, the timing readout,
  the visualisation adapter with the owner's variant, the disabled-button reasons, re-enter.
- Tests:
  - Chrome drive with fake capture (a TTS-generated question) against a dev instance with local
    aliases: `data-voice-state` walks idle → listening → thinking → speaking → idle; captions
    appear; both bubbles are written; Esc ends every track; an admin thread's button is disabled
    with its reason;
  - the WebKit probe opens a bound session against mocks and checks the same state walk;
  - a `scripts/drive/` harness mounts the variant with synthetic analysers through every state
    and `destroy`.
- Live, in the app with the default setup:
  - full-duplex barge-in, with the stored cut matching what was heard;
  - Space, M, stop talking;
  - `none` mode is half duplex; `browser` mode verified or left labelled;
  - the hold chip, with a CPU row still serving and a cloud fallback named;
  - the timing readout against the log line;
  - the variant at 60 fps on the NVIDIA box;
  - the KDE mic indicator off after leaving.
- *Built (WP9):* the notes in §9, §10. Tests:
  - native: the protocol's events both ways, the derived state machine (a turn, a barge-in, a stop
    that cut nothing heard, push-to-talk and a refused turn, a reply played out before its done),
    the captions, the keys and their focus rules, the close words, the variant choice;
  - IT `ui_cache`: the visualisation modules and the `import()` snippet are served as JavaScript
    and revalidate;
  - `scripts/drive/chat-voice-viz.json` (`viz-harness.js`): each variant mounted with two synthetic
    voices as its analysers, walked through every state, destroyed — no frame after, the canvas
    released, the orb's WebGL2 context lost, the analysers' settings given back — three rounds
    with no listener, interval, observer or detached node left;
  - `scripts/realtime-mock.js`, an in-page bound session driven by the probe, behind
    `scripts/media-probe.js`'s phase `realtime` (37 checks): the walk, the 24 kHz appends, captions
    and bubbles, a barge-in's truncate at what was heard (≤ what was sent), stop talking,
    push-to-talk by Space, M, the focus view, a takeover with Re-enter, Esc, an Admin Chat thread's
    reason — in WebKitGTK (`webkit-check.py --media`, mock capture devices) and in the real shell
    (`shell-check.py`). *Extended in the WP9 fixes (review m1, m2):* the mock follows the WP8
    server around a cut (a barge-in finalized at what was sent and re-cut by the page's truncate,
    a truncate of an item still produced cancelling its response, the word rule, refused
    truncates and cancels, `session.updated` and `input_audio_buffer.cleared` answered, a response
    done after its playout, a tool turn, the page's close answered after the drain), and the
    phase has 60 checks: every exit (Leave, another thread, Esc while the microphone opens, a
    mid-session refusal, a refused handshake for a deleted and an Admin Chat thread, the
    microphone ending, pagehide, the page unmounting, a takeover while it speaks), a tool turn,
    push-to-talk chosen while connecting and switched to mid-utterance, a stop while it thinks;
  - `scripts/drive/chat-voice-panel.json`: the panel with each variant in each state, chat panel
    and focus view, for the owner's eye; the composer's text back after every leave;
  - `scripts/drive/chat-voice-realtime.json`, live on a dev copy (Parakeet on the CPU, gemma4-e4b,
    Supertonic): a fake microphone saying TTS clips on cue (`scripts/drive/fake-voice.js`,
    `make-voice-fixtures.py`) — Chrome's fixed-file fake device cannot speak a second utterance
    over a reply whose timing is not known in advance, so `getUserMedia` answers a Web Audio
    stream (the constraints are still recorded and checked).
- *Found live (WP9, for the review):* the stored heard part of a cut can end inside a word
  ("… bestimmte Lichtw" / unheard "ellenlängen …"): the heard table cuts the clause the cut falls
  in by its share of characters (`heard.rs`, realtime §7.3), and §8.4 keeps that partial clause
  when it was said as written. *Decided in the WP9 review (B) and built in its fixes:* the heard
  part is cut back to the last word heard whole, in the heard table, for stock and bound sessions
  alike (realtime §7.3, §8.4). The live drive checks that a barge-in's and a stop's stored cut
  end at a word.

- *Extended in the WP11 UI fixes:*
  - `scripts/realtime-mock.js` answers as the WP11 server batch does: a bound turn refused with
    `lmgw.model.state {chat, held}` (or `loading` then `failed` for a VRAM wait), the chat
    frames' `error {message, code}` and `done {aborted}`, then `error` with the code and
    `response.done` failed; a transcription failed with its code; a commit whose transcript never
    comes; the connect warm's chat stage held; a journal of the stored rows per thread, which the
    thread reads answer, with a turn written during the drain (`opts.lateUser`). Its late-assigned
    members are declared in its literal (NIT 1);
  - `scripts/media-probe.js`, phase `realtime` (77 checks): the entry's focus and announcement, the
    read at the close only, the cut reply once, patched in place (an earlier bubble the same node,
    its open timing line still open), and an integration round (`only: "integration"` runs it
    alone): voice mode entered during a dictation and during a read-aloud (a mocked `…/speak`
    stream), Right Ctrl in voice mode, a speaker press there, an echo change mid-session
    (reopened, `half_duplex` for `none`), the pointer's Talk button, a hold as one amber chip, a
    context overflow worded, `asr_not_configured`, a transcript that never comes, a late turn
    added by the read-back, re-entering while the old session drains (`drainMs` 1500), and five
    enter/leave cycles counting AudioContexts (at their first gain node: the page's glue holds
    the constructor) and WebGL contexts; phase `dictation`: a slow Right Ctrl combination warms
    nothing, Esc while Right Ctrl arms keeps its effect;
  - `scripts/drive/chat-voice-viz.json`: the calm rate idle and quiet and in reduced motion, and
    the full rate back on a state change (`viz-harness.js` `mount(v, {quiet, reduced})`,
    `rate(ms)`);
  - `scripts/ui-matrix.py --chat-drawer open|closed` sets the thread settings panel before the run
    (m7);
  - `scripts/drive/chat-voice-realtime.json`, live: leaving mid-reply shows the cut reply once and
    the transcript is the stored rows with its first bubble the same node; under the dev copy's
    own GPU hold one amber chip in the panel and above the composer, gone with the next turn once
    the hold is off.

**WP10 Docs.**
- The README voice section and `scripts/readme-shots.sh` shots.
- The MCP descriptions of the six keys and of the changed `chat_stt_alias`.
- The DocRoute text for the extension events and table skipping.
- *Built (WP10):* the README's "Voice in the Chat" section with four shots
  (`docs/screenshots/{chat-voice-panel,chat-voice-focus,chat-voice-menu,settings-chat-voice}.png`).
  `scripts/drive/readme-voice.json` shoots them in the newest demo thread, against
  `scripts/realtime-mock.js` and `scripts/drive/fake-voice.js` (no model, no microphone, Chrome
  muted); the mock's `opts.models` (and `patchThread`'s `mode.models`) name the thread's own
  aliases, so the chips and the timing line agree. `scripts/readme-shots.sh` takes `pages`,
  `voice` or `all` as its third argument and converts only the run's own shots. The Settings shot
  shows `~/.local/share/lmgw` in the header instead of the dev copy's path. The seven settings'
  MCP descriptions and the `/v1/realtime` DocRoute (refusal order, `session.created`'s thread,
  the connect warm, the announcement words, the timing event's fields) were checked against the
  code. Every route this branch adds is a `DASHBOARD_BACKEND` exclusion or the `/v1/realtime`
  DocRoute; it adds no `/api/op` and no `x-lmgw` header.

**WP11 Review and acceptance.**
- An Opus review split by area: seam and agent, binding and journal, shell, UI.
- Fixes, then UI verification with `scripts/ui-matrix.py`, the new `scripts/drive/*.json` and
  `scripts/webkit-check.py`.
- A review page for the owner.

## 16. Defaults chosen in this design (open to revision)

- **Loading.** Explicit presses may evict; Background warms never do. Several stages warm as one
  group. Entering voice mode is such a press: its connect warm loads the thread's models (and may
  evict idle ones) whatever `realtime.warm_on_connect` says, which applies to API sessions only;
  the setting's hint says so (WP8 review m6).
- **Speech.** Clauses are cut on the server. Read-aloud is unpaced. Every speaking response skips
  tables; Chat voice also announces code and tables. A cut reply is read whole, greyed rest
  included.
- **Storage.**
  - Each response's turns become one user message.
  - Adjacent user messages are merged when the request is built, not in the store.
  - A reply with no heard text and no tool record is deleted.
  - Unheard text is not searchable. Timing is stored with the reply.
- **Sessions.** A second window binding the same thread takes over. Keep waits until voice mode
  ends.
- **Turn detection.** `chat_turn_detection` defaults to `semantic_vad`; realtime's own A/B is
  still open.
- **Keys.** Right Ctrl holds to dictate. Space is push-to-talk, and stop talking in automatic mode.
  M mutes. Esc leaves.
- **Admin Chat** keeps dictation and read-aloud (§5).
- **Permission.** The shell grants the mic to the gateway's own origin without a prompt. KDE's mic
  indicator shows use.
- **Output routing** is re-applied on every playback context. There is no hard dependency on
  pipewire-utils.

## 17. Decided, and what remains

The owner's rulings of 2026-10-03 are rulings 5–7 above, applied in §8.5, §4.4 and §8.1. The
reasoning default for voice turns (§8.5) follows from ruling 5: an explicit thread setting wins,
otherwise reasoning is off for voice. It is listed so it can be overturned. No other decision is
open.

## Not in this round

- Storing audio.
- Voice profiles and design presets (realtime §19).
- Live partial transcripts while dictating.
- Spoken filler while a tool runs; speculative responses.
- A per-thread voice prompt.
- Output choice inside WebKitGTK itself.
- macOS media permissions in the shell.
- Warming the chat model at a dictation press.
- `timing` and `models` events for unbound sessions.
