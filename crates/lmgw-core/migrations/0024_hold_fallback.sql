-- GPU-hold per-model fallback override (gpu-hold design §2/§3.2).
--
-- `hold_fallback_mode` defaults to 'inherit': a row that has never touched
-- this keeps behaving exactly as `settings.hold.fallback_alias` (chat) or "no
-- fallback" (aux/audio, which never inherit the global — §2) already says.
-- `hold_fallback` is the alias `hold_fallback_mode = 'alias'` reads; NULL
-- everywhere else, and meaningless under any other mode.
--
-- Two columns rather than a single nullable one: every patch struct in this
-- crate already treats an empty string as "not supplied", so a lone
-- `hold_fallback` column could not represent "inherit" vs "explicitly none"
-- vs "not touched by this patch" at once.
ALTER TABLE local_models ADD COLUMN hold_fallback_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE local_models ADD COLUMN hold_fallback TEXT;

ALTER TABLE aux_models ADD COLUMN hold_fallback_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE aux_models ADD COLUMN hold_fallback TEXT;

ALTER TABLE audio_models ADD COLUMN hold_fallback_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE audio_models ADD COLUMN hold_fallback TEXT;
