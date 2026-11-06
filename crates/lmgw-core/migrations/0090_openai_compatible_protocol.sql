-- no-transaction
-- The protocol `openai` becomes the OpenAI platform only, and every other
-- OpenAI-shaped provider gets a protocol of its own, `openai_compatible`
-- (openai egress design §4.1, decisions 1, 2, 7, 8 and 21).
--
-- SQLite cannot alter a CHECK, so this is 0058's twelve-step table rebuild
-- again, for the same reasons: `-- no-transaction` only because `PRAGMA
-- foreign_keys` is a no-op inside a transaction (`models` and
-- `hidden_passthrough_models` reference `upstreams(id) ON DELETE CASCADE`),
-- the rebuild itself one `BEGIN … COMMIT`, and the foreign-key check in Rust
-- after the migrator (`store::migration_guards::foreign_keys`).
--
-- Replay-safe: a crash between `COMMIT` and sqlx's record replays the file
-- against the new table, where no row is `openai` any more and no `anthropic`
-- or `gemini` row has `supports_responses`, so the `CASE`s map nothing twice.
--
-- What the copy changes, and the start notice names row by row
-- (`store::migration_guards::openai_compatible_notice`):
-- * every `openai` row becomes `openai_compatible`, whatever its host
--   (decision 2) — the owner moves the api.openai.com row back to `openai` on
--   the Upstreams page; `openai` + `audio_cpp` becomes `openai_compatible` +
--   `audio_cpp`, which is allowed, so no row is refused;
-- * an `audio_cpp` row whose protocol is not (and does not become)
--   `openai_compatible` — only `anthropic` or `gemini` ones can exist — becomes
--   kind `generic` (decision 7), and the notice names it;
-- * `supports_responses` is cleared on `anthropic` and `gemini` rows: neither
--   API serves `/v1/responses` (decisions 8 and 21).

PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE upstreams_new (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    name                   TEXT NOT NULL UNIQUE,
    protocol               TEXT NOT NULL
                           CHECK (protocol IN
                               ('openai','openai_compatible','anthropic','gemini','llama_cpp')),
    kind                   TEXT NOT NULL DEFAULT 'generic'
                           CHECK (kind IN ('generic','llama_server','audio_cpp')),
    base_url               TEXT NOT NULL,
    api_key                TEXT,
    extra_headers          TEXT NOT NULL DEFAULT '[]',   -- JSON [[name, value], ...]
    timeout_ms             INTEGER NOT NULL DEFAULT 120000,
    enabled                INTEGER NOT NULL DEFAULT 1,
    expose_all             INTEGER NOT NULL DEFAULT 0,
    expose_prefix          TEXT NOT NULL DEFAULT '',
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at             TEXT NOT NULL DEFAULT (datetime('now')),
    supports_responses     INTEGER NOT NULL DEFAULT 0,               -- 0015
    catalog_all_modalities INTEGER NOT NULL DEFAULT 1                -- 0080
                           CHECK (catalog_all_modalities IN (0, 1)),
    -- The OpenAI platform is always a generic row (decision 7).
    CHECK (protocol <> 'openai' OR kind = 'generic'),
    -- Native /v1/responses is a choice on openai_compatible rows only: implied
    -- on openai, never on llama_cpp, and anthropic and gemini do not serve it
    -- (decision 8).
    CHECK (supports_responses = 0 OR protocol = 'openai_compatible'),
    -- An audio.cpp server is an OpenAI-shaped HTTP server (decision 7).
    CHECK (kind <> 'audio_cpp' OR protocol = 'openai_compatible'),
    -- llama_cpp is always a llama-server (0058).
    CHECK (protocol <> 'llama_cpp' OR kind = 'llama_server'),
    -- A llama-server speaks llama_cpp, or Anthropic's /v1/messages (0058).
    CHECK (kind <> 'llama_server' OR protocol IN ('llama_cpp','anthropic'))
);

-- Every column and every id; the mapping in the `CASE`s.
INSERT INTO upstreams_new
    (id, name, protocol, kind, base_url, api_key, extra_headers, timeout_ms,
     enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses,
     catalog_all_modalities)
SELECT id, name,
       CASE WHEN protocol = 'openai' THEN 'openai_compatible' ELSE protocol END,
       CASE WHEN kind = 'audio_cpp' AND protocol NOT IN ('openai','openai_compatible')
            THEN 'generic' ELSE kind END,
       base_url, api_key, extra_headers, timeout_ms,
       enabled, expose_all, expose_prefix, created_at, updated_at,
       CASE WHEN protocol IN ('anthropic','gemini') THEN 0 ELSE supports_responses END,
       catalog_all_modalities
FROM upstreams;

-- Carry the AUTOINCREMENT high-water mark, as 0058 does: `DROP TABLE
-- upstreams` takes its `sqlite_sequence` row with it, and the copy only raises
-- the new table's to max(id), so a deleted top id would be handed out again
-- and inherit its prices and usage rollups. Two statements because the copy
-- creates no sequence row for an empty table.
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
