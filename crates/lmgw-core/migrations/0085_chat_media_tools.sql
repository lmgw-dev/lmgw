-- The Chat's image and audio tools (chat-media-tools design §6.3, §3.4,
-- §5.2): all of the feature's schema in one file.
--
-- A Chat attachment says where it came from: `upload` (the user sent it),
-- `tool` (an image__* or audio__* call of a turn made it, bound to the reply
-- when the turn is saved) or `action` (an image card's action made it). Every
-- draft query reads `upload` only, so a tool's file waiting for its reply is
-- never taken for a draft. `call_id` is the tool call that made a `tool` file.
--
-- An image row says how its prompts are written (`prompt_style`, NULL: the
-- matching recipe's) and how a running generation's preview is drawn
-- (`preview_mode`, NULL: the default — `proj` where the build offers it).
ALTER TABLE chat_attachments ADD COLUMN source TEXT NOT NULL DEFAULT 'upload'
    CHECK (source IN ('upload', 'tool', 'action'));
ALTER TABLE chat_attachments ADD COLUMN call_id TEXT;
CREATE INDEX idx_chat_attachments_call
    ON chat_attachments (thread_id, call_id) WHERE call_id IS NOT NULL;
ALTER TABLE image_models ADD COLUMN prompt_style TEXT
    CHECK (prompt_style IN ('natural', 'keywords', 'tags'));
ALTER TABLE image_models ADD COLUMN preview_mode TEXT
    CHECK (preview_mode IN ('none', 'proj', 'tae', 'vae'));
