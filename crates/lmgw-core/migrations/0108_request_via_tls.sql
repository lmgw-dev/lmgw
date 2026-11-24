-- Which listener a request came in on gains the main TLS listener (headless
-- server design §4.3): `Via::Tls` is written as `tls`, and 0081's CHECK knew
-- `plain` and `devices` only, so a request over the TLS listener got no
-- request_logs row at all ("CHECK constraint failed: via IN ('plain',
-- 'devices')", seen 2026-11-13 on a server image behind TLS, where every
-- request from another machine comes in that way). SQLite cannot change a
-- CHECK in place: the column is made anew with the wider one, filled from the
-- old, and takes its name. Nothing indexes it, and no trigger or view names it.
ALTER TABLE request_logs ADD COLUMN via_listener TEXT NOT NULL DEFAULT 'plain'
    CHECK (via_listener IN ('plain', 'tls', 'devices'));
UPDATE request_logs SET via_listener = via WHERE via <> 'plain';
ALTER TABLE request_logs DROP COLUMN via;
ALTER TABLE request_logs RENAME COLUMN via_listener TO via;
