-- Device keys (client-apps design §1.1): `api_keys.kind` gains 'device', and
-- two columns arrive with it.
--
-- `last_seen_at` is when a device's long-lived connection (its feed, its host
-- link, a realtime session) last opened or closed (L15) — written on those
-- two edges, never per request, so the Devices card can say "last seen" for a
-- device that is not connected. `last_used` on the Keys page stays the usage
-- hour it always was.
--
-- `hosts_label` is the label a device may host MCP tools under (§1.5). Only a
-- device row carries one, which the CHECK below states.
--
-- SQLite cannot widen a CHECK in place, so this is 0037's create-copy-drop-
-- rename rebuild, copying **every** column for 0037's reason: a copy that left
-- `key_plain` or `agent_id` out would drop every agent token's plaintext and
-- its link to its agent. A device key is hash-only (L1), like a client key, so
-- the plaintext constraint is unchanged: only 'agent' and 'owner' keep one.
--
-- No `PRAGMA foreign_keys` bracket, for 0032's reason: nothing in the schema
-- declares REFERENCES api_keys, both `key_id` columns are soft links.
CREATE TABLE api_keys_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
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
                      CHECK (kind IN ('key','internal','agent','owner','device')),
    agent_id          TEXT,
    tool_scope_mode   TEXT NOT NULL DEFAULT 'all'
                      CHECK (tool_scope_mode IN ('all','allow','deny')),
    tool_scope_patterns TEXT NOT NULL DEFAULT '',
    last_seen_at      TEXT,
    hosts_label       TEXT,
    CHECK ((kind IN ('agent','owner')) = (key_plain IS NOT NULL)),
    CHECK ((kind =  'agent')           = (agent_id  IS NOT NULL)),
    CHECK (hosts_label IS NULL OR kind = 'device')
);

INSERT INTO api_keys_new
     (id, name, key_hash, key_plain, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind, agent_id, tool_scope_mode, tool_scope_patterns)
SELECT id, name, key_hash, key_plain, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind, agent_id, tool_scope_mode, tool_scope_patterns FROM api_keys;

-- Carry the AUTOINCREMENT watermark across the rebuild, not just the rows, for
-- 0032's reason: `request_logs.key_id` and `usage_hourly.key_id` are soft
-- links, so a re-used id inherits a deleted key's spend and history.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'api_keys_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'api_keys_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'api_keys'), 0))
 WHERE name = 'api_keys_new';

DROP TABLE api_keys;
ALTER TABLE api_keys_new RENAME TO api_keys;

-- One token per agent (0032), dropped with the old table: `upsert_agent_key`'s
-- `ON CONFLICT (agent_id)` targets it by name.
CREATE UNIQUE INDEX api_keys_agent ON api_keys(agent_id) WHERE agent_id IS NOT NULL;
-- One device per hosting label (§1.5). The label's uniqueness against the MCP
-- servers' prefixes is checked where it is written; this is the backstop
-- between two devices.
CREATE UNIQUE INDEX api_keys_hosts_label ON api_keys(hosts_label) WHERE hosts_label IS NOT NULL;
