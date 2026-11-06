-- The media tools' default models (chat-media-tools design §3.8): a Chat
-- thread's own, as one JSON object (`lmgw_api_types::chat_media::
-- MediaDefaults`: `generate`, `edit`, `speak`, `voice`, `transcribe`). An
-- absent key inherits the thread's folder's (the folder's `defaults` JSON,
-- `media_defaults`), then Settings → Chat's (`chat_media_defaults` in the
-- settings JSON); an empty one is no default. Read tolerantly, as `voice`
-- is. Every existing thread gets `{}`: it inherits everything.
ALTER TABLE chat_threads ADD COLUMN media_defaults TEXT NOT NULL DEFAULT '{}';
