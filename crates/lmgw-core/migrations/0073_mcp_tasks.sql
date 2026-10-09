-- MCP Tasks for hosted servers (MCP Tasks design §2.1): one row per task
-- lmgw started with a task-augmented `tools/call` for a stored Chat thread
-- (the late path). A bridged call (no thread) stores nothing.
--
-- `state`: 'open' rows are followed (`tasks/get`, status notifications);
-- 'ended' rows hold their result until it is delivered into the thread, which
-- deletes them; 'cancel_owed' rows wait for their server's next connection to
-- send `tasks/cancel`. A 'cancel_owed' row whose `result` is still set and
-- whose `thread_id` is too also waits for delivery (a cancel made while the
-- server was not connected ends the task at once and owes the cancel).
-- Nothing else removes a row (design T9).
--
-- `server_id` is a soft link, as every `key_id` column is: the removal of a
-- server row ends its tasks in the removal's own transaction first.
--
-- `(server_id, task_id)` is unique among the rows not yet `ended`: receivers
-- make task ids unique among their own live tasks only, and may reuse one. A
-- new task whose id an older live row holds ends that row first (the server
-- reused its id), while ended rows of the id wait for delivery beside it.
CREATE TABLE mcp_tasks (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id        INTEGER NOT NULL,
  server_label     TEXT NOT NULL,
  task_id          TEXT NOT NULL,
  thread_id        INTEGER NULL REFERENCES chat_threads(id) ON DELETE SET NULL,
  tool             TEXT NOT NULL,
  call_id          TEXT NOT NULL,
  started_by       TEXT NULL,
  state            TEXT NOT NULL,
  status           TEXT NOT NULL,
  status_message   TEXT NULL,
  poll_interval_ms INTEGER NULL,
  ttl_ms           INTEGER NULL,
  ended_by         TEXT NULL,
  result           TEXT NULL,
  created_at       TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX mcp_tasks_live ON mcp_tasks(server_id, task_id) WHERE state <> 'ended';
CREATE INDEX mcp_tasks_thread ON mcp_tasks(thread_id);
CREATE INDEX mcp_tasks_state ON mcp_tasks(state);
