-- Pages a vision model read (review R3, finding 2): a re-chunk of a file whose
-- bytes did not change must not run vision OCR again on the same pages.
--
-- Keyed by everything the answer depends on: the file's bytes (sha256), the
-- page, the vision alias, the prompt kind (`ocr` for a text-less page,
-- `structure` for a table page) and the prompt version. Only successful,
-- non-empty reads are stored. Removed with the last file row naming the bytes.
CREATE TABLE kb_page_read (
    file_sha       TEXT NOT NULL,
    page           INTEGER NOT NULL,
    vision_alias   TEXT NOT NULL,
    mode           TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    text           TEXT NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (file_sha, page, vision_alias, mode, prompt_version)
);
