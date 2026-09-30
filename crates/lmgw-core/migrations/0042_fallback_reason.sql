-- Why a fallback answered instead of the requested local model
-- (candidate-aliases design §4.7): the request log's copy of the
-- `x-lmgw-fallback-reason` response header — `hold` (the GPU hold re-routed
-- it), `external_vram` (VRAM outside lmgw's control was short, so the
-- fallback answered at once instead of queueing), `background` (a background
-- candidate alias). The fallback itself is the row's `upstream_*` columns,
-- and `requested_alias` is what the client asked for. NULL = no fallback;
-- rows written before this column existed are NULL even when the hold
-- served them.
ALTER TABLE request_logs ADD COLUMN fallback_reason TEXT;
