-- The tools each MCP server listed when it was last connected (client-apps
-- design §7.5; the owner, 2026-10-09): two servers that offer a tool of one
-- name both expose it under their server's prefix, and which tools collide
-- is read from every enabled server's tools — a server not connected now
-- (lazy, idle-reaped, a sleeping agent, an offline device) by what it last
-- listed. Kept across restarts so an exposed name does not change with the
-- order servers connect in. The upstream's own names; the owner's renames
-- and the prefix apply when the aggregate is built. Replaced on every
-- listing that differs, gone with the server.
CREATE TABLE mcp_known_tools (
    server_id INTEGER NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    PRIMARY KEY (server_id, tool_name)
);
