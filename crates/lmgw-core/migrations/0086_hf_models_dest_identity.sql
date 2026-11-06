-- no-transaction
-- A tracked download is its file **at its place**: `(repo, file, target,
-- dest_path)`, not `(repo, file, target)`. The shipped upscaler
-- (`image_upscaler_add`, chat-media-tools design §14) fetches a repo's file
-- into `upscalers/<file>` of the image models dir, while `hf_add` of the same
-- file puts it at `<repo>/<file>`; under the old key the second queue moved
-- the first row's `dest_path`, and the copy already on disk was left
-- untracked (no update, no delete from the Downloads page). Now each place is
-- a row of its own, and a re-queue of the same place re-queues its row.
--
-- Rebuilt rather than ALTER'd because SQLite cannot change a table UNIQUE
-- constraint in place — the create-copy-drop-rename 0008, 0018 and 0034 did
-- for this table, with the same `-- no-transaction` header (first line, or
-- sqlx ignores it) so the `PRAGMA foreign_keys` bracket is real.

PRAGMA foreign_keys = OFF;

CREATE TABLE hf_models_new (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    repo               TEXT NOT NULL,
    file               TEXT NOT NULL,
    dest_path          TEXT NOT NULL,
    target             TEXT NOT NULL DEFAULT 'chat' CHECK (target IN ('chat','aux','audio','image')),
    etag               TEXT,
    size_bytes         INTEGER,
    status             TEXT NOT NULL DEFAULT 'queued'
                       CHECK (status IN ('queued','downloading','done','failed','update_available')),
    error              TEXT,
    downloaded_at      TEXT,
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    requested_revision TEXT,
    resolved_commit    TEXT,
    UNIQUE (repo, file, target, dest_path)
);

INSERT INTO hf_models_new
    (id, repo, file, dest_path, target, etag, size_bytes, status, error, downloaded_at,
     created_at, requested_revision, resolved_commit)
SELECT id, repo, file, dest_path, target, etag, size_bytes, status, error, downloaded_at,
       created_at, requested_revision, resolved_commit
FROM hf_models;

DROP TABLE hf_models;
ALTER TABLE hf_models_new RENAME TO hf_models;

PRAGMA foreign_keys = ON;
