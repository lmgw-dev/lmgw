-- Knowledge bases in Chat threads (chat-complete design §9.3).
--
-- A thread's own selection: `kb_ids` (JSON array of knowledge-base ids in
-- `knowledge.db`, '[]' = none), how they reach the model (`kb_mode`: 'auto'
-- retrieves before every turn, 'tool' attaches the kb__* tools instead), and
-- its retrieval budget (`kb_budget_tokens`, NULL = the owner's
-- `chat_kb_budget_tokens` setting). The ids are not foreign keys: the bases
-- live in their own database file, and a base deleted later is reported by
-- the retrieval, not cascaded into the chats.
ALTER TABLE chat_threads ADD COLUMN kb_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE chat_threads ADD COLUMN kb_mode TEXT NOT NULL DEFAULT 'auto';
ALTER TABLE chat_threads ADD COLUMN kb_budget_tokens INTEGER;

-- Per user message: the bases picked with `#` for that message only
-- (`kb_refs`, JSON array), and the retrieval that ran for it (`context`, JSON:
-- the excerpts with their citations, tokens, notes; NULL = none ran). The
-- context is replayed unchanged on every later turn.
ALTER TABLE chat_messages ADD COLUMN kb_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE chat_messages ADD COLUMN context TEXT;
