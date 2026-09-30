-- audio.cpp backend: a third managed container running `audiocpp_server`
-- (OpenAI-shaped /v1/audio/* endpoints). Model entries render `server.json`
-- (one entry per enabled model); exposure + aliasing go through a managed
-- upstream row (kind=audio_cpp, expose_all) like the embed router, so no
-- `public` flag is needed here either.
CREATE TABLE audio_models (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    model_id        TEXT NOT NULL UNIQUE,      -- server.json id / client-facing id
    family          TEXT NOT NULL,             -- audio.cpp loader family (qwen3_tts, …)
    path            TEXT NOT NULL,             -- model dir relative to the audio models dir
    task            TEXT NOT NULL DEFAULT 'tts', -- vad|asr|diar|sep|gen|tts|clon|vc|s2s|align|vdes|spk|svc
    mode            TEXT NOT NULL DEFAULT 'offline', -- offline|streaming
    load_options    TEXT NOT NULL DEFAULT '{}',  -- JSON object — model-load options
    session_options TEXT NOT NULL DEFAULT '{}',  -- JSON object — session/runtime options
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
-- no-transaction
-- Widen `upstreams.kind` for the managed audio.cpp upstream row
-- (kind=audio_cpp). Rebuilt rather than ALTER'd because SQLite can't change
-- a table CHECK constraint in place.

PRAGMA foreign_keys = OFF;

CREATE TABLE upstreams_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    protocol      TEXT NOT NULL CHECK (protocol IN ('openai','anthropic','gemini')),
    kind          TEXT NOT NULL DEFAULT 'generic' CHECK (kind IN ('generic','llama_server','audio_cpp')),
    base_url      TEXT NOT NULL,
    api_key       TEXT,
    extra_headers TEXT NOT NULL DEFAULT '[]',   -- JSON [[name, value], ...]
    timeout_ms    INTEGER NOT NULL DEFAULT 120000,
    enabled       INTEGER NOT NULL DEFAULT 1,
    expose_all    INTEGER NOT NULL DEFAULT 0,
    expose_prefix TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO upstreams_new
SELECT id, name, protocol, kind, base_url, api_key, extra_headers, timeout_ms,
       enabled, expose_all, expose_prefix, created_at, updated_at
FROM upstreams;

DROP TABLE upstreams;
ALTER TABLE upstreams_new RENAME TO upstreams;

PRAGMA foreign_keys = ON;
-- no-transaction
-- HF downloads now serve a third models dir (the audio.cpp container's).
-- Widen the target CHECK to include 'audio'. Rebuilt rather than ALTER'd
-- because SQLite can't change a table CHECK constraint in place.

PRAGMA foreign_keys = OFF;

CREATE TABLE hf_models_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT NOT NULL,
    file          TEXT NOT NULL,
    dest_path     TEXT NOT NULL,
    target        TEXT NOT NULL DEFAULT 'chat' CHECK (target IN ('chat','embed','audio')),
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
SELECT id, repo, file, dest_path, target, etag, size_bytes, status, error, downloaded_at, created_at
FROM hf_models;

DROP TABLE hf_models;
ALTER TABLE hf_models_new RENAME TO hf_models;

PRAGMA foreign_keys = ON;
