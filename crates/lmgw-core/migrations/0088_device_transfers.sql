-- Device-to-device transfers (device transfers design §2, §3, §9): a paired
-- device offers a file to another device of the same app, relayed through
-- lmgw's disk.
--
-- A device row's app id, as its last enrolment declared it (§2): a
-- lowercase reverse-DNS name, or NULL (the device receives no transfer).
-- `waiting_app_id` is the one a key waiting beside a confirmed one declared
-- (device link design §5.2): the owner's Confirm moves it to `app_id` with
-- the key. NULL on every other kind.
ALTER TABLE api_keys ADD COLUMN app_id TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_app_id TEXT;

-- One transfer per row. `id` is 32 random lowercase hex digits, the file's
-- name under `<data_dir>/transfers/`. `from_key_id` and `to_key_id` are the
-- devices' `api_keys.id`, soft links as every `key_id` column is (0061's
-- note): a deleted device's transfers end (§8) and keep its name
-- (`from_name`, `to_name`, without `device:`). `received` is the file's
-- length as of the last upload, and how far an ended one got; while it
-- uploads, the file's length on disk is the resume point (§3). `ready_at`
-- says the receiver was offered it: the receiver sees it, and hears of it,
-- from then on (§1, §6). Times are RFC 3339 UTC to the second, so they
-- compare as text.
CREATE TABLE transfers (
    id          TEXT PRIMARY KEY,
    app_id      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    title       TEXT NOT NULL,
    filename    TEXT NOT NULL,
    size        INTEGER NOT NULL CHECK (size > 0),
    sha256      TEXT NOT NULL,
    from_key_id INTEGER NOT NULL,
    from_name   TEXT NOT NULL,
    to_key_id   INTEGER NOT NULL,
    to_name     TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN
                    ('uploading', 'ready', 'delivered', 'declined', 'canceled', 'expired', 'failed')),
    received    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    ready_at    TEXT,
    ended_at    TEXT,
    end_reason  TEXT
);
CREATE INDEX transfers_from ON transfers(from_key_id, created_at);
CREATE INDEX transfers_to ON transfers(to_key_id, created_at);
CREATE INDEX transfers_active ON transfers(expires_at) WHERE state IN ('uploading', 'ready');
CREATE INDEX transfers_ended ON transfers(ended_at) WHERE ended_at IS NOT NULL;
