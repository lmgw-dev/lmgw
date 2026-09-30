-- lmgw initial schema (§9)

CREATE TABLE upstreams (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    protocol      TEXT NOT NULL CHECK (protocol IN ('openai','anthropic','gemini')),
    kind          TEXT NOT NULL DEFAULT 'generic' CHECK (kind IN ('generic','llama_swap')),
    base_url      TEXT NOT NULL,
    api_key       TEXT,
    extra_headers TEXT NOT NULL DEFAULT '[]',   -- JSON [[name, value], ...]
    timeout_ms    INTEGER NOT NULL DEFAULT 120000,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE models (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    alias             TEXT NOT NULL UNIQUE,
    upstream_id       INTEGER NOT NULL REFERENCES upstreams(id) ON DELETE CASCADE,
    upstream_model_id TEXT NOT NULL,
    param_overrides   TEXT NOT NULL DEFAULT '{}', -- JSON Params
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE local_models (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    swap_model_id TEXT NOT NULL UNIQUE,
    gguf_path     TEXT NOT NULL,
    args          TEXT NOT NULL DEFAULT '[]',  -- JSON [string]
    ttl_seconds   INTEGER NOT NULL DEFAULT 300,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE api_keys (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    key_hash   TEXT NOT NULL,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE request_logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                TEXT NOT NULL DEFAULT (datetime('now')),
    client_key        TEXT,
    ingress_proto     TEXT NOT NULL,
    requested_alias   TEXT NOT NULL,
    upstream_id       INTEGER,
    upstream_name     TEXT,
    upstream_model    TEXT,
    egress_proto      TEXT,
    status            INTEGER NOT NULL,
    ttfb_ms           INTEGER,
    total_ms          INTEGER,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    streamed          INTEGER NOT NULL DEFAULT 0,
    error_kind        TEXT,
    error_msg         TEXT
);

CREATE INDEX idx_request_logs_ts ON request_logs (ts DESC);
CREATE INDEX idx_request_logs_alias ON request_logs (requested_alias);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
