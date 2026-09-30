# Chat: auto-archive, pinning, file attachments

Requested by the owner 2026-09-25. Defaults below were chosen during implementation and are
open to revision.

## Why

Chat threads only ever accumulate. Requirement: idle threads are archived after two weeks and archived
threads deleted a month later (both configurable), pinned threads that sit on top and never
archive or delete, and files in a prompt: images for models that see, text files (source code,
txt) for every model. More file types come later, so the attachment path has one extension point.

## 1. Archive, purge, pin

Schema (migration `0038_chat_archive_attachments.sql`), on `chat_threads`:

- `pinned INTEGER NOT NULL DEFAULT 0`
- `archived_at TEXT` — NULL = active.

Rules:

- The idle clock is `updated_at`, which a new message or a settings change already bumps.
  Pinning does not touch it, so unpinning a long-idle thread archives it at the next sweep
  (archiving is reversible; that is the intended reading of "idle for N days").
- Sweep: `archived_at = now` where `pinned = 0 AND archived_at IS NULL AND updated_at < now - archive_days`;
  delete where `pinned = 0 AND archived_at < now - purge_days`. The purge clock is `archived_at`,
  so a thread archived by hand also gets the full `purge_days`.
- `0` switches a step off (`archive_days = 0`: never auto-archive; `purge_days = 0`: never delete
  archived threads), the same "zero means none" convention as the other retention knobs.
- Pinning an archived thread restores it. Restoring bumps `updated_at` (else the next sweep
  re-archives it). Sending into an archived thread restores it.
- The sweep runs in the existing hourly janitor in `server.rs` (whose first tick fires at boot,
  which matters: the app is started by hand each morning). It logs the counts at info level
  when either is non-zero.

Settings (`config.rs` `Settings`, dashboard Settings page, MCP `lmgw__settings` /
`lmgw__settings_set`): `chat_archive_days` (default 14), `chat_purge_days` (default 30).

API (all under the existing Admin-gated `labs_routes()`, each with a `CAPABILITY_TABLE` row):

- `GET /chat/api/threads` — active threads, pinned first, then `updated_at DESC, id DESC`.
  `GET /chat/api/threads?archived=1` — archived threads, `archived_at DESC`.
  Each thread gains `pinned: bool`, `archived_at: string|null`, and `purge_at: string|null`
  (archived_at + purge_days, computed server-side; null when active, pinned, or purge is off).
  The list response also carries `archived_count` so the UI can label its toggle.
- `POST /chat/api/threads/{id}/pin` `{pinned: bool}` → the updated thread.
- `POST /chat/api/threads/{id}/archive` `{archived: bool}` → the updated thread.

## 2. Attachments

An attachment is uploaded when it is picked, lives on the thread as part of the composer draft
until a send binds it to that user message, and is replayed with that message on every later
turn.

Schema (same migration):

```sql
CREATE TABLE chat_attachments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id  INTEGER NOT NULL REFERENCES chat_threads(id) ON DELETE CASCADE,
    message_id INTEGER REFERENCES chat_messages(id) ON DELETE CASCADE, -- NULL = draft
    kind       TEXT NOT NULL,   -- 'image' | 'text'; future kinds add a variant
    name       TEXT NOT NULL,
    mime       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    data       BLOB NOT NULL,
    ord        INTEGER NOT NULL DEFAULT 0, -- position in its message, from the send's id list
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

Drafts need no garbage collection: they are visible as chips in that thread's composer and go
with the thread.

Kind is decided from the bytes, never from the file extension or the browser's MIME:

- PNG, JPEG, GIF, WebP magic → `image` with that MIME.
- Otherwise valid UTF-8 with no NUL byte → `text`, MIME `text/plain; charset=utf-8`.
- Otherwise 415 `unsupported_attachment`, message naming what is supported.

One module owns kinds (`web/chat_attach.rs` or similar): detection, and rendering an attachment
into IR `ContentPart`s for a given model. That function is the extension point for PDFs, audio
and the rest.

Rendering into the model request, in `build_messages` (both the plain and the agentic send path):

- A user message's parts are its attachments in the order the send listed them (the chips'
  order, stored as `ord`), then its typed text (long material first, question last).
- `text` → one text part: `<file name="NAME">\nCONTENT\n</file>`.
- `image` → `ContentPart::Image` base64, when the resolved model's `capabilities.vision` is
  `Some(true)` or unknown (`None`; the upstream then answers for itself).
- `image` for a model with `vision == Some(false)`: a new message carrying one is refused with
  400 `model_no_vision` naming the model. An image already in history is replaced by the text
  part `[image "NAME" not sent: MODEL does not accept images]`, and the UI says so (below).
- Attachment bytes are never copied into `ir_messages` or the request log.

Size: the upload route reads its body against the existing `max_body_mb` limit (declared or
chunked), so an oversize file gets a 413 `body_limit` naming the setting, as `/v1` does. No other cap: a text file too big
for the context window surfaces as the upstream's error, never a silent truncation.

API:

- `POST /chat/api/threads/{id}/attachments?name=FILENAME` — raw body → `{id, kind, name, mime, size}`.
- `POST /chat/api/attachments/{id}/delete` — drafts only; 409 once sent.
- `GET /chat/api/attachments/{id}` — the bytes. `image` with its sniffed MIME, `text` always as
  `text/plain; charset=utf-8`; both with `X-Content-Type-Options: nosniff` and
  `Content-Security-Policy: sandbox`, so nothing uploaded can run on the dashboard origin.
- `GET /chat/api/threads/{id}`: each message gains `attachments: [{id, kind, name, mime, size}]`;
  the response gains `draft_attachments` (same shape).
- `POST /chat/api/threads/{id}/send` `{content, attachments: [id…]}` — ids must be drafts of this
  thread, else 400. `content` may be empty when attachments are present. A thread titled from
  its first message uses the first attachment's name when the text is empty.

## 3. UI (Chat page)

- Thread list: a "Pinned" group above the date groups. Each row's actions move into the
  shared `RowMenu`: Pin/Unpin, Archive/Restore, Delete (two-step, as today). A pinned row shows
  a pin mark.
- A toggle in the list toolbar switches to "Archived (n)". Archived rows say when they will be
  deleted (`purge_at`). An open archived thread shows a strip above the composer: archived on
  X, deleted on Y, with Restore.
- Composer: a paperclip button (file picker, multiple), drop onto the chat column, and paste
  of clipboard images. Each file uploads at once and becomes a chip above the composer: image
  thumbnail or file name, size, remove ✕; an upload error stays on its chip with the server's
  message.
- Vision gating uses the model catalog. The catalog's vision flag must distinguish unknown from
  no. When the thread's model is known not to see, image chips are marked, Send is disabled
  with the reason, and history images carry a note that they are not sent to this model.
- Sent messages show their attachments as chips; an image chip opens the image, a text chip
  opens its content in a Modal.

## Not in this round

PDFs, audio, other binaries. Token estimate per text chip. A server cursor for the thread list.
