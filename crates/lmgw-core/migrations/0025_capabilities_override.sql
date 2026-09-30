-- Owner overrides for /v1/models capability facts (model-capabilities design
-- §7): a JSON object deep-merged over what lmgw derives from the GGUF/config
-- (local_models) or the upstream catalog (models, the alias table), so an
-- owner can fill a hole the automatic derivation leaves — a stock cloud
-- alias's modalities, a `tool_calls.kind: text` format the catalog never
-- states. NULL means no override; the derived object is published as is.
ALTER TABLE local_models ADD COLUMN capabilities_override TEXT;
ALTER TABLE models ADD COLUMN capabilities_override TEXT;
