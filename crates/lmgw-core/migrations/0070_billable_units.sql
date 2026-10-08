-- Billable units (billable-units design §5): prices in units other than
-- tokens, and the quantities a request processed besides them.
--
-- Providers bill on more than tokens: minutes of input audio, characters of
-- input text, generated images, a fee per request. A request row records what
-- it processed in those units, typed columns rather than a quantity/unit pair
-- (§5.1), and the rollup sums them beside the tokens.
--
-- Unknown is NULL, never 0, here as everywhere (usage-analytics §2.3). Old
-- rows are never re-priced and have no quantities: nothing can be backfilled,
-- so the rollup's DEFAULT 0 on an earlier bucket means "not recorded".
-- `store::units_since` reads when this file was applied from
-- `_sqlx_migrations`, and the Usage page says so (§5.5).

-- request_logs: what the request processed besides tokens, each NULL when it
-- was neither measured nor reported by the provider (§4.1, §5.2).
ALTER TABLE request_logs ADD COLUMN audio_in_ms            INTEGER;  -- input audio
ALTER TABLE request_logs ADD COLUMN chars_in               INTEGER;  -- input text, as sent
ALTER TABLE request_logs ADD COLUMN images_out             INTEGER;  -- generated images
-- The non-token part of cost_micro (§3.2): NULL when the scope has no row in
-- any other unit, and NULL with cost_micro when any priced part is unknown.
ALTER TABLE request_logs ADD COLUMN cost_units_micro       INTEGER;
-- The rates as used (§3.4), each named `price_` + its unit, snapshotted like
-- price_in: the row explains its own cost after the price changes.
ALTER TABLE request_logs ADD COLUMN price_per_audio_minute REAL;
ALTER TABLE request_logs ADD COLUMN price_per_mchar        REAL;
ALTER TABLE request_logs ADD COLUMN price_per_image        REAL;
ALTER TABLE request_logs ADD COLUMN price_per_request      REAL;

-- usage_hourly (§5.3): a quantity adds its measured value and an unmeasured
-- one adds 0, as tokens_in sums reported tokens. The remainder carries the
-- quantities of every request it counts, beside cost_unknown_tokens.
ALTER TABLE usage_hourly ADD COLUMN audio_in_ms              INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN chars_in                 INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN images_out               INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_audio_in_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_chars_in    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_images_out  INTEGER NOT NULL DEFAULT 0;

-- prices (§2.1, §2.2, §5.4): the unit set, and one `price` column for every
-- unit but tokens. `price_in`/`price_out` keep their token meaning, and the
-- table CHECK keeps the two shapes apart: a token row carries the four token
-- rates and no `price`, any other row `price` and no token rate.
--
-- `per_second` and `per_char` are dropped: they named a scale that
-- `per_audio_minute` and `per_mchar` replace, and lmgw never wrote them. A row
-- in either, or a `per_image`/`per_request` row that holds token rates, could
-- not be carried over; `store::migration_guards::billable_units` refuses the
-- upgrade before this file runs and names such rows. Every other row copies
-- unchanged: lmgw itself has only ever written `per_mtok`.
--
-- SQLite cannot alter a CHECK, so the table is rebuilt. Nothing references
-- `prices` and it has no foreign keys, so this runs in sqlx's own transaction
-- (unlike 0058's `-- no-transaction`): a failure leaves the old table.
CREATE TABLE prices_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 'alias'          → scope_key is the models.alias (or aux/audio prefixed id)
    -- 'upstream_model' → scope_key is '<upstream_id>:<upstream_model_id>', which
    --                    is what an expose_all passthrough request resolves to.
    scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('alias','upstream_model')),
    scope_key   TEXT NOT NULL,
    -- The default stays per_mtok, so an insert that names no unit is a token
    -- row as before.
    unit        TEXT NOT NULL DEFAULT 'per_mtok'
                CHECK (unit IN ('per_mtok','per_audio_minute','per_mchar','per_image',
                                'per_request')),
    -- per_mtok: currency units per 1M tokens. NULL = this dimension is not
    -- priced, which is NOT the same as free.
    price_in          REAL,
    price_out         REAL,
    price_cache_read  REAL,
    price_cache_write REAL,
    -- Every other unit: the rate per the unit's scale — per minute of input
    -- audio, per 1M input characters, per generated image, per answered
    -- request (§3.3).
    price             REAL,
    -- 'catalog' rows are refreshed from the upstream's own model list; 'manual'
    -- rows are the owner's and always win, per unit (§2.3).
    source      TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('catalog','manual')),
    note        TEXT,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (CASE WHEN unit = 'per_mtok' THEN price IS NULL
                ELSE price_in IS NULL AND price_out IS NULL
                     AND price_cache_read IS NULL AND price_cache_write IS NULL END)
);

-- Every row with its id and updated_at.
INSERT INTO prices_new
    (id, scope_kind, scope_key, unit, price_in, price_out, price_cache_read,
     price_cache_write, source, note, updated_at)
SELECT id, scope_kind, scope_key, unit, price_in, price_out, price_cache_read,
       price_cache_write, source, note, updated_at
FROM prices;

-- Carry the AUTOINCREMENT high-water mark, as 0058 does for `upstreams`:
-- `DROP TABLE prices` takes its `sqlite_sequence` row with it, and the copy only
-- raises the new table's to max(id), so a deleted top id would be handed out
-- again. Two statements because the copy creates no sequence row for an empty
-- table.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'prices_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'prices_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'prices'), 0))
 WHERE name = 'prices_new';

DROP TABLE prices;
ALTER TABLE prices_new RENAME TO prices;

-- One row per (scope, source, unit), as 0026 had it: a catalog refresh upserts
-- its own row and never clobbers the owner's, and one scope holds one row per
-- unit — tokens and a per-request fee side by side.
CREATE UNIQUE INDEX idx_prices_scope ON prices (scope_kind, scope_key, source, unit);
