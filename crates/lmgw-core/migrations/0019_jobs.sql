-- Generalized background jobs (quickdoc design §9c).
--
-- Replaces the bespoke in-memory download registry: every long-running unit of
-- work — an HF transfer today, corpus ingestion / re-embedding / eval runs next
-- — gets one durable row here, so a job that was interrupted by a shutdown is
-- visible as such instead of vanishing with the process.
--
-- `kind` is text rather than an enum CHECK on purpose: a new kind ships as one
-- executor impl, and a migration to widen a constraint every time would make
-- that a two-place change. Rows whose kind has no registered executor are
-- listed but never run.
CREATE TABLE jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    -- Dedup/correlation key scoped to the kind (`hf:<hf_models.id>`). NULL for
    -- kinds that allow unlimited concurrent instances.
    key         TEXT,
    label       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'queued'
                CHECK (status IN ('queued','running','done','failed','canceled')),
    -- Kind-specific request payload and last progress snapshot, both JSON.
    input       TEXT NOT NULL DEFAULT '{}',
    progress    TEXT NOT NULL DEFAULT '{}',
    result      TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    started_at  TEXT,
    finished_at TEXT
);

CREATE INDEX jobs_status_idx ON jobs (status, id DESC);

-- One live job per (kind, key): the duplicate guard the download manager used
-- to keep in a HashMap, now enforced where the truth lives. Finished rows fall
-- out of the index, so the same key can be re-run any number of times.
CREATE UNIQUE INDEX jobs_active_key_idx ON jobs (kind, key)
    WHERE key IS NOT NULL AND status IN ('queued','running');
