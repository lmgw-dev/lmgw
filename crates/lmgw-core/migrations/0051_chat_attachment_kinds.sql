-- Attachment kinds beyond image and text (chat-complete design §8): pdf,
-- office and audio.
--
-- `extracted`: the derived text — a PDF's pages under `--- page N ---`, an
-- office file as markdown, an audio file's transcript. NULL for images and
-- for plain text (the bytes are the text).
-- `meta`: JSON — pages, text-less pages, class (text|scanned|hybrid), sheet
-- names, transcript alias / error, tokens (the chips' estimate). '' = none.
-- `mode`: a text-class PDF's choice, 'text' | 'images'; NULL = not applicable
-- or not chosen yet.
ALTER TABLE chat_attachments ADD COLUMN extracted TEXT;
ALTER TABLE chat_attachments ADD COLUMN meta TEXT;
ALTER TABLE chat_attachments ADD COLUMN mode TEXT;

-- Rendered page images of a PDF, filled on first need and dropped with the
-- attachment.
CREATE TABLE chat_attachment_pages (
    attachment_id INTEGER NOT NULL REFERENCES chat_attachments(id) ON DELETE CASCADE,
    page          INTEGER NOT NULL,
    png           BLOB NOT NULL,
    PRIMARY KEY (attachment_id, page)
);
