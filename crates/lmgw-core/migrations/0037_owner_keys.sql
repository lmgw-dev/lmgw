-- The owner principal: `api_keys.kind` gains 'owner' (principals design §6).
--
-- SQLite cannot widen a CHECK in place, so this is the same create-copy-drop-
-- rename rebuild 0032_agent_containers.sql used — with one deliberate
-- difference. 0032's copy listed neither `key_plain` nor `agent_id`, because
-- it *added* those two columns; repeating that list here would drop every
-- agent token's plaintext and every token's link to its agent, and then fail
-- the CHECK below on the way out. Every column is copied.
--
-- The old table's single pair of constraints tied plaintext and agent id
-- together ("both, or neither"). An owner key keeps a plaintext — lmgw has to
-- hand it back on the Keys page and to the shell, exactly as an agent's is
-- handed to its container — but has no agent, so the pair is split into one
-- constraint per column.
--
-- No `PRAGMA foreign_keys` bracket, for 0032's reason: nothing in the schema
-- declares REFERENCES api_keys, both `key_id` columns are soft links.
CREATE TABLE api_keys_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,  -- see the note below
    name              TEXT NOT NULL UNIQUE,
    key_hash          TEXT NOT NULL,
    key_plain         TEXT,                          -- kind='agent' or 'owner'; 0600 DB
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    scope_mode        TEXT NOT NULL DEFAULT 'all'
                      CHECK (scope_mode IN ('all','allow','deny')),
    scope_patterns    TEXT NOT NULL DEFAULT '',
    budget_micro      INTEGER NOT NULL DEFAULT 0,
    budget_period     TEXT NOT NULL DEFAULT 'month'
                      CHECK (budget_period IN ('day','month','total')),
    rpm_limit         INTEGER NOT NULL DEFAULT 0,
    tpm_limit         INTEGER NOT NULL DEFAULT 0,
    concurrency_limit INTEGER NOT NULL DEFAULT 0,
    expires_at        TEXT,
    note              TEXT NOT NULL DEFAULT '',
    kind              TEXT NOT NULL DEFAULT 'key'
                      CHECK (kind IN ('key','internal','agent','owner')),
    agent_id          TEXT,
    CHECK ((kind IN ('agent','owner')) = (key_plain IS NOT NULL)),
    CHECK ((kind =  'agent')           = (agent_id  IS NOT NULL))
);

INSERT INTO api_keys_new
     (id, name, key_hash, key_plain, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind, agent_id)
SELECT id, name, key_hash, key_plain, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind, agent_id FROM api_keys;

-- Carry the AUTOINCREMENT watermark across the rebuild, not just the rows —
-- 0032 explains at length why: `request_logs.key_id` and `usage_hourly.key_id`
-- are soft links, so a re-used id inherits a deleted key's spend and history.
-- Two statements because the copy only creates a `sqlite_sequence` row when it
-- inserted something.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'api_keys_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'api_keys_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'api_keys'), 0))
 WHERE name = 'api_keys_new';

DROP TABLE api_keys;
ALTER TABLE api_keys_new RENAME TO api_keys;

-- One token per agent (0032). Dropped with the old table, so it is recreated
-- here — `upsert_agent_key`'s `ON CONFLICT (agent_id)` targets it by name.
CREATE UNIQUE INDEX api_keys_agent ON api_keys(agent_id) WHERE agent_id IS NOT NULL;
