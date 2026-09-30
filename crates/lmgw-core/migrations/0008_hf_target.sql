-- no-transaction
-- HF downloads now serve two models dirs: the chat router's and the embed
-- router's. Tag each tracked file with its `target` so the downloader writes
-- into the right dir, and widen the uniqueness to (repo, file, target) so the
-- same GGUF can be tracked independently per dir.
--
-- Rebuilt rather than ALTER'd because the UNIQUE(repo,file) table constraint
-- has to change (SQLite can't drop a table constraint in place). Existing rows
-- are all chat downloads.

PRAGMA foreign_keys = OFF;

CREATE TABLE hf_models_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,
    file          TEXT NOT NULL,
    dest_path     TEXT NOT NULL,
    target        TEXT NOT NULL DEFAULT 'chat' CHECK (target IN ('chat','embed')),
    etag          TEXT,
    size_bytes    INTEGER,
    status        TEXT NOT NULL DEFAULT 'queued'
                  CHECK (status IN ('queued','downloading','done','failed','update_available')),
    error         TEXT,
    downloaded_at TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (repo, file, target)
);

INSERT INTO hf_models_new
    (id, repo, file, dest_path, target, etag, size_bytes, status, error, downloaded_at, created_at)
SELECT id, repo, file, dest_path, 'chat', etag, size_bytes, status, error, downloaded_at, created_at
FROM hf_models;

DROP TABLE hf_models;
ALTER TABLE hf_models_new RENAME TO hf_models;

PRAGMA foreign_keys = ON;
