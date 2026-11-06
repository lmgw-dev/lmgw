-- One fallback per model and per alias, for every reason it cannot answer
-- (openai egress design §14.10, decisions 25 and 26).
--
-- Every table whose rows a client can name gets one nullable `fallback`:
-- NULL is "no fallback". The two-column split of 0024 and 0044 (a mode next
-- to a name) existed for a third state, `inherit`, which read the global
-- `settings.hold.fallback_alias`; the global and `inherit` go, so the name
-- alone says everything. Each row keeps exactly the fallback it answered
-- with before:
--   - mode `alias` with a name that is not blank: that name, untrimmed, as
--     the loader handed it out (`hold_fallback_for` filtered blanks only);
--   - mode `alias` with a blank name, or mode `none`: none (a stale name in
--     the column was never read and is dropped);
--   - mode `inherit`, or one this build does not know (the loader read an
--     unknown mode as `inherit`), on a chat row or a candidate alias: the
--     global as the
--     loader read it, or none when it was unset or blank;
--   - mode `inherit` on an aux, audio or image row: none, as before (those
--     classes never inherited the global).
-- The guard in `store/migration_guards/one_fallback.rs` runs first: it
-- refuses the start for the one global the SQL below cannot read the way the
-- loader read it, and names every row whose fallback changes source.

-- The global as the loader reads it: NULL when unset, blank or not JSON (a
-- blob that is not JSON is a default `Settings` to the loader).
-- local_models (chat): `inherit` read the global.
ALTER TABLE local_models ADD COLUMN fallback TEXT;
UPDATE local_models SET fallback = CASE hold_fallback_mode
  WHEN 'alias' THEN CASE WHEN TRIM(hold_fallback) <> '' THEN hold_fallback END
  WHEN 'none' THEN NULL
  ELSE (SELECT CASE WHEN TRIM(g) <> '' THEN g END FROM (
      SELECT CASE WHEN json_valid(value) THEN json_extract(value, '$.hold.fallback_alias') END AS g
      FROM settings WHERE key = 'settings'))
  END;
ALTER TABLE local_models DROP COLUMN hold_fallback;
ALTER TABLE local_models DROP COLUMN hold_fallback_mode;

-- aux_models, audio_models, image_models: `inherit` meant none.
ALTER TABLE aux_models ADD COLUMN fallback TEXT;
UPDATE aux_models SET fallback = CASE WHEN hold_fallback_mode = 'alias'
  AND TRIM(hold_fallback) <> '' THEN hold_fallback END;
ALTER TABLE aux_models DROP COLUMN hold_fallback;
ALTER TABLE aux_models DROP COLUMN hold_fallback_mode;

ALTER TABLE audio_models ADD COLUMN fallback TEXT;
UPDATE audio_models SET fallback = CASE WHEN hold_fallback_mode = 'alias'
  AND TRIM(hold_fallback) <> '' THEN hold_fallback END;
ALTER TABLE audio_models DROP COLUMN hold_fallback;
ALTER TABLE audio_models DROP COLUMN hold_fallback_mode;

ALTER TABLE image_models ADD COLUMN fallback TEXT;
UPDATE image_models SET fallback = CASE WHEN hold_fallback_mode = 'alias'
  AND TRIM(hold_fallback) <> '' THEN hold_fallback END;
ALTER TABLE image_models DROP COLUMN hold_fallback;
ALTER TABLE image_models DROP COLUMN hold_fallback_mode;

-- candidate_aliases: `inherit` read the global unconditionally; `fallback`
-- exists already.
UPDATE candidate_aliases SET fallback = CASE fallback_mode
  WHEN 'alias' THEN CASE WHEN TRIM(fallback) <> '' THEN fallback END
  WHEN 'none' THEN NULL
  ELSE (SELECT CASE WHEN TRIM(g) <> '' THEN g END FROM (
      SELECT CASE WHEN json_valid(value) THEN json_extract(value, '$.hold.fallback_alias') END AS g
      FROM settings WHERE key = 'settings'))
  END;
ALTER TABLE candidate_aliases DROP COLUMN fallback_mode;

-- Plain aliases had none.
ALTER TABLE models ADD COLUMN fallback TEXT;

-- The global goes, after it was copied.
UPDATE settings SET value = json_remove(value, '$.hold.fallback_alias')
  WHERE key = 'settings' AND json_valid(value);

-- The first start types the fallbacks that name a cloud model, which this
-- migration cannot (§14.10): it needs the upstream catalogs.
INSERT INTO settings (key, value) VALUES ('fallback_type_check', 'pending');
