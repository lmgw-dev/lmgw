-- The device's own name in its enrolment (device link design §3.4,
-- *2026-10-23*): free text, as its user chose it on the device ("Anna's
-- Fold"), shown on the Devices card beside the owner's name for the link.
-- Written with `client_info` in the same enrolment, so it is that column's
-- twin. Never trusted for anything.
--
-- The enrolment's self-description belongs to the key it came with: the
-- `waiting_` twins of `enrolled_at`, `enrolled_from`, `client_info` and
-- `device_name` hold a key waiting beside a confirmed one (device link
-- design §5.2), as `waiting_app_id` does (0088). The owner's Confirm moves
-- them onto the main columns; a new token or a cleared key drops them. So a
-- re-pair nobody confirms leaves the confirmed device's line alone.
--
-- v0.5.0 wrote a waiting key's time, address and client onto the main columns
-- (they described the newcomer, though the row kept its working key). A row
-- with a key waiting at the upgrade moves those three to the twins and clears
-- them: they never described the kept key, and Confirm would otherwise move
-- empty twins over the newcomer's values.
ALTER TABLE api_keys ADD COLUMN device_name TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_enrolled_at TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_enrolled_from TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_client_info TEXT;
ALTER TABLE api_keys ADD COLUMN waiting_device_name TEXT;

UPDATE api_keys
   SET waiting_enrolled_at = enrolled_at,
       waiting_enrolled_from = enrolled_from,
       waiting_client_info = client_info,
       enrolled_at = NULL,
       enrolled_from = NULL,
       client_info = NULL
 WHERE kind = 'device' AND waiting_key_hash IS NOT NULL;
