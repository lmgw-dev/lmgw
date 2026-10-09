-- Personality profiles (personality-profiles design §1.2): how a Chat
-- thread's model talks — a persona, a length rule, static examples, a voice
-- block, a reasoning switch and the speech voice — kept in one row each and
-- picked by a thread with `chat_threads.profile_id`.
--
-- `body`: the content fields as one JSON object
-- (`config::chat_profile::ProfileBody`), strict on input and read
-- tolerantly: a key this build cannot read is dropped on its own. An absent
-- key is unset on the owner's own rows, and the built-in text on a built-in
-- one, so a built-in follows its improvements; an explicit null is unset on
-- either.
--
-- `builtin`: the built-in profile a row is (`concise`), NULL for the owner's
-- own. Its texts are not stored (`store::chat_profiles::builtin`).
--
-- AUTOINCREMENT: the Chat feed and clients name profiles by id, so a deleted
-- profile's id is never handed to a new one.
CREATE TABLE chat_profiles (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Unique in any case (also checked with Unicode case folding on write);
    -- never `default`, which is no profile.
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    builtin    TEXT UNIQUE,
    body       TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- A thread's profile; NULL is none ("Default"), which every existing thread
-- keeps. Deleting a profile sets its threads back to none.
ALTER TABLE chat_threads ADD COLUMN profile_id INTEGER
    REFERENCES chat_profiles(id) ON DELETE SET NULL;
CREATE INDEX idx_chat_threads_profile ON chat_threads(profile_id);

-- The one built-in profile, seeded once, here: a deleted built-in is never
-- seeded again (it can be created again on request).
INSERT INTO chat_profiles (name, builtin, body) VALUES ('Concise', 'concise', '{}');
