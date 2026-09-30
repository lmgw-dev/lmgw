-- no-transaction
-- HF downloads now serve a fourth models dir (the sd-server class's). Widen
-- the target CHECK to include 'image'. Rebuilt rather than ALTER'd because
-- SQLite cannot change a table CHECK constraint in place — the same
-- create-copy-drop-rename 0008 and 0018 already did for this table.
--
-- Its own file, separate from 0033's `CREATE TABLE image_models`, and
-- deliberately: sqlx honours `-- no-transaction` only when it is the **first**
-- line of the migration (`sqlx-core/src/migrate/source.rs`:
-- `sql.starts_with("-- no-transaction")`), so a combined file would have to
-- run the table creation outside a transaction too — and a failure halfway
-- through would then leave `image_models` created, the migration unrecorded,
-- and every later start replaying a `CREATE TABLE` that now fails. Split, the
-- unprotected half is a single rebuild with nothing before it to undo.
--
-- The `PRAGMA foreign_keys` bracket is kept for symmetry with 0008/0018 even
-- though nothing declares `REFERENCES hf_models`: outside a transaction the
-- pragma is real, which is the whole point of the header above it.
--
-- The downloader itself does not offer `image` yet (WP3) — this is the column
-- constraint that has to exist before it can.

PRAGMA foreign_keys = OFF;

CREATE TABLE hf_models_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,
    file          TEXT NOT NULL,
    dest_path     TEXT NOT NULL,
    target        TEXT NOT NULL DEFAULT 'chat' CHECK (target IN ('chat','aux','audio','image')),
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
SELECT id, repo, file, dest_path, target, etag, size_bytes, status, error, downloaded_at, created_at
FROM hf_models;

DROP TABLE hf_models;
ALTER TABLE hf_models_new RENAME TO hf_models;

PRAGMA foreign_keys = ON;
