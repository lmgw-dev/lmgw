# Chat, made complete: rendering, parameters, message actions, folders, search, export, temporary chats, attachment kinds, knowledge bases, key scope editor

Requested by the owner 2026-09-30, after a gap analysis against Open WebUI. The open
questions were answered the same day (folders one level with defaults: approved; knowledge-base tools on Chat
**and** `/mcp`, since keys already scope per tool; text-PDF attachments switchable per chip with a
configurable default). Everything else below is a default chosen during implementation, listed in §13 for review.

Branch `feat/chat-complete`. The ideas that were *not* picked (web search, compare/leaderboard,
presets, prompt library, memory, code interpreter) are in `docs/ideas.md`.

## Ground rules for every work package

- **File size.** `crates/lmgw-ui/src/pages/chat.rs` is 3.3k lines and `web/chat.rs` 1.3k. New
  logic goes into new sibling modules (`pages/chat_actions.rs`, `pages/chat_folders.rs`,
  `web/chat_folders.rs`, `store/chat_folders.rs`, …), wired with `mod x;` (+ `pub use` where the
  hub needs it). Hub files only gain the call sites. Integration tests are modules of the one
  `crates/lmgw-core/tests/it/` binary (`tests/it/<name>.rs` + a `mod` line in `main.rs`).
- **No hidden limits.** No guessed caps. Where a bound exists it is a visible setting or field,
  and an overrun surfaces as a visible error. Counts that are grouped or paged say how many
  there are.
- **Routes.** Every new route gets its `CAPABILITY_TABLE` row in `server.rs` and either a
  `DocRoute` or an `openapi/exclusions.rs` row (the `/chat/api/*` and `/api/knowledge/*`
  dashboard backends are exclusions with reason `DASHBOARD_BACKEND`). A new `/api/op/<name>`
  goes in `web/op_names.rs` and gets an `OpDoc`. The `openapi_coverage` test enforces this.
- **Settings.** A new setting touches: `config/settings.rs` (field, default fn, `Default`),
  `ops/settings_patch.rs` (`SettingsPatch` + apply + validation), `web/api_settings.rs` (read,
  patch field, apply — a second parallel path, keep in sync), `ops/reads.rs` (the `lmgw__settings`
  read), `mcp/selfadmin/catalog/runtime.rs` (the `lmgw__settings_set` schema),
  `lmgw-api-types/src/settings.rs`, and the UI table in `lmgw-ui/src/pages/settings.rs`.
- **Errors** from `/chat/api` stay the flat `ApiError {code, message}` (`web/chat.rs` helper).
- **UI rules** from the UX pass hold: overlays are `<dialog>` `Modal` or `Popover` only; no
  `position:fixed` inside `.content`; container-query breakpoints; async work that outlives a
  page goes through `crate::scope::Scope`. Destructive actions use `widgets/confirm.rs`
  `ConfirmButton` (two clicks), then really do it.
- **Release notes**: each package adds its lines to `docs/release-notes.md`.

## 1. Rendering

### 1.1 Math (KaTeX)

- Vendor KaTeX (latest 0.16.x from the npm tarball) into `crates/lmgw-ui/assets/vendor/katex/`:
  the ES module or min.js, `katex.min.css`, and the `fonts/*.woff2` files (woff/ttf are not
  needed; the CSS lists woff2 first). Trunk already copies `assets/vendor` whole. Check that the
  embedded-asset server (`web/ui.rs`, rust_embed + mime guessing) serves `.woff2` and `.mjs`
  with correct types; fix the mapping if not.
- `md_to_html` (`pages/chat.rs`) enables `Options::ENABLE_MATH` (pulldown-cmark 0.13). Its `$…$`
  rules already keep "$5 and $10" as text (a closing `$` may not follow whitespace).
- Models also write `\(…\)` and `\[…\]`, which CommonMark would turn into escaped brackets. A
  pre-pass outside fenced and inline code rewrites them to `$…$` / `$$…$$` before parsing.
- `assets/codeblocks.js` (or a sibling module that `lmgwDecorateCode` calls) renders
  `.math-inline` / `.math-display` spans with `katex.render(tex, el, {displayMode, throwOnError:
  false})`. KaTeX is fast enough to run on every streamed token; decoration stays idempotent.

### 1.2 Diagrams (Mermaid)

- Vendor Mermaid (latest 11.x). Prefer the single-file UMD `mermaid.min.js`, loaded on first
  need (dynamic `<script>` or `import()`), so pages without diagrams do not pay for it.
- ```` ```mermaid ```` blocks stay plain code while streaming. When the message settles,
  `mermaid.render` replaces the `pre` with the SVG. A small toolbar keeps **Code** (toggle back
  to source) and **Copy**. A parse error keeps the code block and shows the error under it.
- `mermaid.initialize({startOnLoad: false, securityLevel: 'strict', theme: 'base',
  themeVariables: …})`, with colours from `app.css`'s tokens. The palette is Breeze-like graphite
  and blue, never pastel.

### 1.3 Raw HTML in model output is escaped

`md_to_html` passes raw HTML through today, and the result goes into `innerHTML`, so a reply
containing `<img src=x onerror=…>` runs script in the dashboard origin, which has the owner's
rights. Knowledge bases (§9) put untrusted document text in front of models, so this now
matters. `Event::Html` / `Event::InlineHtml` are emitted as escaped text. HTML the owner wants to
*see* rendered still has the sandboxed **Preview** of fenced ```` ```html ```` blocks.

## 2. Sampling parameters

Per thread, and per folder default (§5): `top_p`, `top_k`, `min_p`, `repeat_penalty`,
`presence_penalty`, `frequency_penalty`, `seed`, `stop` (a list). Blank means that the route's
default applies, like `temperature` today.

- Migration adds the columns to `chat_threads`. `stop` is a JSON array, `'[]'` by default.
- `ir::Params` gains `min_p: Option<f64>` and `repeat_penalty: Option<f64>`, both in
  `with_defaults`. The OpenAI ingress parses them (they are llama.cpp extensions that rode
  `passthrough` until now). The OpenAI egress emits them when set, which is what the passthrough
  did. Anthropic and Gemini drop them, as they drop every other unmodelled field.
- **The Chat never sends a parameter the route cannot take.** A new
  `web/chat_sampling.rs::split(params, route) -> (sent, ignored)` decides from the route's protocol
  and upstream kind:
  - llama-server: all of them;
  - generic OpenAI-compatible: temperature, top_p, presence, frequency, seed, stop;
  - Anthropic: temperature, top_p, top_k, stop;
  - Gemini: temperature, top_p, top_k, seed, stop.
  The ignored names join `done.reasoning_ignored`'s existing path into the stats row as
  `ignored: top_k, min_p`. This is the thread's own choice being refused, so it must be visible.
  An API client's request is not filtered; this is Chat-only.
- UI: a **Sampling** section in Thread settings (`SettingsDraft` gains the fields; `seed` and
  `top_k` are integers; `stop` is one sequence per line). Unsaved-change detection and
  `apply_settings` are extended the same way.

## 3. Message actions (no branching)

The conversation stays one linear list. **An action that rewrites history is final**: everything
after the rewrite point is deleted.

| Action | On | Effect |
|---|---|---|
| Copy | any | the message's markdown source to the clipboard |
| Delete | any | removes that one message (its attachments cascade); two-click confirm |
| Edit | user | replace text; **delete every later message**; resend (one action, "Save & send") |
| Edit | assistant | replace text in place; no resend |
| Regenerate | assistant | delete it and everything after; answer again from the history before it |
| Regenerate | user | delete everything after it; answer it again |
| Continue | last assistant | the model continues that same message (assistant prefill) |

Details:

- **Truncation warning.** Edit-user and Regenerate say how many later messages go ("discards 4
  later messages") and confirm when that number is > 0.
- **The thread's current settings are used.** A regenerate after switching the model is how you
  retry with another model. Attachments of the edited user message stay bound (in-place update,
  never delete + reinsert).
- **Editing an assistant message** clears its `reasoning`, its token counts and its
  `ir_messages`. They no longer describe the text. A reply with tool calls says in the editor
  that its tool record is dropped from the history.
- **Continue** is offered only when the thread's resolved route supports prefill: Anthropic
  (not with extended thinking on), and llama-server (a trailing assistant message is a prefill in
  current builds; mock-tested here, live-checked at the end). It is not offered on
  OpenAI-protocol cloud routes or Gemini. The thread JSON carries `continue: {ok, reason}`
  computed from `snapshot().resolve`. If a GPU-hold reroute at send time lands on a route
  without prefill, the send is refused visibly. The stream's `delta`s are the continuation only.
  On `done` the row's `content`/`reasoning` are **appended to**, not replaced, and the token
  counts become the last call's. Continue is not offered on a row with `ir_messages`.
- **Backend.** Thread-scoped routes, so temporary threads (§7) dispatch the same way:
  - `POST /chat/api/threads/{tid}/messages/{mid}/delete`
  - `POST /chat/api/threads/{tid}/messages/{mid}/edit` `{content}` → JSON for assistant rows,
    and an SSE stream (the send's event set) for user rows
  - `POST /chat/api/threads/{tid}/messages/{mid}/regenerate` → SSE
  - `POST /chat/api/threads/{tid}/continue` → SSE
- **Refactor.** Everything in `send` from loading the history onward
  (`list_chat_messages` → attachments → `build_messages` → `ChatRequest` → spawn `run_send` /
  `agentchat::run_send`) becomes one `start_turn(state, repo, thread, mode)` with
  `mode = Fresh | Continue {message_id}`. The persist step at the stream's end inserts for
  `Fresh` and appends for `Continue`. `send`, edit-user, regenerate and continue all call it.
- **SSE.** A new first event `turn {user_message_id}` (absent on continue/regenerate-assistant)
  lets the optimistic user bubble learn its DB id; `done.message_id` stays the assistant's. The
  UI's `Msg` gains `db_id: RwSignal<Option<i64>>` (today `MsgRow.id` is parsed and dropped).
- **UI.** An action row under each message, shown on hover/focus and always on touch-width
  panes. All actions are disabled while any reply in this thread streams. Edit opens the message
  in place as a textarea with Save/Cancel (Ctrl+Enter saves). New code goes in
  `pages/chat_actions.rs`.

## 4. Search

- Migration: an FTS5 table in the main DB (bundled SQLite 3.51 has FTS5):
  `chat_fts(body, kind UNINDEXED, ref_id UNINDEXED, thread_id UNINDEXED, tokenize='unicode61
  remove_diacritics 2', prefix='2 3')`. Rows of kind `t` are thread titles, `m` message content,
  and `a` sent attachment names. Triggers on insert, update and delete of the three tables keep it
  in sync, and the migration backfills it. unicode61 without porter works for German and English
  alike.
- `GET /chat/api/search?q=&archived=0|1|all&folder=<id>&offset=` →
  `{total_threads, threads: [{thread_id, title, folder_id, archived, updated_at, match_count,
  hits: [{kind, message_id?, role?, snippet}]}], next_offset}`.
  - Threads are ordered by their best bm25 rank. A page is 50 threads; `total_threads` and
    `next_offset` make paging visible.
  - Each thread shows its 3 best hits and `match_count` says how many there are.
  - Snippets come from FTS5 `snippet()` with private-use sentinel characters as match markers.
    The UI HTML-escapes the snippet, then turns the sentinels into `<mark>`.
  - Query syntax: words are ANDed, each as a quoted prefix term (`"foo"*`); `"a phrase"` stays a
    phrase. Input is sanitised, so FTS syntax errors cannot happen.
- UI: the sidebar filter keeps its instant client-side title/model filter. After 300 ms of
  typing and at least 2 characters, a **Messages** section below lists the server hits with
  snippets. Clicking one opens `?t=<id>&m=<message_id>`, scrolls to the message and flashes it.
  Temporary threads are not indexed.

## 5. Folders

One level, no nesting. A folder groups threads and can carry **defaults that a new thread in it
starts with** (a copy, like the default system prompt today; changing a folder later does not
touch its existing threads).

- Migration:
  - `chat_folders(id, name, sort INTEGER, defaults TEXT NOT NULL DEFAULT '{}', created_at,
    updated_at)`;
  - `chat_threads.folder_id INTEGER REFERENCES chat_folders(id) ON DELETE SET NULL`.
  `defaults` is a typed serde struct `ThreadDefaults` in which every field is optional (unset =
  the global behaviour): model_alias, system_prompt, temperature, max_tokens, the §2 sampling
  fields, the three reasoning overrides, mcp_tools, kb_ids and kb_mode (§9).
- Routes (`web/chat_folders.rs`):
  - `GET/POST /chat/api/folders`
  - `POST /chat/api/folders/{id}` `{name?, sort?, defaults?}`
  - `POST /chat/api/folders/{id}/delete` `{threads: "keep"|"delete"}`
  - `POST /chat/api/threads/{id}/move` `{folder_id|null}`
  - `POST /chat/api/threads` accepts `folder_id` and applies that folder's defaults over the
    global ones.
  Thread JSON carries `folder_id`.
- Sidebar:
  - A **Folders** block sits above the date groups. Each folder is collapsible (the state is
    kept in localStorage), lists its threads pinned-first then by `updated_at`, and has a row
    menu: New chat here, Rename, Folder settings, Export, Delete.
  - A thread appears exactly once: a thread in a folder is shown in its folder (pinned ones on
    top with the pin mark); the **Pinned** group shows pinned threads without a folder.
  - Moving works by HTML5 drag-and-drop of a thread row onto a folder header or onto the
    "no folder" zone, and by the row menu's **Move to…**.
  - **Folder settings** is a `Modal` with the same fields as Thread settings, plus the name.
  - The archived view is flat and shows each thread's folder name.

## 6. Export

- Routes, all with `Content-Disposition: attachment`:
  - `GET /chat/api/threads/{id}/export?format=md|json` gives one `.md` or `.json` file;
  - `GET /chat/api/folders/{id}/export?format=…` gives a `.zip` with one file per thread;
  - `GET /chat/api/export?format=…&archived=0|1|all` gives a `.zip` of everything.
- **Markdown** is the readable transcript. It has a title, a metadata block (model, created,
  folder, system prompt, non-default parameters), `## You` / `## Assistant · <model>` sections,
  reasoning in `<details>`, tool calls as fenced JSON, retrieval context as a collapsed list of
  citations, and attachments listed by name and kind. Attachment bytes are not included, and the
  file says so and names the JSON export.
- **JSON** is lossless: thread, settings, messages (incl. reasoning, `ir_messages`, context), and
  attachments with base64 bytes and their extracted text. There is a
  `"format": "lmgw.chat.v1"` marker, so an import can come later.
- File names are `lmgw-chat-<id>-<slug>.<ext>` and `lmgw-chats-<yyyy-mm-dd>.zip`. The `zip`
  crate comes in with §8.
- Temporary threads (§7) export from memory. That is one of the two ways to keep one.
- **Downloads in the app window.** The UI uses the existing `download(href, name)` helper
  (`pages/docs.rs`). It must be verified that WebKitGTK in the Tauri window actually saves the
  file; `src-tauri` has no download handler today. If it does not, add an `on_download` handler
  in `src-tauri/src/main.rs` that saves to the XDG Downloads dir and tells the page the final
  path, which the UI toasts. The plain-browser path needs nothing.

## 7. Temporary chats

A temporary chat is **never written to the database**. It lives in memory in the gateway process
until it is discarded or lmgw exits. `request_logs` still gets its usual metadata row; it has no
content columns.

- `state.chat_temp`: a mutex-guarded map of temporary threads (thread, messages, attachments).
  Ids come from one process-wide counter going **down from −1**. Threads, messages and
  attachments all get negative ids, so every existing `/chat/api/threads/{id}/…` and
  `/chat/api/attachments/{id}` route dispatches on the sign.
- **Repository seam.** A `web/chat_repo.rs` enum `ChatRepo { Db, Temp }` with the operations
  the chat handlers use:
  - get/update thread and title;
  - list messages and attachments;
  - append user message with attachments, append assistant;
  - update, delete and truncate message;
  - attachment insert/get/delete.
  `send`, `start_turn`, the persist step, `agentchat::run_send`, message actions and export go
  through it. No handler talks to `store::*chat*` directly afterwards.
- Routes:
  - `POST /chat/api/threads` `{temporary: true}` creates one; the kind is always `chat`, with no
    folder.
  - The thread list carries temporary threads in a separate `temporary: [...]` array.
  - `POST /chat/api/threads/{id}/persist` writes a temporary thread to the DB as a normal thread
    (messages, attachments, settings) and returns the new positive id. This is **Keep**.
  - Delete discards it.
- UI:
  - A **New temporary chat** entry next to New chat.
  - A temporary thread shows a banner: "Temporary — not saved. Discarded when you leave it or
    lmgw restarts. Keep / Export".
  - Leaving it (opening another thread, New chat, leaving the Chat page) discards it without a
    question. That is the point of it.
  - Temporary threads that survived a reload (the page lost them, the gateway did not) are listed
    in a **Temporary** group at the top of the sidebar until discarded.
  - Message actions, attachments, tools and knowledge bases all work in a temporary thread.

## 8. Attachment kinds

New kinds next to `image` and `text`: `pdf`, `office`, `audio`. Sniffing stays byte-based
(`chat_attach::detect`):

| Kind | Sniffed by |
|---|---|
| pdf | `%PDF-` |
| office | a ZIP whose `[Content_Types].xml` or `mimetype` says docx, xlsx, pptx, odt, ods or odp |
| audio | RIFF/WAVE, ID3 or an MPEG frame sync, OggS, fLaC, `ftyp` (M4A), EBML (webm) |

Anything else is refused with a message that lists what *is* accepted. Legacy OLE `.doc`/`.ppt`
are not supported, and neither is `.xls` (any OLE2 file is refused as legacy binary
Office; calamine was dropped for own sparse xlsx/ods readers, 2026-09-30).

- **Shared extraction module** `crates/lmgw-core/src/extract/` (`mod.rs`, `pdf.rs`,
  `office.rs`, `sniff.rs`), used by both the chat attachments and the knowledge-base ingestion
  (§9).
  - `pdf.rs` ports folder-chat's `run_piped` (stdin writer task, `kill_on_drop`, timeout,
    `ToolMissing`), `pdftotext -layout -enc UTF-8 - -` split on form feeds into pages, and
    `pdftoppm -r 150 -png -singlefile -f N -l N -` for one page image. Timeouts are visible
    constants.
  - `office.rs` uses `zip` + `quick-xml` for docx (paragraphs, tables as markdown), pptx (per
    slide, in slide order) and odt/odp. It has own sparse readers for xlsx/ods (per sheet, as a
    markdown table).
  - A missing `pdftotext` refuses a PDF upload with "install poppler-utils". The RPM gains
    `poppler-utils` in `tauri.conf.json`'s `rpm.depends`.
- Migration on `chat_attachments`:
  - `extracted TEXT` (derived text; the audio transcript);
  - `meta TEXT` (JSON: pages, text-less pages, sheet names, transcript alias, …);
  - `mode TEXT` (PDF: `text` | `images`; NULL = not applicable or not chosen yet).
  A new `chat_attachment_pages(attachment_id, page, png BLOB)` caches rendered pages; it is
  filled on first need and cascades with its attachment.
- **PDF.** Extraction runs at upload time, which classifies the file:
  - **Text PDF** (every page has text): the chip gets a **Text | Pages** switch. Its initial
    value comes from the new setting `chat_pdf_mode` = `text` (default) | `images` | `ask`. With
    `ask` the chip starts unset, and Send is blocked until every PDF chip has a choice, with the
    same visible reason style as the vision gate. The switch is
    `POST /chat/api/attachments/{id}/mode {mode}` and is refused once the attachment is sent.
  - **Scanned** (no page has text) and **hybrid** (some pages have none) are automatic. Text
    pages go as text; text-less pages go as page images when the model has vision, and otherwise
    as a visible note ("pages 3, 7: no text, and this model does not see images").
  - `images` mode sends every page as an image, which needs vision; without it, the chip says so
    and Send is blocked, like an image today.
  - Text goes as the existing `<file name=… kind="pdf" pages=…>` block with `--- page N ---`
    markers.
- **Office**: always the extracted text, wrapped the same way.
- **Audio.**
  - If the model's `input_modalities` contain `audio` (a new `model_audio_input` next to
    `model_vision`, reading `exposed_entry(...).capabilities.input_modalities`), it goes natively
    as `ContentPart::Audio`. The mime mapping is wav → `audio/wav`, mp3 → `audio/mp3`, flac →
    `audio/flac`; other containers take the transcript path.
  - Otherwise it goes as the **transcript** from the STT alias in the new setting
    `chat_stt_alias` (Settings → Chat; the picker offers models whose capability `task` is
    `asr`). The transcript is made at upload time when the thread's model has no audio input, so
    the chip can show it. Otherwise it is made on first need, and it is stored in `extracted`
    with the alias in `meta`.
  - If no STT alias is set and the model has no audio input, Send is blocked with that reason.
  - This needs an in-process `transcribe(state, alias, bytes, filename, mime) -> Result<String>`
    factored out of `proxy/audio.rs` (`gate::open` + `audio_send`), which logs its own usage row
    like any call.
- **The text-block escape.** Content inside `<file …>` is not escaped today, so a document
  containing `</file>` breaks out. Occurrences of `</file` in content become `<\/file`.
- **Render becomes async.** It needs page images and transcripts. `chat_attach::render(state,
  att, caps) -> Vec<ContentPart>` returns parts plus visible notes.
- **Chips** show a kind icon, the page count, and a token estimate for text content ("~12k
  tokens", chars/4 marked with `~`); this also closes the old "no token estimate per text chip"
  leftover. They also show the PDF switch and the blocking reasons. The attachment viewer shows
  extracted text or a transcript with **Download original**. WebKitGTK has no inline PDF viewer.

## 9. Knowledge bases

The owner's own documents, in named collections. They are retrieved into Chat threads and served
as the `kb__*` built-in toolset on `/mcp`.

### 9.1 Storage

- A **separate SQLite file** `<data_dir>/knowledge.db`, clamped to 0600 like `lmgw.sqlite`. It is
  never the docs corpus file (`quickdoc.db` is deliberately world-readable and exported whole).
  Original files live next to it under `<data_dir>/knowledge/` (dir 0700, files 0600), named by
  their sha256.
- **Reuse quickdoc-core's search machinery**, not its docs-shaped schema:
  - `vector.rs` (f16 BLOBs, L2 norm, the resident `VectorMatrix`);
  - the `Embedder` / `Reranker` / `TokenCounter` traits and `EmbedIdentity`;
  - `rrf_fuse`, `fts_match_expression`;
  - on the gateway side, `InProcessEmbedder` (`probe`, `for_corpus`-style identity pinning),
    `InProcessReranker`, `rerank_alias`, `TiktokenCounter`, the hold/budget gates, and the
    re-embed pattern.
  `Retriever` is bound to the `corpus`/`chunk`/`chunk_fts` tables. Either give `knowledge.db` the
  same chunk-table shape and a parameterised retriever, or extract a table-agnostic search core
  in quickdoc-core. That choice is the WP designer's, recorded in this section when made; both
  keep one implementation of hybrid search.

  **Chosen (WP9): a table-agnostic search core in quickdoc-core.** The pipeline
  — BM25 + KNN → RRF → optional rerank → `limit` → token budget, with the
  per-stage trace — is `quickdoc_core::retrieve::hybrid_search` over a
  `SearchSource` trait (FTS stage, KNN stage, fetch by id, a document's id and
  text). `Retriever` is the docs corpus's source over `chunk`/`chunk_fts`,
  unchanged for its callers (`Hit<D = Chunk>`); `knowledge::retrieve` is a
  second source over `kb_chunk`/`kb_chunk_fts` in `knowledge.db`, whose schema
  follows §9 rather than mimicking a corpus's (`kb`, `kb_file`, `kb_chunk`
  with `page`, `seq` and spans into the file's stored text). Why not the same
  table shape: a corpus's `document`/`source`/`derived_*` columns and its
  corpus-pinned `Retriever` would have had to be faked for files, pages and
  several bases per search. Bases sharing an embedding identity and a rerank
  model are one source (one BM25, the per-base resident matrices scanned
  exactly, one fusion); different models are searched apart and fused by rank.
  Two small additions to the core serve both users: a search without an
  embedder answers from BM25 alone and says why (`knn_skipped`), and
  `apply_budget` is public so a merged result is budgeted by the same rule.
- Schema:
  - `kb(id, name, description, embed identity (upstream, model, dims), rerank_alias,
    vision_alias, chunk_tokens, chunk_overlap, mcp_visible, created_at, updated_at)`;
  - `kb_file(id, kb_id, name, kind, mime, size, sha256, status
    pending|ingesting|ready|failed, error, pages, skipped_pages, chunk_count, added_at,
    ingested_at)`;
  - chunks with `kb_id, file_id, page, heading_path, span, payload, embedding`, and FTS5 over the
    payload.

### 9.2 Ingestion

- Upload is `POST /api/knowledge/{kb}/files` (multipart, several files), bounded by
  `max_body_mb` like chat uploads. Text is extracted with §8's module; a PDF is split per page.
  Text-less pages are OCR'd through the KB's `vision_alias` with folder-chat's `OCR_PROMPT` /
  `STRUCTURE_PROMPT` (ported), or skipped and counted in `skipped_pages` when no vision alias is
  set. The file row says so.
- A **deterministic chunker**; quickdoc's chunking is an LLM loop and is not reused. It is
  markdown-aware (heading path, paragraphs, fenced code kept whole where it fits, tables with
  their header repeated), per page for PDFs, and per sheet for spreadsheets, with `chunk_tokens`
  / `chunk_overlap` from the KB (defaults 512 / 64, visible on the KB). `chunk_tokens` is
  validated against the embed model's real context length; a larger value is refused, never
  clamped.
- A new job kind `kb_ingest` keyed `kb:<id>` (jobs subsystem, one live job per KB) takes every
  `pending` file. It reports progress and can be cancelled. Unchanged files (same sha256) are
  skipped.
- The GPU hold refuses local embedding (`refuse_if_held`). The job then stops with a visible
  "GPU hold is on" status and the files stay `pending`. **Resume** on the KB page restarts it,
  and so does the next upload.
- Changing a KB's embed model re-pins it and re-embeds every chunk (`kb_reembed`, the quickdoc
  `reembed` pattern).

### 9.3 Retrieval into Chat

- The thread (and folder defaults) gain `kb_ids` (JSON array) and `kb_mode` = `auto` (default) |
  `tool`, plus `kb_budget_tokens`. Its default is the new setting `chat_kb_budget_tokens` (4000;
  visible on the thread).
- **`#` in the composer** opens a picker of knowledge bases. A picked one becomes a chip scoped
  to **this message**, stored on the user message as `kb_refs`. Thread-level `kb_ids` apply to
  every turn.
- **Auto mode.**
  - Before the model is called, the turn runs hybrid search over `kb_ids ∪ kb_refs`, using the
    current user message as the query plus the previous user message when there is one, so that
    follow-ups like "and in August?" still hit. It then applies rerank if the KB has one, and
    fills the token budget.
  - The excerpts are **stored with the user message** (new column `context` JSON: kb, file,
    page, chunk id, text, score). Later turns replay them unchanged, which keeps llama.cpp's
    prompt cache warm and keeps the citations stable.
  - They are sent as a block ahead of the user's text: `<context source="knowledge">` with
    numbered `<excerpt n kb file page>` elements and one instruction line ("use these excerpts
    where relevant; cite them as [n]").
  - A new SSE event `retrieval {excerpts, tokens, ms}` shows "4 excerpts from Taxes · 3.1k
    tokens" collapsed above the answer.
  - `[n]` in the answer renders as a citation link that opens the **source viewer**: a `Modal`
    with the file's extracted text around the excerpt, the excerpt highlighted, the page number,
    and **Download original**.
- **Tool mode** attaches the `kb` built-in toolset to the thread (the agentchat path) instead of
  pre-retrieving. The model calls:
  - `kb__list` (bases with descriptions and file counts);
  - `kb__search {query, kb?, budget_tokens?}` (numbered excerpts with file/page);
  - `kb__read {file_id, page? | from_chunk?}` (read on in a document).
  In the Chat, tool mode is restricted to the thread's selected bases.
- **Empty or failing retrieval** (hold on, embed model gone, KB still ingesting) never blocks the
  turn. It sends without excerpts and says why in the `retrieval` event.

### 9.4 The `kb` toolset on `/mcp`

Registered like `docs`:
- a reserved prefix (`mcp/mod.rs` `RESERVED_NAMESPACES` grows to 3);
- `mcp/kb.rs` with `PREFIX`, `owns`, `list`, `call`;
- `exec.rs`: `KB_LABEL`, arms in `builtin_label` / `resolve_builtin` / `available_labels`, a
  `SplitExecutor` branch;
- `mcp/ingress.rs`: `list_tools`, `call_tool` and the instructions text;
- `mcp/inventory.rs`: an inventory block;
- `agents/mod.rs`: `ToolSurface` labels.
Key scopes work unchanged with `kb__*` globs. On `/mcp` only knowledge bases with
`mcp_visible` (default **on**, a switch per base) are listed or searchable. The Chat's own
selection ignores that switch.

### 9.5 Knowledge page

- The route is `/knowledge`, with a nav entry after Docs.
  - The list shows name, files, chunks, embed model, status and the live job.
  - **New knowledge base** asks for name, description, embed alias (aux embed models and cloud
    embedding aliases), optional reranker, optional vision alias, and chunk settings.
- The detail page has three tabs:
  - **Files**: drag-and-drop upload of several files; per file its status and reason, pages,
    skipped pages and chunks; delete; re-ingest.
  - **Search**: a playground showing query → excerpts with scores and trace. It is the way to
    judge retrieval.
  - **Settings**: edit the KB (a model change warns that it re-embeds everything), toggle
    `mcp_visible`, delete the KB (two-click).
- The backend is `/api/knowledge/*` (a dashboard backend, `Cap::Admin`, an exclusion row each).

## 10. Key scope editor

The alias scope and the tool scope of a client key are raw glob textareas today
(`pages/usage/key_editor.rs`), and creation (`KeyCreate`) takes no scope at all.

- **Tool scope:** mode (all / allow only / deny) and a **picker** built from `/api/tools`
  (`plane == "/mcp"`, not stale; the self-admin `lmgw` source is never offered to a client key).
  - Per source (the builtins `docs` and `kb`, and each server) there is a whole-source checkbox,
    which stores `<prefix>__*` and so follows new tools. The source expands to individual tools,
    which store exact names.
  - A server without a tool prefix has no namespace glob. Its whole-source choice enumerates the
    tools, with a note that new tools will not be included automatically.
  - The allow and deny modes use the same picker (in deny, the ticks mean "hidden").
  - Lines the picker cannot represent stay in a collapsed **Advanced patterns** textarea. Nothing
    hand-written is lost.
  - The live "Sees N of M tools" preview stays.
- **Alias scope:** mode and a model multi-picker from the shared model catalog (the same data as
  `ModelPicker`), plus the same Advanced textarea for globs.
- **Create:** `KeyCreate` gains a collapsed **Scope** section with both editors. `key_create`
  accepts the four scope fields optionally, with the same validation and refusals as `key_set`
  (owner keys refuse scopes). Its `OpDoc` and `openapi/ops/args.rs` are updated.
- **Shared widget.** Chat's `McpPicker` / `McpServerPick` / `ToolPick` move into
  `crates/lmgw-ui/src/widgets/tool_picker.rs` over a `Vec<{label, allowed_tools}>`-style
  selection. Chat and the key editor both use it; the key editor converts to and from
  `(mode, patterns)`. Glob derivation uses the tool **prefix** (`name.split_once("__")` /
  `McpServerView.tool_prefix`), not the server label.

## 11. Settings added

| Key | Default | Where |
|---|---|---|
| `chat_pdf_mode` | `text` (`text` / `images` / `ask`) | Settings → Chat |
| `chat_stt_alias` | empty (none) | Settings → Chat; an alias whose task is `asr` |
| `chat_kb_budget_tokens` | 4000 | Settings → Chat; each thread shows and overrides it |

## 12. Build order

Work packages (WPs) run sequentially unless marked parallel. Each one ends green on
`cargo fmt --check --all` and `cargo test -p <crates it touched>` and a fresh `trunk build` when
the UI changed, and is committed with explicit paths.

1. **WP1 Rendering** (§1). UI and assets only. Runs in parallel with WP2.
2. **WP2 Chat repository + temporary chats backend + `start_turn`** (§7 backend, §3 refactor).
   Core only.
3. **WP3 Sampling parameters** (§2).
4. **WP4 Message actions + temporary chat UI** (§3, §7 UI).
5. **WP5 Folders** (§5).
6. **WP6 Search** (§4).
7. **WP7 Export** (§6), including the Tauri download check.
8. **WP8 Extraction module + attachment kinds** (§8, the settings `chat_pdf_mode` and
   `chat_stt_alias`).
9. **WP9 Knowledge bases backend** (§9.1–9.4).
10. **WP10 Knowledge UI + Chat integration** (§9.3 UI, §9.5).
11. **WP11 Key scope editor** (§10).
12. Review (Opus, split by area), fixes, UI verification (`scripts/ui-matrix.py`, new
    `scripts/drive/*.json` checks, `scripts/webkit-check.py`), and a review page.

## 13. Defaults chosen during implementation (open to revision)

- Delete removes one message only. Edit-user always resends. Editing an assistant message
  drops its reasoning, token counts and tool record.
- Continue only on Anthropic (without extended thinking) and llama-server routes.
- Raw HTML in model output is escaped (§1.3). This is a behaviour change: a model's inline
  `<br>` or `<details>` shows as text.
- The sampling parameters the Chat withholds per protocol (§2), shown as "ignored".
- Search: 50 threads per page and 3 hits per thread, with the counts shown; no stemming.
- A thread lives in exactly one place in the sidebar. Pinned threads that are in a folder show
  in their folder.
- Export: Markdown without attachment bytes; JSON lossless; no import.
- Temporary chat is discarded silently on leaving; Keep converts it into a normal thread.
- `chat_pdf_mode` = text. The transcript is used for audio when the model has no audio input.
- Knowledge bases:
  - chunks of 512/64 tokens;
  - a 4000-token retrieval budget;
  - the query is the current plus the previous user message;
  - excerpts are stored with the message;
  - `mcp_visible` on by default;
  - originals are kept on disk under the data dir.
- The key scope editor also covers the alias scope, and creation gets both scopes.

## Not in this round

Branching and version history of messages, chat import, a mic/dictation button and read-aloud,
web search, compare mode, presets, a prompt library, memory, a code interpreter (see
`docs/ideas.md`), OCR of scanned PDFs in the Chat itself (the knowledge-base path has it), and
legacy OLE Office formats.
