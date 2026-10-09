-- MCP approvals in the Chat (client-apps design §6, L13).
--
-- A gated turn saves its reply with the calls it stopped on and the key of
-- the principal that started it, as JSON (`store::PendingApprovals`): the
-- held calls of its last stop, and every decision made on the reply — who
-- decided, approve or decline, and whether the user moved on without
-- deciding. NULL on every reply that never stopped for an approval.
ALTER TABLE chat_messages ADD COLUMN pending_approvals TEXT;

-- Who approved the call a tool-call row records (§6.3), as `<kind>:<name>`
-- (`owner:dashboard`, `device:phone`). NULL for a call no approval decided.
ALTER TABLE request_logs ADD COLUMN approved_by TEXT;
