-- no-transaction
-- llama-swap → llama-server router mode (§8): retag the upstream kind and
-- rename local-model columns to router terminology.
--
-- Runs outside a transaction because PRAGMA foreign_keys is a no-op inside
-- one, and `upstreams` must be rebuilt to change its CHECK constraint.

PRAGMA foreign_keys = OFF;

CREATE TABLE upstreams_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    protocol      TEXT NOT NULL CHECK (protocol IN ('openai','anthropic','gemini')),
    kind          TEXT NOT NULL DEFAULT 'generic' CHECK (kind IN ('generic','llama_server')),
    base_url      TEXT NOT NULL,
    api_key       TEXT,
    extra_headers TEXT NOT NULL DEFAULT '[]',   -- JSON [[name, value], ...]
    timeout_ms    INTEGER NOT NULL DEFAULT 120000,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO upstreams_new
SELECT id, name, protocol,
       CASE kind WHEN 'llama_swap' THEN 'llama_server' ELSE kind END,
       base_url, api_key, extra_headers, timeout_ms, enabled, created_at, updated_at
FROM upstreams;

DROP TABLE upstreams;
ALTER TABLE upstreams_new RENAME TO upstreams;

PRAGMA foreign_keys = ON;

ALTER TABLE local_models RENAME COLUMN swap_model_id TO model_id;
ALTER TABLE local_models RENAME COLUMN ttl_seconds TO idle_seconds;
