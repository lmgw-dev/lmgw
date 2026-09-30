-- Chat search (chat-complete design §4): one FTS5 table over thread titles
-- (kind 't'), message content ('m') and the names of *sent* attachments ('a').
--
-- The table is a plain (not external-content) one, and its rowid is derived
-- from the source row — `id * 4 + {0: thread, 1: message, 2: attachment}` —
-- so a trigger removes a row by rowid instead of scanning the UNINDEXED
-- columns. `ref_id` is the source row's id, `thread_id` its thread (for a
-- title, the thread itself). Foreign-key cascades fire the child tables'
-- DELETE triggers, so deleting a thread takes its messages' and attachments'
-- rows with it.
CREATE VIRTUAL TABLE chat_fts USING fts5(
    body,
    kind UNINDEXED,
    ref_id UNINDEXED,
    thread_id UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);

INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    SELECT id * 4, title, 't', id, id FROM chat_threads;
INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    SELECT id * 4 + 1, content, 'm', id, thread_id FROM chat_messages;
INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    SELECT id * 4 + 2, name, 'a', id, thread_id FROM chat_attachments WHERE message_id IS NOT NULL;

CREATE TRIGGER chat_fts_thread_ai AFTER INSERT ON chat_threads BEGIN
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    VALUES (new.id * 4, new.title, 't', new.id, new.id);
END;
CREATE TRIGGER chat_fts_thread_au AFTER UPDATE OF title ON chat_threads BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4;
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    VALUES (new.id * 4, new.title, 't', new.id, new.id);
END;
CREATE TRIGGER chat_fts_thread_ad AFTER DELETE ON chat_threads BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4;
END;

CREATE TRIGGER chat_fts_msg_ai AFTER INSERT ON chat_messages BEGIN
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    VALUES (new.id * 4 + 1, new.content, 'm', new.id, new.thread_id);
END;
CREATE TRIGGER chat_fts_msg_au AFTER UPDATE OF content ON chat_messages BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4 + 1;
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    VALUES (new.id * 4 + 1, new.content, 'm', new.id, new.thread_id);
END;
CREATE TRIGGER chat_fts_msg_ad AFTER DELETE ON chat_messages BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4 + 1;
END;

-- A draft attachment (message_id NULL) is not indexed; sending binds it, and
-- that UPDATE of message_id is what brings its name in.
CREATE TRIGGER chat_fts_att_ai AFTER INSERT ON chat_attachments WHEN new.message_id IS NOT NULL BEGIN
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    VALUES (new.id * 4 + 2, new.name, 'a', new.id, new.thread_id);
END;
CREATE TRIGGER chat_fts_att_au AFTER UPDATE OF name, message_id ON chat_attachments BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4 + 2;
    INSERT INTO chat_fts (rowid, body, kind, ref_id, thread_id)
    SELECT new.id * 4 + 2, new.name, 'a', new.id, new.thread_id WHERE new.message_id IS NOT NULL;
END;
CREATE TRIGGER chat_fts_att_ad AFTER DELETE ON chat_attachments BEGIN
    DELETE FROM chat_fts WHERE rowid = old.id * 4 + 2;
END;
