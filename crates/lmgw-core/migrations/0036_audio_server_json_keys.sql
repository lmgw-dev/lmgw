-- The `server.json` keys audio.cpp grew after lmgw's audio class was written
-- (audio upstream catch-up, 2026-09-21).
--
-- audio.cpp has no CLI surface: the mounted `server.json` *is* its
-- configuration, so every knob upstream adds there is a knob this gateway
-- cannot express until it has a column. Six of them are per model:
--
--   lazy                    - override the class's lazy_load for this row.
--   busy_timeout_ms         - how long a request waits for a model that is
--                             already running, and the ceiling a request's own
--                             value is clamped to. A TTS clip and a music
--                             generation do not belong under one bound.
--   default_request_options - request-option defaults for every call to this
--                             model; the request that names one still wins.
--   model_spec_override     - a `<family>.json` (or a directory of them) that
--                             replaces the image's built-in catalog lookup, so
--                             a family newer than the container image can be
--                             served without waiting for a new image.
--   config_id / weight_id   - which named config/weights asset to load when
--                             the model directory holds more than one.
--
-- All nullable or defaulted: an existing row keeps rendering exactly the
-- `server.json` it rendered before this migration, which is what keeps the
-- container-adoption check (§3.4) from evicting every audio container on the
-- first boot after an upgrade.
ALTER TABLE audio_models ADD COLUMN lazy INTEGER;
ALTER TABLE audio_models ADD COLUMN busy_timeout_ms INTEGER;
ALTER TABLE audio_models ADD COLUMN default_request_options TEXT NOT NULL DEFAULT '{}';
ALTER TABLE audio_models ADD COLUMN model_spec_override TEXT;
ALTER TABLE audio_models ADD COLUMN config_id TEXT;
ALTER TABLE audio_models ADD COLUMN weight_id TEXT;
