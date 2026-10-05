-- Chat voice (chat-voice design §2.2, §3): a thread's voice overrides, and
-- what a spoken turn leaves on its message. No audio is stored.
--
-- `chat_threads.voice`: the thread's own voice settings as one JSON object
-- (`store::chat_voice::ThreadVoice`) — ASR and TTS alias, voice, speech
-- style, language, read-aloud, turn detection and the thread's TTS seed. An
-- absent key inherits Settings → Chat → Voice, then realtime's settings.
-- Only the voice feature reads these fields, so they share one column, read
-- tolerantly: a key a newer build wrote is dropped, not a reason to fail the
-- thread. Every existing thread gets `{}`: it overrides nothing.
--
-- `chat_messages.voice`: how a turn was spoken (`MessageVoice`) — on a user
-- message the transcription model and its timings, on a spoken reply the TTS
-- model, the voice, the timing and the part that was never heard. NULL is a
-- typed turn, which every existing message is.
ALTER TABLE chat_threads ADD COLUMN voice TEXT NOT NULL DEFAULT '{}';
ALTER TABLE chat_messages ADD COLUMN voice TEXT;
