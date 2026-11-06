-- Service-tier pricing (openai egress design §5.3, decisions 10, 11, 15).
--
-- `request_logs.service_tier`: the `service_tier` an `openai` row's response
-- reported, verbatim (`default`, `flex`, `priority`, …). NULL when none was
-- read: every row of another protocol, every row from before this migration,
-- and an `openai` answer that named no tier.
ALTER TABLE request_logs ADD COLUMN service_tier TEXT;

-- `request_logs.price_note`: why a row's cost is what it is, when that is not
-- plain — a tier with no price on file (§5.6), or an answer that ended before
-- its upstream reported usage (§5.2). NULL otherwise.
ALTER TABLE request_logs ADD COLUMN price_note TEXT;

-- `request_logs.priced_alias`: the alias a row's cost was priced under when it
-- is not `requested_alias` — the fallback alias that answered. NULL otherwise.
-- The Prices page's list of unpriced tiers re-checks a group against the rows
-- the group was priced by, which are the answering alias's (§5.6).
ALTER TABLE request_logs ADD COLUMN priced_alias TEXT;

-- The list of unpriced tiers reads only the rows that carry a note; the index
-- holds only those, so the page does not scan the whole log (§5.6).
CREATE INDEX idx_request_logs_price_note ON request_logs (service_tier)
    WHERE price_note IS NOT NULL;

-- `prices.service_tier`: OpenAI's pricing tier the row is for. Every existing
-- row is `standard` (decision 11), the one tier every other provider has, and
-- the catalog sync writes `standard` only. A new column with its own CHECK, so
-- `ADD COLUMN` does it (SQLite checks it against the existing rows, which all
-- take the default) and `prices` is not rebuilt; nothing can be lost, so no
-- guard stands in front of this file.
ALTER TABLE prices ADD COLUMN service_tier TEXT NOT NULL DEFAULT 'standard'
    CHECK (service_tier IN ('standard','flex','fast','ultrafast','scale'));

-- One row per (scope, source, unit, tier): a flex row stands beside the
-- standard one of the same scope and unit.
DROP INDEX idx_prices_scope;
CREATE UNIQUE INDEX idx_prices_scope
    ON prices (scope_kind, scope_key, source, unit, service_tier);
