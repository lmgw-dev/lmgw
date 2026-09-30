-- Per-tool owner kill switch for the northbound tool surface.
--
-- Until now the only tool-level switch was `mcp_servers.enabled`, which is per
-- *server*, and the built-in toolsets (`lmgw__*`, `docs__*`) had none at all —
-- so `docs__query` was served to every agent on /mcp with nothing in the UI
-- that even listed it, let alone turned it off. This table is the owner's
-- switch, keyed by the **fully-qualified exposed name** because that is the
-- only identifier every source shares (a built-in has no server id, and a
-- southbound tool's exposed name is what a caller actually names).
--
-- Presence means **disabled**: an install that has never touched this has an
-- empty table and every tool stays offered, which is what every existing /mcp
-- client and chat thread keeps seeing.
--
-- `source` is the source label recorded at the moment the tool was disabled
-- (`lmgw`, `docs`, or the server's label). It is not a foreign key on purpose:
-- a row must survive its server being deleted or renamed, so the inventory can
-- show it as **stale** and say where it came from instead of silently keeping
-- a switch nobody can see.
CREATE TABLE tool_disabled (
    tool_name   TEXT PRIMARY KEY,
    source      TEXT NOT NULL DEFAULT '',
    disabled_at TEXT NOT NULL DEFAULT (datetime('now'))
);
