-- Priced request rows + the usage detail pricing needs (usage-analytics §2.1).
--
-- Everything is nullable and stays NULL when unknown. A request whose price is
-- not known is `price_source='unknown'` and `cost_micro IS NULL` — *never* 0
-- (§2.3). A zero for an unknown reads as authoritative and is wrong downward,
-- which is the one failure mode that makes an analytics page worse than none.
--
-- Money is integer **micro-units** of the configured currency; floats do not
-- add up. Note the arithmetic falls out clean: a price expressed per 1M tokens
-- means cost_micro = tokens * price_per_mtok, no scaling either way.

-- identity + classification
ALTER TABLE request_logs ADD COLUMN key_id INTEGER;   -- api_keys.id; client_key keeps the name
ALTER TABLE request_logs ADD COLUMN class TEXT;       -- chat | aux | audio | tool

-- money
ALTER TABLE request_logs ADD COLUMN cost_micro INTEGER;
ALTER TABLE request_logs ADD COLUMN cost_in_micro INTEGER;
ALTER TABLE request_logs ADD COLUMN cost_out_micro INTEGER;
-- the prices as USED, snapshotted — not a FK to a mutable price row (§2.2):
-- providers re-price, and recomputing history against today's sheet silently
-- rewrites a number the owner already read.
ALTER TABLE request_logs ADD COLUMN price_in REAL;
ALTER TABLE request_logs ADD COLUMN price_out REAL;
ALTER TABLE request_logs ADD COLUMN price_cache_read REAL;
ALTER TABLE request_logs ADD COLUMN price_cache_write REAL;
ALTER TABLE request_logs ADD COLUMN price_source TEXT;  -- catalog|manual|free_local|unknown

-- usage detail the IR did not carry before (§2.3)
ALTER TABLE request_logs ADD COLUMN cached_in_tokens INTEGER;
ALTER TABLE request_logs ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE request_logs ADD COLUMN reasoning_tokens INTEGER;

-- llama.cpp timings: what a local request costs instead of money (§2.4).
-- Previously parsed, shown live in the Chat tab, and dropped.
ALTER TABLE request_logs ADD COLUMN prefill_ms REAL;
ALTER TABLE request_logs ADD COLUMN decode_ms REAL;
ALTER TABLE request_logs ADD COLUMN decode_tok_s REAL;
ALTER TABLE request_logs ADD COLUMN prompt_n INTEGER;      -- prefill tokens actually processed
ALTER TABLE request_logs ADD COLUMN cache_n INTEGER;       -- prompt tokens served from the KV cache
ALTER TABLE request_logs ADD COLUMN draft_n INTEGER;       -- speculative tokens proposed
ALTER TABLE request_logs ADD COLUMN draft_accepted INTEGER;

CREATE INDEX idx_request_logs_key ON request_logs (key_id, ts DESC);
