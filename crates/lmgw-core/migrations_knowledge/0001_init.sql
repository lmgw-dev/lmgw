-- Knowledge bases (chat-complete design §9.1): the owner's own documents, in
-- named collections, retrieved into Chat threads and served as `kb__*` on /mcp.
--
-- A separate file (`knowledge.db`) with its own migrations, next to lmgw.sqlite
-- and clamped to 0600 like it: these are private documents. Never the docs
-- corpus file (quickdoc.db is deliberately world-readable and exported whole).
-- The originals live beside it under `knowledge/`, named by their sha256.

-- One named collection. Its vectors are pinned to the *resolved* embedding
-- identity (upstream, model, dims), not to the alias typed — the same rule a
-- docs corpus follows: an alias remapped to another model of the same width
-- would otherwise poison every answer with no symptom.
CREATE TABLE kb (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    description    TEXT NOT NULL DEFAULT '',
    -- What the owner picked, kept for display; the pin below is what counts.
    embed_alias    TEXT NOT NULL,
    embed_upstream TEXT NOT NULL,
    embed_model    TEXT NOT NULL,
    embed_dims     INTEGER NOT NULL,
    -- '' = no rerank stage for this base.
    rerank_alias   TEXT NOT NULL DEFAULT '',
    -- '' = text-less PDF pages are skipped and counted, not read.
    vision_alias   TEXT NOT NULL DEFAULT '',
    chunk_tokens   INTEGER NOT NULL DEFAULT 512,
    chunk_overlap  INTEGER NOT NULL DEFAULT 64,
    -- Listed and searchable on /mcp. The Chat's own selection ignores it.
    mcp_visible    INTEGER NOT NULL DEFAULT 1,
    -- `re_embed_required` while a model change re-embeds every chunk.
    status         TEXT NOT NULL DEFAULT 'ready'
                   CHECK (status IN ('ready','re_embed_required')),
    -- Bumped by the triggers below on every chunk write, so the resident
    -- vector matrix a search holds is reloaded exactly when it went stale.
    vectors_rev    INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

-- One uploaded file. `sha256` names its original on disk; two bases holding
-- the same bytes share one original, which is deleted with its last row.
CREATE TABLE kb_file (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kb_id         INTEGER NOT NULL REFERENCES kb(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    -- `text` | `pdf` | `office` — what the bytes sniffed as, never the name.
    kind          TEXT NOT NULL,
    -- The concrete format (`pdf`, `docx`, `xlsx`, `text`, …) and its MIME.
    sub           TEXT NOT NULL DEFAULT '',
    mime          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    sha256        TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending','ingesting','ready','failed')),
    -- Why it failed, or why it is still pending (a GPU hold, a cancel). NULL
    -- when there is nothing to say.
    error         TEXT,
    -- PDF pages, slides or sheets; NULL for a text file.
    pages         INTEGER,
    -- PDF pages with no text that no vision model read.
    skipped_pages INTEGER NOT NULL DEFAULT 0,
    -- JSON array of visible per-file notes ("pages 3, 7: no text …").
    notes         TEXT NOT NULL DEFAULT '[]',
    chunk_count   INTEGER NOT NULL DEFAULT 0,
    -- The extracted text the chunks' spans point into — what the source
    -- viewer shows around an excerpt. NULL until ingested.
    text          TEXT,
    added_at      TEXT NOT NULL DEFAULT (datetime('now')),
    ingested_at   TEXT,
    UNIQUE (kb_id, sha256)
);
CREATE INDEX kb_file_kb ON kb_file(kb_id, status);
CREATE INDEX kb_file_sha ON kb_file(sha256);

-- A chunk: a verbatim region of its file's extracted text (`span_start` ..
-- `span_end`, byte offsets into `kb_file.text`), the payload a caller
-- receives (the region, with a split table's header row repeated or a split
-- code fence re-opened), and its vector. `id` is content-derived —
-- sha256(kb ‖ file sha256 ‖ seq ‖ payload) — so an unchanged file re-ingested
-- with unchanged settings keeps its ids, and the citations stored with Chat
-- messages keep resolving.
CREATE TABLE kb_chunk (
    id           TEXT PRIMARY KEY,
    kb_id        INTEGER NOT NULL REFERENCES kb(id) ON DELETE CASCADE,
    file_id      INTEGER NOT NULL REFERENCES kb_file(id) ON DELETE CASCADE,
    -- Reading order within the file.
    seq          INTEGER NOT NULL,
    -- 1-based PDF page; NULL for anything else.
    page         INTEGER,
    heading_path TEXT NOT NULL DEFAULT '',
    span_start   INTEGER NOT NULL,
    span_end     INTEGER NOT NULL,
    payload      TEXT NOT NULL,
    -- tiktoken o200k_base count of the embedded text (heading path + payload).
    tokens       INTEGER NOT NULL DEFAULT 0,
    -- Little-endian f16[embed_dims], L2-normalised. NULL until embedded.
    embedding    BLOB,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX kb_chunk_file ON kb_chunk(file_id, seq);
CREATE INDEX kb_chunk_kb ON kb_chunk(kb_id);

-- External-content FTS5 over the payload and heading path. `porter` over
-- `unicode61 remove_diacritics 2`: English queries find inflected forms, and
-- stripping diacritics on both sides lets "Gebuhr" find "Gebühr". Porter only
-- strips English suffixes, which leaves German words mostly alone; the vector
-- stage carries the rest.
CREATE VIRTUAL TABLE kb_chunk_fts USING fts5(
    payload,
    heading_path,
    content='kb_chunk',
    content_rowid='rowid',
    tokenize='porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER kb_chunk_fts_ai AFTER INSERT ON kb_chunk BEGIN
    INSERT INTO kb_chunk_fts(rowid, payload, heading_path)
    VALUES (new.rowid, new.payload, new.heading_path);
END;

CREATE TRIGGER kb_chunk_fts_ad AFTER DELETE ON kb_chunk BEGIN
    INSERT INTO kb_chunk_fts(kb_chunk_fts, rowid, payload, heading_path)
    VALUES ('delete', old.rowid, old.payload, old.heading_path);
END;

-- Scoped to the indexed columns: a re-embed writes every `embedding` and must
-- not churn the index.
CREATE TRIGGER kb_chunk_fts_au AFTER UPDATE OF payload, heading_path ON kb_chunk BEGIN
    INSERT INTO kb_chunk_fts(kb_chunk_fts, rowid, payload, heading_path)
    VALUES ('delete', old.rowid, old.payload, old.heading_path);
    INSERT INTO kb_chunk_fts(rowid, payload, heading_path)
    VALUES (new.rowid, new.payload, new.heading_path);
END;

-- The resident-matrix revision (see `kb.vectors_rev`).
CREATE TRIGGER kb_chunk_rev_ai AFTER INSERT ON kb_chunk BEGIN
    UPDATE kb SET vectors_rev = vectors_rev + 1 WHERE id = new.kb_id;
END;

CREATE TRIGGER kb_chunk_rev_ad AFTER DELETE ON kb_chunk BEGIN
    UPDATE kb SET vectors_rev = vectors_rev + 1 WHERE id = old.kb_id;
END;

CREATE TRIGGER kb_chunk_rev_au AFTER UPDATE OF embedding ON kb_chunk BEGIN
    UPDATE kb SET vectors_rev = vectors_rev + 1 WHERE id = new.kb_id;
END;
