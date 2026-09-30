-- Three gaps found by an adversarial review of the usage plane, all of the same
-- kind: a number the write path knows and the aggregate cannot represent.

-- 1. The decode-token count itself.
--
-- `roll_up` folds llama.cpp's `predicted_n` into usage_hourly.decode_tokens, but
-- the raw row kept only the *rate* (decode_tok_s) and the duration — so
-- `rebuild_usage` had to reconstruct the count as `rate × ms`, and
-- `CAST(41.23 * 24.25 / 1000.0 AS INTEGER)` is 0: a short turn lost every one
-- of its tokens, and a backfill silently lowered throughput most where turns
-- were shortest. Store what was actually counted.
ALTER TABLE request_logs ADD COLUMN predicted_n INTEGER;

-- 2. Anthropic's cache-*write* tokens.
--
-- They bill at 1.25x, are priced correctly on the row, and then had nowhere to
-- go in the rollup — so the dearest input tier was invisible in every chart,
-- aggregate and export. `tokens_cached` continues to mean cache *reads*.
ALTER TABLE usage_hourly ADD COLUMN tokens_cache_write INTEGER NOT NULL DEFAULT 0;

-- 3. `upstream_id` on the latency histogram.
--
-- Every other figure on the Usage page narrows when the owner filters to one
-- upstream; the percentiles did not, because the histogram had no column to
-- filter on. Filtering to the local llama-server to ask "did the image bump
-- make it slower" returned a number dominated by cloud latency, with nothing
-- to indicate it. A WITHOUT ROWID primary key cannot be widened in place, so
-- the table is rebuilt; existing rows take upstream_id 0, which reads as
-- "recorded before this column existed" and is exactly what an unfiltered
-- query already returns.
CREATE TABLE usage_latency_hourly_new (
    bucket_utc  TEXT NOT NULL,
    key_id      INTEGER NOT NULL DEFAULT 0,
    alias       TEXT NOT NULL,
    upstream_id INTEGER NOT NULL DEFAULT 0,
    class       TEXT NOT NULL DEFAULT 'chat',
    metric      TEXT NOT NULL CHECK (metric IN ('ttfb','total')),
    bucket_idx  INTEGER NOT NULL,
    count       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx)
) WITHOUT ROWID;

INSERT INTO usage_latency_hourly_new
    (bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx, count)
SELECT bucket_utc, key_id, alias, 0, class, metric, bucket_idx, count
FROM usage_latency_hourly;

DROP TABLE usage_latency_hourly;
ALTER TABLE usage_latency_hourly_new RENAME TO usage_latency_hourly;
CREATE INDEX idx_usage_latency_bucket ON usage_latency_hourly (bucket_utc);
