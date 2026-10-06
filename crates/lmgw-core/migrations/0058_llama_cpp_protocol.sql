-- no-transaction
-- llama.cpp gets a protocol of its own: `upstreams.protocol` gains `llama_cpp`,
-- and protocol and kind can no longer disagree (llama.cpp egress design §5,
-- decisions 10, 11, 12 and 19).
--
-- SQLite cannot alter a CHECK, so this is its twelve-step table rebuild. The
-- file is `-- no-transaction` only because `PRAGMA foreign_keys` is a no-op
-- inside a transaction: `models` and `hidden_passthrough_models` reference
-- `upstreams(id) ON DELETE CASCADE`, and with the pragma on, `DROP TABLE
-- upstreams` would run an implicit DELETE that cascades through every alias.
-- The rebuild itself is one `BEGIN … COMMIT`, so a crash leaves either the old
-- table or the new one, never neither. `PRAGMA foreign_key_check` (step 10) runs
-- in Rust after the migrator (`store::run_migrations`): here it would only
-- return rows, and nobody would read them.
--
-- Replay-safe. sqlx records the migration after the file ran, so a crash
-- between `COMMIT` and that record replays the whole file against the new
-- table: every `CASE` below maps a row it already mapped to itself.
--
-- What the copy changes, and the start notice names row by row
-- (`store::migration_guards::llama_cpp_notice`):
-- * `openai` + `llama_server` is the old spelling of `llama_cpp`;
-- * `llama_cpp` never forwards `/v1/responses` natively (decision 19), so
--   `supports_responses` is cleared on those rows;
-- * `gemini` + `llama_server` is a wire llama-server does not serve: the row
--   keeps its protocol and becomes `generic` (decision 12);
-- * `anthropic` + `llama_server` stays as it is — llama-server serves
--   `/v1/messages`, and the kind is what makes such a row local and free.

PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE upstreams_new (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    name               TEXT NOT NULL UNIQUE,
    protocol           TEXT NOT NULL
                       CHECK (protocol IN ('openai','anthropic','gemini','llama_cpp')),
    kind               TEXT NOT NULL DEFAULT 'generic'
                       CHECK (kind IN ('generic','llama_server','audio_cpp')),
    base_url           TEXT NOT NULL,
    api_key            TEXT,
    extra_headers      TEXT NOT NULL DEFAULT '[]',   -- JSON [[name, value], ...]
    timeout_ms         INTEGER NOT NULL DEFAULT 120000,
    enabled            INTEGER NOT NULL DEFAULT 1,
    expose_all         INTEGER NOT NULL DEFAULT 0,
    expose_prefix      TEXT NOT NULL DEFAULT '',
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at         TEXT NOT NULL DEFAULT (datetime('now')),
    supports_responses INTEGER NOT NULL DEFAULT 0,               -- 0015
    -- llama_cpp is always a llama-server, and never forwards /v1/responses.
    CHECK (protocol <> 'llama_cpp' OR (kind = 'llama_server' AND supports_responses = 0)),
    -- A llama-server speaks llama_cpp, or Anthropic's /v1/messages.
    CHECK (kind <> 'llama_server' OR protocol IN ('llama_cpp','anthropic'))
);

-- Every column and every id; the mapping in the `CASE`s.
INSERT INTO upstreams_new
    (id, name, protocol, kind, base_url, api_key, extra_headers, timeout_ms,
     enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
SELECT id, name,
       CASE WHEN protocol = 'openai' AND kind = 'llama_server' THEN 'llama_cpp'
            ELSE protocol END,
       CASE WHEN protocol = 'gemini' AND kind = 'llama_server' THEN 'generic'
            ELSE kind END,
       base_url, api_key, extra_headers, timeout_ms,
       enabled, expose_all, expose_prefix, created_at, updated_at,
       CASE WHEN protocol IN ('openai','llama_cpp') AND kind = 'llama_server' THEN 0
            ELSE supports_responses END
FROM upstreams;

-- Carry the AUTOINCREMENT high-water mark, as 0032 does for `api_keys`.
-- `DROP TABLE upstreams` takes its `sqlite_sequence` row with it, and the copy
-- only raises the new table's to max(id). Without this a deleted top id would
-- be handed out again, and prices (`'<upstream_id>:<model>'`, 0026) and usage
-- rollups (0028), which deleting an upstream cleans neither of, would attach
-- to the new row. Two statements because the copy creates no sequence row for
-- an empty table.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'upstreams_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'upstreams_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'upstreams'), 0))
 WHERE name = 'upstreams_new';

DROP TABLE upstreams;
ALTER TABLE upstreams_new RENAME TO upstreams;

COMMIT;

PRAGMA foreign_keys = ON;
