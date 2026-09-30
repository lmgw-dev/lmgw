-- quickdoc corpus schema (§4).
--
-- This is a *separate* database file from the gateway's own (§3): it holds no
-- secrets, its migrations never mix with the main DB's, nuke-and-reingest is
-- always safe, and one `rsync` of the file is a full backup.

-- A corpus is one library at one version — `axum@0.8`. Several may exist per
-- library, one per version.
CREATE TABLE corpus (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    library               TEXT NOT NULL,
    version               TEXT NOT NULL,
    status                TEXT NOT NULL DEFAULT 'ingesting'
                          CHECK (status IN ('ingesting','ready','failed','re_embed_required')),
    -- Resolved embedding identity, pinned at ingest and verified on every
    -- query. An alias remapped to a different model with the same dimensions is
    -- otherwise silent and undetectable.
    embed_upstream        TEXT NOT NULL,
    embed_model           TEXT NOT NULL,
    embed_dims            INTEGER NOT NULL,
    -- A corpus is a function of two models; `docs__resolve` reports both.
    ingest_model          TEXT NOT NULL DEFAULT '',
    ingest_prompt_version TEXT NOT NULL DEFAULT '',
    crawl_date            TEXT NOT NULL DEFAULT '',
    source_kind           TEXT NOT NULL DEFAULT '',
    eval_score            REAL,
    chunk_count           INTEGER NOT NULL DEFAULT 0,
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now')),
    -- `library@version` is the client-facing corpus id, so it must be unique.
    UNIQUE (library, version)
);

-- One ingestion root. `fence` is the JSON array of domains a fetch may touch;
-- the ingest model never free-crawls (§8).
CREATE TABLE source (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    corpus_id  INTEGER NOT NULL REFERENCES corpus(id) ON DELETE CASCADE,
    root       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('llms_txt','markdown','rustdoc_json','html')),
    fence      TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (corpus_id, root)
);

-- One fetched page. `content_hash` gates incremental re-ingest: an unchanged
-- page never re-runs the model.
CREATE TABLE document (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    url          TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    fetched_at   TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (source_id, url)
);

-- `id` is hash(document url, payload) — see `store::chunk_id`. Content-derived
-- rather than offset-derived so a chunk whose text is unchanged keeps its id
-- when an earlier section of the page grows: citations and golden queries
-- survive a re-ingest.
--
-- `corpus_id` is denormalised from document → source → corpus because every
-- retrieval scopes by corpus and that join is on the hot path.
CREATE TABLE chunk (
    id              TEXT PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    corpus_id       INTEGER NOT NULL REFERENCES corpus(id) ON DELETE CASCADE,
    heading_path    TEXT NOT NULL DEFAULT '',
    span_start      INTEGER NOT NULL,
    span_end        INTEGER NOT NULL,
    -- VERBATIM slice of the source document. Never model-rewritten (§8).
    payload         TEXT NOT NULL,
    -- Little-endian f16[embed_dims], L2-normalised (§5). NULL until embedded.
    embedding       BLOB,
    -- LLM-derived, embedded for recall, surfaced only as labels.
    derived_title   TEXT NOT NULL DEFAULT '',
    derived_summary TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX chunk_document ON chunk(document_id);
CREATE INDEX chunk_corpus ON chunk(corpus_id);

-- External-content FTS5: the payload lives once, in `chunk`. `porter` stems
-- prose queries ("routing" → "route"); identifiers survive because unicode61
-- splits them the same way on both sides.
CREATE VIRTUAL TABLE chunk_fts USING fts5(
    payload,
    heading_path,
    derived_title,
    derived_summary,
    content='chunk',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

CREATE TRIGGER chunk_fts_ai AFTER INSERT ON chunk BEGIN
    INSERT INTO chunk_fts(rowid, payload, heading_path, derived_title, derived_summary)
    VALUES (new.rowid, new.payload, new.heading_path, new.derived_title, new.derived_summary);
END;

CREATE TRIGGER chunk_fts_ad AFTER DELETE ON chunk BEGIN
    INSERT INTO chunk_fts(chunk_fts, rowid, payload, heading_path, derived_title, derived_summary)
    VALUES ('delete', old.rowid, old.payload, old.heading_path, old.derived_title, old.derived_summary);
END;

-- Scoped to the indexed columns: writing an `embedding` must not churn the FTS
-- index, and re-embedding a whole corpus is exactly that write.
CREATE TRIGGER chunk_fts_au AFTER UPDATE OF payload, heading_path, derived_title, derived_summary
ON chunk BEGIN
    INSERT INTO chunk_fts(chunk_fts, rowid, payload, heading_path, derived_title, derived_summary)
    VALUES ('delete', old.rowid, old.payload, old.heading_path, old.derived_title, old.derived_summary);
    INSERT INTO chunk_fts(rowid, payload, heading_path, derived_title, derived_summary)
    VALUES (new.rowid, new.payload, new.heading_path, new.derived_title, new.derived_summary);
END;

-- Retrieval quality is measured, never vibed. `expected_chunk_ids` is a JSON
-- array; ids that no longer resolve surface as orphans in the eval report
-- instead of silently vanishing.
CREATE TABLE golden_query (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    corpus_id          INTEGER NOT NULL REFERENCES corpus(id) ON DELETE CASCADE,
    query              TEXT NOT NULL,
    expected_chunk_ids TEXT NOT NULL DEFAULT '[]',
    origin             TEXT NOT NULL DEFAULT 'manual' CHECK (origin IN ('manual','synthetic')),
    created_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX golden_query_corpus ON golden_query(corpus_id);

-- `docs__request` queue (§7). Empty `version` means "unspecified"; it is part
-- of the uniqueness key so a repeat request bumps `count` instead of piling up.
CREATE TABLE doc_request (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    library            TEXT NOT NULL,
    version            TEXT NOT NULL DEFAULT '',
    reason             TEXT,
    client_name        TEXT,
    count              INTEGER NOT NULL DEFAULT 1,
    first_requested_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_requested_at  TEXT NOT NULL DEFAULT (datetime('now')),
    status             TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending','fulfilled','dismissed')),
    UNIQUE (library, version)
);
