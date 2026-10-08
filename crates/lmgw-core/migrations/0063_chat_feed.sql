-- The Chat change feed (client-apps design §2.3, L6): change records, not
-- renderings. A record says what changed and who changed it; delivery
-- renders the thread or folder as it is then (`web::chat_feed`), so a
-- catch-up never replays a stale copy.
--
-- `seq` is AUTOINCREMENT: a number is never handed out twice, not after the
-- newest rows were pruned either, so a cursor names one record for good.
-- `admin` is the thread's kind at the write (a kind never changes), kept so
-- a device's feed leaves an Admin Chat thread out once the thread is gone
-- too (L3). `detail` holds an event's facts that are not state, such as
-- `folder.current`'s previous thread and its reason.
CREATE TABLE chat_feed (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    at          TEXT    NOT NULL DEFAULT (datetime('now')),
    type        TEXT    NOT NULL,
    thread_id   INTEGER,
    folder_id   INTEGER,
    message_ids TEXT,
    by          TEXT,
    admin       INTEGER NOT NULL DEFAULT 0,
    detail      TEXT
);
CREATE INDEX chat_feed_at ON chat_feed(at);

-- One row: this database's epoch, minted once, and how far retention has
-- pruned. A cursor is "<epoch>:<seq>". One minted by another database (a
-- fresh data dir, another install's backup) names another epoch; one at or
-- past `pruned_through` still has every record after it.
CREATE TABLE chat_feed_meta (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    epoch          TEXT    NOT NULL,
    pruned_through INTEGER NOT NULL DEFAULT 0
);
INSERT INTO chat_feed_meta (id, epoch) VALUES (1, lower(hex(randomblob(16))));
