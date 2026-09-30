-- Agent identity: the token, its agent link, and the two catalog columns the
-- container runtime needs (agent-container-runtime design §5).
--
-- SQLite cannot alter a CHECK, so the `kind IN ('key','internal')` constraint
-- from 0029_key_policy.sql is replaced by the create-copy-drop-rename rebuild
-- 0030_usage_fidelity.sql already uses. Written out in full because
-- request_logs.key_id (0027) and usage_hourly.key_id (0028) point at these ids,
-- so they must survive the copy verbatim.
--
-- No `PRAGMA foreign_keys = OFF/ON` bracket, unlike the rebuilds in 0002, 0008,
-- 0013 and 0018: nothing in the schema declares REFERENCES api_keys — both
-- key_id columns are soft links (0028's is a plain INTEGER NOT NULL DEFAULT 0)
-- — so there is no constraint to suspend.
CREATE TABLE api_keys_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,  -- as 0001_init.sql:40; see below
    name              TEXT NOT NULL UNIQUE,
    key_hash          TEXT NOT NULL,
    key_plain         TEXT,                          -- kind='agent' only (§3.1); 0600 DB, like mcp_servers.env
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
                      CHECK (kind IN ('key','internal','agent')),
    agent_id          TEXT,
    CHECK (kind <> 'agent' OR (key_plain IS NOT NULL AND agent_id IS NOT NULL)),
    CHECK (kind =  'agent' OR (key_plain IS NULL     AND agent_id IS NULL))
);

INSERT INTO api_keys_new
     (id, name, key_hash, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind)
SELECT id, name, key_hash, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind FROM api_keys;

-- Carry the AUTOINCREMENT **watermark** across the rebuild, not just the rows.
--
-- `DROP TABLE api_keys` takes its `sqlite_sequence` row with it, so without
-- this the rebuilt table's high-water mark is whatever the copy happened to
-- set — `max(id)` — and the next key created re-uses the id of a key that was
-- deleted *before* the migration ran. That id is soft-linked from
-- `request_logs.key_id` and `usage_hourly.key_id`, so the new key would inherit
-- the deleted one's spend, call count and history: the exact failure the
-- AUTOINCREMENT below exists to prevent, reintroduced by the rebuild that
-- preserves it.
--
-- Two statements because the copy only creates a sequence row when it inserted
-- something: an install whose `api_keys` was empty (a fresh one, or one whose
-- every key was deleted) has no `api_keys_new` row to UPDATE yet.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'api_keys_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'api_keys_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'api_keys'), 0))
 WHERE name = 'api_keys_new';

DROP TABLE api_keys;
ALTER TABLE api_keys_new RENAME TO api_keys;

-- One token per agent (§3.1), enforced here rather than in the code that mints
-- it: a second row for the same agent would make "the agent's key" ambiguous
-- on the lookup path.
CREATE UNIQUE INDEX api_keys_agent ON api_keys(agent_id) WHERE agent_id IS NOT NULL;

-- AUTOINCREMENT is kept deliberately. Without it SQLite hands out max(id)+1 and
-- therefore **re-uses a deleted row's id** — and `agent_delete` now deletes key
-- rows, so the next key created would silently inherit the deleted agent's
-- request_logs.key_id / usage_hourly.key_id history. The explicit `id` column in
-- the INSERT above is what preserves the existing ids across the copy, and the
-- `sqlite_sequence` carry-over above is what preserves the watermark;
-- AUTOINCREMENT only governs what comes after.

-- Package provenance and the dev override (§3.4).
ALTER TABLE agents ADD COLUMN provenance TEXT NOT NULL DEFAULT '{}';
ALTER TABLE agents ADD COLUMN dev_url TEXT;

-- An agent-owned MCP registration (§3.3), removed with its agent.
ALTER TABLE mcp_servers ADD COLUMN agent_id TEXT;
