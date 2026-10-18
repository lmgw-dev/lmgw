-- The catalog request asks for every output modality (decision-models design
-- §6.2): `GET {base}/models?output_modalities=all` on an `openai` row of kind
-- `generic` while this is 1. OpenRouter's default list omits every model whose output is not
-- text (459 of 661 entries on 2026-10-12, 461 of 664 on 2026-10-14), Jev and
-- the other decision models among them; with the parameter it lists them.
--
-- A per-row option, because one OpenAI-compatible endpoint refuses the
-- parameter: Gemini's (`…/v1beta/openai/models`) answers 400 "Unknown name
-- \"output_modalities\": Cannot bind query parameter", which would empty that
-- row's catalog. OpenAI, Kilo's gateway and llama-server answered the same
-- list with and without it (live check, 2026-10-14). Only the catalog
-- request of an `openai` row of kind `generic` carries the parameter; on the
-- other rows the column says nothing.
--
-- Every row starts on, except an `openai` row on Gemini's host, the one
-- endpoint known to refuse it.
ALTER TABLE upstreams ADD COLUMN catalog_all_modalities INTEGER NOT NULL DEFAULT 1
    CHECK (catalog_all_modalities IN (0, 1));

UPDATE upstreams SET catalog_all_modalities = 0
 WHERE protocol = 'openai' AND base_url LIKE '%generativelanguage.googleapis.com%';
