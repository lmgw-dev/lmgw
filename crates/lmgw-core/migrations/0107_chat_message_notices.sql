-- A reply's notices: the MCP labels its thread or folder attached that
-- offered the turn no tools (an unreachable server, an offline device, a
-- switched-off toolset), as the turn's `error` frames said them while it
-- streamed. They are the reply's, kept with it, so a later read of the
-- thread still says them beside it. A JSON array of
-- {server_label, code, message}; NULL for a reply whose turn had none, for
-- every other role, and for every row from before this migration.
ALTER TABLE chat_messages ADD COLUMN notices TEXT;
