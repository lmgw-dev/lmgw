-- A Chat reply a fallback that cannot see answered (the owner's ruling of
-- 2026-10-06: a configured fallback is always used, with no exception by
-- content): the turn's note, saying who answered for which model and what
-- went to it in the images' place ("PDF pages went to it as the PDF's text",
-- "the images went to it as placeholders"). Shown on the reply, so it
-- survives a reload and an export. NULL on every other row.
ALTER TABLE chat_messages ADD COLUMN images_note TEXT;
