-- Server-side voice selection for audio.cpp TTS models.
--
-- Voice is a property of the loaded session, not of a request: fish-audio
-- exposes no built-in voices and samples a NEW random speaker per request
-- unless it is given a reference clip, and pocket-tts refuses to synthesize at
-- all without one ("session prepare() requires a session voice"). audio.cpp
-- takes both as model config — `voice_presets` (name -> {voice_id} or
-- {voice_ref, reference_text}) plus a `default_voice_preset` used when a
-- request names no voice — so a client can finally get the same speaker twice.
ALTER TABLE audio_models ADD COLUMN voice_presets TEXT NOT NULL DEFAULT '{}';
-- Either a preset name from the object above, or an inline preset object.
ALTER TABLE audio_models ADD COLUMN default_voice_preset TEXT NOT NULL DEFAULT '';
