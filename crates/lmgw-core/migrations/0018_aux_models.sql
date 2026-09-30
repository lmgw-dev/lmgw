-- no-transaction
-- The second llama-server container stops being "the embed router" and becomes
-- the **aux** router: one container for every small stateless model class,
-- which is now embeddings *and* rerankers (llama.cpp `reranking = true`).
-- Nothing physical moves — the Podman container name, its port and the
-- client-facing `embed/` prefix are settings an install already owns and they
-- are left exactly as they are. What changes is lmgw's own vocabulary, so this
-- migration carries the four places that vocabulary is stored.

-- 1. The model table gains a class discriminant. Every existing row is an
--    embedder, which is what the default says.
ALTER TABLE embed_models RENAME TO aux_models;
ALTER TABLE aux_models
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'embed' CHECK (kind IN ('embed','rerank'));

-- 2. Download targets follow the container's name. Rebuilt rather than ALTER'd
--    because SQLite can't change a table CHECK constraint in place.
PRAGMA foreign_keys = OFF;

CREATE TABLE hf_models_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,
    file          TEXT NOT NULL,
    dest_path     TEXT NOT NULL,
    target        TEXT NOT NULL DEFAULT 'chat' CHECK (target IN ('chat','aux','audio')),
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
SELECT id, repo, file, dest_path,
       CASE target WHEN 'embed' THEN 'aux' ELSE target END,
       etag, size_bytes, status, error, downloaded_at, created_at
FROM hf_models;

DROP TABLE hf_models;
ALTER TABLE hf_models_new RENAME TO hf_models;

PRAGMA foreign_keys = ON;

-- 3. The managed upstream row that fronts the container. Renamed in place so
--    its id — and therefore every alias pointing at it — survives; provisioning
--    looks the row up by name, so leaving it would silently create a second
--    expose_all upstream on the same port.
UPDATE upstreams SET name = 'llama-aux' WHERE name = 'llama-embed';

-- 4. The settings blob's key. `Settings` also carries a serde alias for the old
--    spelling, but that only covers a blob arriving from outside the DB (a
--    hand-edited value, a restored backup) — the stored one is migrated here so
--    the next save can't write the legacy key back.
UPDATE settings
SET value = json_remove(
        json_set(value, '$.aux_router', json_extract(value, '$.embed_router')),
        '$.embed_router')
WHERE key = 'settings' AND json_extract(value, '$.embed_router') IS NOT NULL;
