-- Two review fixes (R2, findings 6 and 8).
--
-- 1. Counting the chunks with a vector (`COUNT(embedding)`) read every blob of
--    every base on each auto-mode Chat turn and each `kb__search`. The chunks
--    *without* a vector are few and are what every count wants to know
--    (`embedded = chunks - unembedded`), so they get their own small index:
--    the count is an index scan over just those rows, and `COUNT(*)` per base
--    runs off `kb_chunk_kb`.
CREATE INDEX kb_chunk_unembedded ON kb_chunk(kb_id) WHERE embedding IS NULL;

-- 2. `vectors_rev` moved once per row on an `embedding` update, so a re-embed
--    bumped it once per chunk. The re-embed writes a batch in one transaction
--    and bumps the revision once for it (`store::set_chunk_embeddings`,
--    `store::clear_embeddings`); inserts and deletes keep their triggers.
DROP TRIGGER kb_chunk_rev_au;
