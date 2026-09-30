-- The agent catalog (agent-catalog design §3).
--
-- An agent is a manifest: one JSON document naming a model, prompts, the MCP
-- tools it may reach, a config form and a run shape. Adding one never rebuilds
-- lmgw, which is the whole point — `web/workflows.rs` is 1.7k lines that fuse
-- IMAP, a hand-rolled model call and page-specific DTOs into one workflow that
-- cannot be copied, exported or joined by a second one.
--
-- Deliberately **not** on the `Snapshot`: nothing on the request hot path reads
-- an agent, so the API and the executor read the row when asked. Putting it on
-- the snapshot would make every manifest edit a config reload for no reader.
CREATE TABLE agents (
    id         TEXT PRIMARY KEY,
    -- The validated manifest, re-serialized canonically. `schema_version` is
    -- inside it, so a row written by a newer build is recognizable as such by
    -- an older one rather than crashing it.
    manifest   TEXT NOT NULL,
    -- Stored config values, secrets included — the same JSON column convention
    -- upstream keys and MCP env already use. Never exported (§5).
    config     TEXT NOT NULL DEFAULT '{}',
    enabled    INTEGER NOT NULL DEFAULT 1,
    -- `builtin` survives an edit: "Reset to shipped" needs to know the row has
    -- an embedded original to go back to.
    source     TEXT NOT NULL DEFAULT 'authored'
               CHECK (source IN ('builtin','imported','authored')),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- A `chat` agent materializes an ordinary Chat thread (§2.5); this is the only
-- thing that distinguishes it from one opened by hand, and it is what lets the
-- agent's Runs tab list its threads. NULL for every thread opened from the Chat
-- page, which is every thread that exists today.
ALTER TABLE chat_threads ADD COLUMN agent_id TEXT;

-- The internal identity the mail workflow logged under becomes the one every
-- agent run logs under (usage-analytics §4.4). Same row — renamed, not replaced
-- — so the spend history it has accumulated stays attached. `telemetry::
-- internal_identity` maps the surviving `"workflow"` ingress label onto the new
-- name until WP4 deletes the workflow and its label with it.
UPDATE api_keys
   SET name = 'internal:agents',
       note = 'Agent runs: model turns and tool calls'
 WHERE name = 'internal:workflow-mail';
