-- Chat: auto-archive, pinning and file attachments (chat-archive-pin-
-- attachments design §1, §2).
--
-- `chat_threads` gains two columns over the one idle clock it already had
-- (`updated_at`, bumped by a new message or a settings change): `pinned`
-- keeps a thread off the sweep entirely, `archived_at` is when the sweep (or
-- the owner, by hand) archived it — NULL means active. Restoring a thread
-- (unarchiving, pinning an archived one, or sending into one) clears
-- `archived_at` and bumps `updated_at`, or the next sweep would re-archive it
-- at once.
--
-- `chat_attachments` is the upload: a draft (`message_id IS NULL`) lives with
-- the thread until a send binds it to that user message, then is replayed
-- with it on every later turn. `kind` is decided from the bytes at upload
-- time, never the filename or the browser's declared type — 'image' or
-- 'text' today, with room for a future kind to add its own variant rather
-- than a new column. Both parent links cascade: deleting a thread (or, for a
-- sent attachment, its message) takes its attachments with it.
ALTER TABLE chat_threads ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chat_threads ADD COLUMN archived_at TEXT;

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

CREATE INDEX idx_chat_attachments_thread ON chat_attachments(thread_id, id);
CREATE INDEX idx_chat_attachments_message ON chat_attachments(message_id);
