-- Ladder models (ladder design §4.1, §6). A local chat row's weights
-- selector becomes a table of rungs: `ladder` holds the *higher* rungs only
-- (`{gguf_path, ctx_size}` JSON objects) — the base rung stays the row's own
-- `gguf_path` / `params.ctx_size`, unchanged, so everything that already
-- reads those fields keeps working for rung 1. An empty array (the default)
-- means "not a ladder", exactly like `local_models.args`'s empty-array
-- default for "no freeform flags".
--
-- `request_logs.rung` is the request gate's record of which rung answered a
-- ladder row's request, 1-based like every other rung surface (§12 entry
-- 11). NULL for a row without a ladder, and — until phase 3's WP3 wires a
-- climb into the request path — for every row today.
ALTER TABLE local_models ADD COLUMN ladder TEXT NOT NULL DEFAULT '[]';
ALTER TABLE request_logs ADD COLUMN rung INTEGER;
