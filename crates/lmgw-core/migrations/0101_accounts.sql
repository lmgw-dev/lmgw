-- Accounts and sessions (headless server design §5.1, WP6): schema only.
--
-- Account 1 is not made here: on a fresh database `api_keys` is still empty
-- when this runs, so the account could not name its door key. The owner-key
-- seed (`agents::token::seed_owner_keys`) makes it after the migrations, on
-- every start, and attaches every owner row without an account to it.
--
-- `AUTOINCREMENT` on both ids: an id is never handed out twice, so a deleted
-- account's id (and its door key's name `owner:account-<id>`, a session id
-- the card or a principal still names) never comes back as someone else's.
CREATE TABLE accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    login               TEXT NOT NULL UNIQUE,  -- [a-z0-9._-], stored in lowercase (§5.2)
    display_name        TEXT NOT NULL DEFAULT '',
    password_hash       TEXT,                  -- PHC string (argon2id); NULL = no password yet
    enabled             INTEGER NOT NULL DEFAULT 1,
    door_key_id         INTEGER NOT NULL,      -- api_keys.id of its door key
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    created_by          INTEGER,               -- accounts.id, soft link
    password_changed_at TEXT,
    last_login_at       TEXT,
    failed_logins       INTEGER NOT NULL DEFAULT 0   -- since the last good sign-in
);

-- Set-password links (§5.5): one per account, a second replaces the first.
CREATE TABLE account_tokens (
    token_hash  TEXT PRIMARY KEY,         -- hex(sha256(token))
    account_id  INTEGER NOT NULL UNIQUE,
    expires_at  TEXT NOT NULL             -- RFC 3339 UTC
);

-- One row per sign-in (§5.2). An id of its own beside the token's hash: the
-- Accounts card ends a session by it (`session_end { id }`), and the
-- principal carries it, so neither ever holds the hash.
CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash   TEXT NOT NULL UNIQUE,    -- hex(sha256(token))
    account_id   INTEGER NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL,
    expires_at   TEXT NOT NULL,           -- RFC 3339 UTC
    peer         TEXT NOT NULL,           -- the client address at sign-in
    user_agent   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX sessions_account ON sessions (account_id);

-- Browsers that signed in to an account before (§5.4).
CREATE TABLE known_browsers (
    token_hash  TEXT PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    expires_at  TEXT NOT NULL             -- RFC 3339 UTC
);

ALTER TABLE api_keys ADD COLUMN account_id INTEGER;     -- soft link, as every key_id column is
ALTER TABLE api_keys ADD COLUMN made_by_login TEXT;     -- the creator's login, copied at creation
