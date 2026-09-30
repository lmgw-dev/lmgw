-- Per-token prices (usage-analytics design §2.2).
--
-- lmgw has always *parsed* upstream catalog pricing (catalog.rs) and
-- republished it on `/v1/models`; nothing ever multiplied it by a token count.
-- This is the table that makes a request priceable.
--
-- One currency, no FX (§9): the amounts here are plain numbers and the
-- `currency` **setting** is the label they are displayed under. *(rev of the
-- spec's §2.2 sketch, which carried a per-row currency — a per-row currency
-- with no conversion is an invitation to add rows that cannot be summed.)*
--
-- `unit` is here from day one even though only `per_mtok` is implemented:
-- audio is billed per second and images per image, and a price table without a
-- unit has to be migrated — and every row before it re-interpreted — the day
-- the first TTS bill arrives.
CREATE TABLE prices (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 'alias'          → scope_key is the models.alias (or aux/audio prefixed id)
    -- 'upstream_model' → scope_key is '<upstream_id>:<upstream_model_id>', which
    --                    is what an expose_all passthrough request resolves to.
    scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('alias','upstream_model')),
    scope_key   TEXT NOT NULL,
    unit        TEXT NOT NULL DEFAULT 'per_mtok'
                CHECK (unit IN ('per_mtok','per_second','per_char','per_image','per_request')),
    -- Currency units per 1M tokens. NULL = this dimension is not priced, which
    -- is NOT the same as free: see price_source on request_logs.
    price_in          REAL,
    price_out         REAL,
    price_cache_read  REAL,
    price_cache_write REAL,
    -- 'catalog' rows are refreshed from the upstream's own model list; 'manual'
    -- rows are the owner's and always win (§2.2). Both may exist for one scope.
    source      TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('catalog','manual')),
    note        TEXT,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- One row per (scope, source, unit): a catalog refresh upserts its own row and
-- can never clobber the owner's.
CREATE UNIQUE INDEX idx_prices_scope ON prices (scope_kind, scope_key, source, unit);
