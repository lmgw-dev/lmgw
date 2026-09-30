-- Hugging Face model manager (§8): GGUF files downloaded from HF repos into
-- the models dir, tracked for update checks (ETag comparison on the resolve
-- URL). Live byte progress is in-memory only; this is the durable state.
CREATE TABLE hf_models (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,                -- e.g. unsloth/gemma-4-31B-it-GGUF
    file          TEXT NOT NULL,                -- path within the repo
    dest_path     TEXT NOT NULL,                -- relative to the models dir
    etag          TEXT,                         -- ETag of the last completed download
    size_bytes    INTEGER,
    status        TEXT NOT NULL DEFAULT 'queued'
                  CHECK (status IN ('queued','downloading','done','failed','update_available')),
    error         TEXT,
    downloaded_at TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (repo, file)
);
