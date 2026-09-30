-- Structured llama-server params for local models (dedicated form fields).
-- Stored as one JSON object (same pattern as settings); the freeform `args`
-- column remains for everything without a dedicated field.
ALTER TABLE local_models ADD COLUMN params TEXT NOT NULL DEFAULT '{}';
