-- Pairing over the device link (device link design §3.6, §12): a device
-- authenticates by its client certificate's key, enrolled with a one-time
-- pairing token and confirmed by the owner, no longer by a bearer key.
--
-- A device row's `key_hash` names its credential by a prefix: `spki:<hex
-- SHA-256 of its SubjectPublicKeyInfo>` once enrolled, `pending:<random>`
-- before, `v1:<old hash>` for a row paired with a link from before TLS. A
-- bearer's hash is 64 hex digits without a prefix, so no bearer matches a
-- device row any more: every existing device row is marked `v1:` here, keeps
-- its id, name, policy, hosting label, admin level and history, and pairs
-- again with a new token.
--
-- The columns, on device rows only (NULL, `[]` or 0 on every other kind):
-- - `urls`: the device's lmgw URLs, a JSON list of `https` base URLs in the
--   owner's order (§3.1);
-- - `cert_serial`, `cert_not_after`, `cert_notice_after`: the latest
--   certificate issued (RFC 3339 UTC);
-- - `enrolled_at`, `enrolled_from`, `client_info`: the last enrolment's time,
--   peer address and `client` text;
-- - `confirmed_at`: the owner's confirmation (§3.5); NULL refuses the device;
-- - `contested_at`, `contested_from`: a used token presented again by another
--   key (§3.3);
-- - `local_only`: paired as "this computer" (§4.1);
-- - `waiting_key_hash`, `waiting_cert_serial`, `waiting_cert_not_after`,
--   `waiting_cert_notice_after`: a new key enrolled while the row kept its
--   confirmed one (a re-pair that kept the current key, §5.2), with its
--   certificate; it waits for the owner's Confirm, which makes it the row's
--   key and ends the kept one (§3.5).
ALTER TABLE api_keys ADD COLUMN urls TEXT NOT NULL DEFAULT '[]';
ALTER TABLE api_keys ADD COLUMN cert_serial TEXT;
ALTER TABLE api_keys ADD COLUMN cert_not_after TEXT;
ALTER TABLE api_keys ADD COLUMN cert_notice_after TEXT;
ALTER TABLE api_keys ADD COLUMN enrolled_at TEXT;
ALTER TABLE api_keys ADD COLUMN enrolled_from TEXT;
ALTER TABLE api_keys ADD COLUMN client_info TEXT;
ALTER TABLE api_keys ADD COLUMN confirmed_at TEXT;
ALTER TABLE api_keys ADD COLUMN contested_at TEXT;
ALTER TABLE api_keys ADD COLUMN contested_from TEXT;
ALTER TABLE api_keys ADD COLUMN local_only INTEGER NOT NULL DEFAULT 0
    CHECK (local_only IN (0, 1));
ALTER TABLE api_keys ADD COLUMN waiting_key_hash TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_cert_serial TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_cert_not_after TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_cert_notice_after TEXT;

UPDATE api_keys SET key_hash = 'v1:' || key_hash WHERE kind = 'device';

-- One device per key: the principal on the device listener is the row whose
-- `key_hash` is the certificate key's, so two rows must never hold one.
CREATE UNIQUE INDEX api_keys_device_spki ON api_keys(key_hash)
    WHERE kind = 'device' AND key_hash LIKE 'spki:%';
-- And a waiting key on one row only; the enrolment's transaction refuses a
-- key that is another row's `key_hash` or waiting key.
CREATE UNIQUE INDEX api_keys_device_waiting_spki ON api_keys(waiting_key_hash)
    WHERE kind = 'device' AND waiting_key_hash IS NOT NULL;

-- One pairing token per device row (§3.3), stored as its SHA-256 only. A new
-- token replaces the row's. `used_*` say who used it: the enrolling key's
-- `spki:` hash, the peer address, and the certificate the enrolment answered
-- (standard base64 DER), so the same key presenting the token again within
-- its life gets the same answer. `ended_at` is the owner ending it (the QR
-- card's close, a new token, the CA's rotation). A soft link, as every
-- `key_id` column is (0061's note): the key ops delete the row in the same
-- transaction as the device.
CREATE TABLE device_pairing_tokens (
    key_id      INTEGER PRIMARY KEY,
    token_hash  TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    used_at     TEXT,
    used_from   TEXT,
    used_by     TEXT,
    certificate TEXT,
    ended_at    TEXT
);

-- Which listener a request came in on (device link design §4.5): `plain`
-- (`bind_addr`) or `devices` (the device listener). Every row before this
-- file came in on the plain listener.
ALTER TABLE request_logs ADD COLUMN via TEXT NOT NULL DEFAULT 'plain'
    CHECK (via IN ('plain', 'devices'));
