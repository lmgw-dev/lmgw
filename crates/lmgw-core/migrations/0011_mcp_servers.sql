-- MCP gateway, config plane. lmgw aggregates N registered MCP servers behind its
-- single northbound `/mcp` endpoint — the tool-plane analogue of `upstreams` for
-- the model plane. This table holds the *definitions* only; live rmcp peer
-- connections (long-lived stdio pipes, HTTP session ids, cached tool lists) live
-- in `AppState.mcp::McpManager`, reconciled on each snapshot reload — the same
-- config-vs-live split as `upstreams`/`RouterManager`.
CREATE TABLE mcp_servers (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    enabled         INTEGER NOT NULL DEFAULT 1,
    transport       TEXT NOT NULL,               -- 'stdio' | 'http' | 'sse' (config::McpTransport)
    -- stdio transport:
    command         TEXT,                         -- bare entrypoint; ignored when container_image is set (argv synthesized)
    args            TEXT NOT NULL DEFAULT '[]',   -- JSON [string]
    env             TEXT NOT NULL DEFAULT '[]',   -- JSON [[name,value]] (secrets; 0600 DB) — array, matching Vec<(String,String)>
    cwd             TEXT,
    container_image TEXT,                         -- non-NULL ⇒ Podman-isolated; `podman run -i` argv synthesized
    extra_run_args  TEXT NOT NULL DEFAULT '[]',   -- JSON [string]; GPU/CDI/:Z flags, like RouterSettings.extra_run_args
    -- http/sse transport:
    url             TEXT,
    headers         TEXT NOT NULL DEFAULT '[]',   -- JSON [[name,value]] (auth tokens; 0600 DB)
    -- common:
    tool_prefix     TEXT NOT NULL DEFAULT '',     -- '' = bare; else tools exposed as `<prefix>__<tool>`
    timeout_ms      INTEGER NOT NULL DEFAULT 60000,
    autostart       INTEGER NOT NULL DEFAULT 1,   -- 0 = lazy-connect on first use
    idle_seconds    INTEGER NOT NULL DEFAULT 0,   -- reap an idle connection after N s (0 = never)
    allow_sampling  INTEGER NOT NULL DEFAULT 1,   -- declare the sampling capability to this server (reconnect-affecting)
    sampling_alias  TEXT,                          -- per-server model override (NULL ⇒ global Settings.sampling_alias)
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Per-server tool overrides: hide a discovered tool from the aggregate catalog,
-- or rename its exposed name. The tool-plane analogue of `hidden_passthrough_models`.
-- `tool_name` is the upstream (un-prefixed) name as the server reports it.
CREATE TABLE mcp_tool_overrides (
    server_id   INTEGER NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name   TEXT NOT NULL,
    hidden      INTEGER NOT NULL DEFAULT 0,
    rename      TEXT,                              -- optional exposed-name override
    PRIMARY KEY (server_id, tool_name)
);
