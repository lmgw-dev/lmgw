-- What budgets count for a row unpriced only for a hosted-tool fee (openai
-- egress design §6.1): the part lmgw could price, a lower bound.
--
-- A hosted tool's fee (web search, file search, a container session) is in
-- no `usage` and lmgw has no price unit for it yet, so such a row's cost is
-- NULL, never the token part alone. Budgets read cost, so a budgeted client
-- key could name a hosted tool on every request and spend tokens no budget
-- ever saw. The row's cost, its totals and its rollup's `cost_micro` stay
-- unpriced; only the budget spend (`store::key_spend_micro`,
-- `store::total_spend_micro`) adds this column.
--
-- `request_logs.budget_floor_micro`: the known part. NULL for every other
-- row, and for every row from before this migration.
ALTER TABLE request_logs ADD COLUMN budget_floor_micro INTEGER;

-- `usage_hourly.budget_floor_micro`: its hourly sum, kept beside
-- `cost_micro` the way the row keeps it beside `cost_micro`.
ALTER TABLE usage_hourly ADD COLUMN budget_floor_micro INTEGER NOT NULL DEFAULT 0;
