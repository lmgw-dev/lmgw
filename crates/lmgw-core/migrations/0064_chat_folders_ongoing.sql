-- Ongoing-conversation folders and a folder's own retention (client-apps
-- design §3; the answer to §11 Q2).
--
-- `ongoing_idle_minutes`: NULL, the folder is not an ongoing conversation;
-- 0, it is one and rolls over to a new thread only when asked; N > 0, it
-- also rolls over once its current thread has had no message for N minutes.
--
-- `current_thread_id`: the thread the conversation continues in. Only an
-- ongoing folder has one, and only a chat thread in the folder, not
-- archived, is one: every write that would break that clears it in its own
-- transaction and records `folder.current` in the change feed. The
-- reference is the backstop for a delete that did not.
--
-- `archive_days` / `purge_days`: the folder's own retention for its
-- threads; NULL is the global setting (`chat_archive_days`,
-- `chat_purge_days`), 0 disables that step for the folder.
ALTER TABLE chat_folders ADD COLUMN ongoing_idle_minutes INTEGER
    CHECK (ongoing_idle_minutes IS NULL OR ongoing_idle_minutes >= 0);
ALTER TABLE chat_folders ADD COLUMN current_thread_id INTEGER
    REFERENCES chat_threads(id) ON DELETE SET NULL;
ALTER TABLE chat_folders ADD COLUMN archive_days INTEGER
    CHECK (archive_days IS NULL OR archive_days >= 0);
ALTER TABLE chat_folders ADD COLUMN purge_days INTEGER
    CHECK (purge_days IS NULL OR purge_days >= 0);
CREATE INDEX idx_chat_folders_current ON chat_folders(current_thread_id);
