-- The audit log (headless server design §5.8, ruling 18, WP7): who did
-- what, written wherever accounts are on. Attribution among admins, not
-- tamper evidence: any account can change the log's retention.
--
-- `account_id` is a soft link, as every key_id column is: a deleted
-- account's rows stay, and `principal` carries its login as it was then.
CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         TEXT NOT NULL,            -- RFC 3339 UTC
    account_id INTEGER,                  -- NULL: no account (the offline command line, an unknown name)
    principal  TEXT NOT NULL,            -- "sam, session #12", "owner, key owner:dashboard"
    via        TEXT NOT NULL,            -- dashboard | api | mcp | chat | agent | cli | cli-offline | tray | app
    peer       TEXT NOT NULL,            -- the client address, "local" for the offline command line
    action     TEXT NOT NULL,            -- an op's or tool's name, a route, a sign-in event
    target     TEXT NOT NULL DEFAULT '{}', -- JSON: the row it names, a settings save's fields; secrets redacted
    outcome    INTEGER NOT NULL,         -- the answer's HTTP status
    -- A sign-in event no credential stands behind, repeated by the same
    -- peer within a minute of the row's `ts`, adds to the row (D91): how
    -- many times, and the last time.
    count      INTEGER NOT NULL DEFAULT 1,
    last_ts    TEXT                      -- RFC 3339 UTC, NULL while count is 1
);
CREATE INDEX audit_log_ts ON audit_log (ts);
-- The Activity list's account filter (§5.8), newest first.
CREATE INDEX audit_log_account ON audit_log (account_id, id);
