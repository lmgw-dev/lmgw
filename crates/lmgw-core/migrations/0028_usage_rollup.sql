-- Hourly usage rollups (usage-analytics design §3) — the history layer.
--
-- `request_logs` is a tail: `retention_days` (30) and `retention_max_rows`
-- (200k) prune it, so a dashboard reading it directly loses a month of history
-- the first time the gateway gets busy. These rows are the history; the raw
-- rows stay the detail. The pruner touches only the raw rows (§3.3).
--
-- Written in the SAME transaction as the request_logs insert (§3.2). Not a
-- periodic batch over recent rows: a batch's window and the pruner's window are
-- two clocks that eventually disagree, and the failure mode is silent
-- under-counting of exactly the busiest hour.
CREATE TABLE usage_hourly (
    -- UTC hour, 'YYYY-MM-DDTHH'. Rendering in local time (and grouping to local
    -- days) happens in the query (§3.4) — a bucket that silently means UTC-day
    -- to the query and local-day to the reader is wrong every day.
    bucket_utc  TEXT NOT NULL,
    key_id      INTEGER NOT NULL DEFAULT 0,   -- 0 = no key (auth off / unattributed)
    alias       TEXT NOT NULL,
    upstream_id INTEGER NOT NULL DEFAULT 0,
    class       TEXT NOT NULL DEFAULT 'chat',
    -- Four classes, not a status code: the cardinality of this key is what keeps
    -- the table small enough to never need its own retention.
    outcome     TEXT NOT NULL DEFAULT 'ok'
                CHECK (outcome IN ('ok','client_error','upstream_error','refused')),

    requests         INTEGER NOT NULL DEFAULT 0,
    tokens_in        INTEGER NOT NULL DEFAULT 0,
    tokens_out       INTEGER NOT NULL DEFAULT 0,
    tokens_cached    INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning INTEGER NOT NULL DEFAULT 0,

    cost_micro            INTEGER NOT NULL DEFAULT 0,
    -- What the cost total does NOT cover, carried beside it so every sum can
    -- state its own remainder (§2.3).
    cost_unknown_requests INTEGER NOT NULL DEFAULT 0,
    cost_unknown_tokens   INTEGER NOT NULL DEFAULT 0,

    -- Latency sums for the cheap questions; the distribution lives in
    -- usage_latency_hourly below, because percentiles do not average — a p95
    -- rolled up as the mean of hourly p95s is a number that exists nowhere in
    -- the data.
    ttfb_sum    INTEGER NOT NULL DEFAULT 0,
    ttfb_count  INTEGER NOT NULL DEFAULT 0,
    total_sum   INTEGER NOT NULL DEFAULT 0,
    total_count INTEGER NOT NULL DEFAULT 0,
    total_min   INTEGER,
    total_max   INTEGER,

    -- Local throughput as a ratio pair, so tok/s aggregates as Σtokens / Σms.
    -- A mean of per-request rates is not the rate.
    decode_tokens INTEGER NOT NULL DEFAULT 0,
    decode_ms     REAL    NOT NULL DEFAULT 0,
    prefill_ms    REAL    NOT NULL DEFAULT 0,
    prompt_n      INTEGER NOT NULL DEFAULT 0,
    cache_n       INTEGER NOT NULL DEFAULT 0,
    draft_n       INTEGER NOT NULL DEFAULT 0,
    draft_accepted INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (bucket_utc, key_id, alias, upstream_id, class, outcome)
) WITHOUT ROWID;

CREATE INDEX idx_usage_hourly_bucket ON usage_hourly (bucket_utc);

-- The latency distribution, as a sparse histogram: one row per occupied bucket.
--
-- *(rev of the spec's §3.1, which packed 24 u32 counts into a BLOB per rollup
-- row.)* A BLOB has to be read, decoded, incremented and written back, which
-- makes every logged request a read-modify-write inside the same transaction as
-- the insert — two concurrent requests then race for the same row and one of
-- them loses its log row to a BUSY. Rows are pure `count = count + 1` upserts:
-- atomic in SQL, no read, and summable by the query planner instead of by hand.
--
-- Buckets are 4 per octave: `idx = round(4 * log2(ms))`, so a reported
-- percentile is accurate to about ±9% rather than the ±100% a doubling ladder
-- would give. `store::latency_bucket_ms` maps an index back to milliseconds.
--
-- Only **successful** requests are recorded. A 401's time-to-first-byte is not
-- a latency signal, and letting refusals into the distribution makes a p50 drop
-- precisely when a gateway starts failing fast.
CREATE TABLE usage_latency_hourly (
    bucket_utc TEXT NOT NULL,
    key_id     INTEGER NOT NULL DEFAULT 0,
    alias      TEXT NOT NULL,
    class      TEXT NOT NULL DEFAULT 'chat',
    metric     TEXT NOT NULL CHECK (metric IN ('ttfb','total')),
    bucket_idx INTEGER NOT NULL,
    count      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_utc, key_id, alias, class, metric, bucket_idx)
) WITHOUT ROWID;

CREATE INDEX idx_usage_latency_bucket ON usage_latency_hourly (bucket_utc);
