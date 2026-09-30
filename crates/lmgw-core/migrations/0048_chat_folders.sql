-- Chat folders (chat-complete design §5): one level, no nesting. A folder
-- groups threads and can carry defaults a *new* thread in it starts with
-- (`defaults` is the JSON of `ThreadDefaults`; '{}' = none). Deleting a
-- folder never takes a thread with it by itself: the column goes back to NULL
-- (the "delete threads" choice removes them explicitly, in one transaction).
CREATE TABLE chat_folders (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    sort       INTEGER NOT NULL DEFAULT 0,
    defaults   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

ALTER TABLE chat_threads ADD COLUMN folder_id INTEGER REFERENCES chat_folders(id) ON DELETE SET NULL;
CREATE INDEX idx_chat_threads_folder ON chat_threads(folder_id);
