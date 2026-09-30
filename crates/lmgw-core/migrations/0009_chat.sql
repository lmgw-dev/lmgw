-- Chat tab: persisted "test the models" conversations. Each thread carries its
-- own model + sampling settings so different threads can target different models
-- (the alias resolves through `Snapshot::resolve` exactly like an API client).
-- Messages store the rendered text plus per-message token counts, which feed the
-- live stats panel and let a reopened thread show its history without a refetch.
CREATE TABLE chat_threads (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    title         TEXT NOT NULL DEFAULT 'New chat',
    model_alias   TEXT NOT NULL DEFAULT '',     -- client-facing model name (alias / local / passthrough)
    system_prompt TEXT NOT NULL DEFAULT '',
    temperature   REAL,                          -- NULL = upstream/alias default
    max_tokens    INTEGER,                       -- NULL = model's real remaining context budget
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE chat_messages (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id         INTEGER NOT NULL REFERENCES chat_threads(id) ON DELETE CASCADE,
    role              TEXT NOT NULL,             -- 'user' | 'assistant' | 'system'
    content           TEXT NOT NULL,
    prompt_tokens     INTEGER,                   -- as reported by the upstream (assistant rows)
    completion_tokens INTEGER,
    created_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_chat_messages_thread ON chat_messages(thread_id, id);
