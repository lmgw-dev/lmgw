-- Synthetic golden-query candidates (§10, §11).
--
-- A *separate* table from `golden_query` on purpose: a generated query is a
-- proposal, and an eval must never score one the owner has not looked at. §11's
-- curation queue is this table filtered to `pending`; accepting a candidate is
-- what writes a real `golden_query` row (origin `synthetic`), and the id it
-- became is kept here so an accepted row can still be traced back to the run
-- that proposed it.
--
-- Rejections are kept rather than deleted so a second generation run does not
-- propose the same query again — `UNIQUE (corpus_id, query)` is what makes that
-- automatic, and it is also what stops one run from filing a duplicate twice.
CREATE TABLE golden_candidate (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    corpus_id          INTEGER NOT NULL REFERENCES corpus(id) ON DELETE CASCADE,
    query              TEXT NOT NULL,
    -- The chunk the query was written from; the model never names a chunk id,
    -- code attaches the one it was shown (§8's split of duties, applied here).
    expected_chunk_ids TEXT NOT NULL DEFAULT '[]',
    -- The model's own words about why that chunk answers it — the context the
    -- owner curates with, never stored as anything but a label.
    rationale          TEXT NOT NULL DEFAULT '',
    -- Which model wrote it, so a bad batch is attributable to a model rather
    -- than to "the generator".
    model              TEXT NOT NULL DEFAULT '',
    status             TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending','accepted','rejected')),
    -- The golden_query this became, when it was accepted.
    golden_query_id    INTEGER,
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    decided_at         TEXT NOT NULL DEFAULT '',
    UNIQUE (corpus_id, query)
);
CREATE INDEX golden_candidate_corpus ON golden_candidate(corpus_id, status);
