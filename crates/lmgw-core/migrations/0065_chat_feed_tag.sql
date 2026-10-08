-- The check a feed cursor carries (client-apps design §2.1; review W6-3):
-- a random tag per record, drawn when the record is written and kept with
-- it. A cursor is "<epoch>:<seq>:<tag>", and a resume compares its tag with
-- the one stored at its number, so a database restored from an older copy,
-- which writes other records at the same numbers, is still found (W5-4).
--
-- Random, and no function of the record: the hash it replaces was computed
-- from what the record says (when, what, which thread or folder, by whom),
-- and a device handed the cursor of a record it may not see (a keep-alive's
-- `id:`, `hello.cursor`) could match it offline against the threads and
-- folders it guesses. A tag says nothing about its record.
--
-- A column default may not be an expression in `ALTER TABLE ADD COLUMN`,
-- so the tag is drawn by a trigger: every insert gets one, whichever
-- statement wrote it. 16 hex digits (64 bits).
ALTER TABLE chat_feed ADD COLUMN tag TEXT NOT NULL DEFAULT '';
UPDATE chat_feed SET tag = lower(hex(randomblob(8)));
CREATE TRIGGER chat_feed_tag AFTER INSERT ON chat_feed
    WHEN NEW.tag = ''
BEGIN
    UPDATE chat_feed SET tag = lower(hex(randomblob(8))) WHERE seq = NEW.seq;
END;
