-- A user message's origin (chat-plane design §9.10): one sentence its
-- client words about where the user wrote it ("Typed in Kai's sidebar in
-- Firefox, beside the tab …"), sent as `origin` with the send or the edit.
-- It is the message's, not the thread's: the model reads it before that
-- message's text on every turn, so the history stays the same from turn to
-- turn (a system prompt that changed with every tab would not) and each
-- earlier message keeps saying where it came from. NULL for a message whose
-- client sent none, for every other role, and for every row from before
-- this migration.
ALTER TABLE chat_messages ADD COLUMN origin TEXT;
