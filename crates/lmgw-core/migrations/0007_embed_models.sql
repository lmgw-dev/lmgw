-- Embedding models served by a *second* llama-server router container (the
-- embed router). Slimmer than `local_models`: embedding models don't need the
-- chat sampling/jinja/spec-decode params. Exposure + aliasing go through a
-- managed upstream row (kind=llama_server, expose_all) pointing at the embed
-- container's port, so no `public` flag or synthetic routing is needed here —
-- an enabled model is in the preset, and the preset is the upstream's catalog.
CREATE TABLE embed_models (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    model_id     TEXT NOT NULL UNIQUE,        -- preset section / client-facing id
    gguf_path    TEXT NOT NULL,               -- relative to the embed models dir
    pooling      TEXT,                         -- --pooling (none|mean|cls|last|rank)
    ctx_size     INTEGER,                      -- --ctx-size (NULL = llama-server default)
    args         TEXT NOT NULL DEFAULT '[]',   -- JSON [string] — freeform extra flags
    idle_seconds INTEGER NOT NULL DEFAULT 0,   -- --sleep-idle-seconds (0 = never)
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
