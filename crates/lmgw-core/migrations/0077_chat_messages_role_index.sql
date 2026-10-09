-- A bound realtime session's read of its thread's late MCP task results
-- (MCP Tasks design §3.4, `store::mcp_tasks::results_read`): the newest
-- reply of a thread (the last of its 'assistant' rows) and its result rows
-- (role 'tool') past an id, each an index range instead of a walk over the
-- whole thread. It runs at every wake of a bound session's thread, so on
-- every voice turn.
CREATE INDEX idx_chat_messages_thread_role ON chat_messages(thread_id, role, id);
