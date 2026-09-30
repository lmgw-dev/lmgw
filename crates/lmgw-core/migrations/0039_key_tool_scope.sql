-- Per-key MCP tool scope (key tool scope design).
--
-- A client key's alias scope said which models it could ask for; nothing said
-- which tools it could see. Every key on `/mcp` got the whole aggregate, and
-- every `/v1/responses` caller could attach any registered server. The same
-- three modes and the same glob list, over exposed tool names this time
-- (`github__*`, `docs__query`). 'all' is today's behaviour, so an existing key
-- sees exactly what it saw before.
ALTER TABLE api_keys ADD COLUMN tool_scope_mode TEXT NOT NULL DEFAULT 'all'
     CHECK (tool_scope_mode IN ('all','allow','deny'));
ALTER TABLE api_keys ADD COLUMN tool_scope_patterns TEXT NOT NULL DEFAULT '';
