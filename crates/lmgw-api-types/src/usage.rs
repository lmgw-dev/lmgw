//! Usage analytics, cost & policy
//! (docs/design/2026-09-18-usage-analytics-cost-policy-design.md)

use serde::{Deserialize, Serialize};

/// One (bucket, series) cell. Mirrors `store::UsageCell`.
///
/// Money is integer **micro-units** of the configured currency everywhere on
/// this plane; a float that has been through JSON twice does not add up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageCell {
    /// Local-time bucket label: `2026-09-18`, `2026-09-18T14`, `2026-W38`, `2026-09`.
    pub bucket: String,
    /// The series key this cell belongs to (`""` when ungrouped).
    pub series: String,
    pub requests: i64,
    /// **Total** input, cache included — the same meaning `ir::Usage`'s
    /// `prompt_tokens` carries, so fresh (uncached) input is
    /// `tokens_in - tokens_cached - tokens_cache_write` and never a fourth
    /// stored number that could drift from the other three.
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Input served from the provider's prompt cache, billed at the cheap
    /// read rate.
    pub tokens_cached: i64,
    /// Input written *into* the prompt cache — Anthropic's 1.25x tier, the
    /// dearest input a window can contain. Zero everywhere else, because no
    /// other provider bills a cache write.
    pub tokens_cache_write: i64,
    pub tokens_reasoning: i64,
    pub cost_micro: i64,
    /// What `cost_micro` does **not** cover. Every total that is displayed is
    /// obliged to show this beside it (design §2.3) — a cost figure with a
    /// silent hole in it is worse than no cost figure.
    pub cost_unknown_requests: i64,
    pub cost_unknown_tokens: i64,
    pub errors: i64,
    pub refusals: i64,
    pub ttfb_sum: i64,
    pub ttfb_count: i64,
    pub total_sum: i64,
    pub total_count: i64,
    /// Local-model throughput as a ratio pair: tok/s is Σtokens / Σms, never a
    /// mean of per-request rates.
    pub decode_tokens: i64,
    pub decode_ms: f64,
    pub prefill_ms: f64,
    pub prompt_n: i64,
    pub cache_n: i64,
    pub draft_n: i64,
    pub draft_accepted: i64,
}

/// Identity of one series, with the colour slot it wears everywhere.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SeriesMeta {
    pub key: String,
    /// What to print — a key id resolves to its name here, an upstream id to
    /// its name, an alias is itself.
    pub label: String,
    /// Chart colour slot 0..5, or `null` for the neutral "Other" bucket.
    ///
    /// Assigned from the entity's position in the gateway's own sorted list of
    /// aliases, **not** from its rank in this response: a filter that drops a
    /// series must not repaint the survivors, or a reader who learned "qwen is
    /// teal" is being lied to.
    pub slot: Option<u8>,
    /// True for a locally-served model — it can never appear in a spend chart,
    /// because its money cost is a real zero (design §2.4).
    pub local: bool,
    /// On the folded "Other" series only: how many entities it sums, so a
    /// legend can say "Other (49 aliases)" instead of hiding the tail's size.
    /// `null` on every named series.
    pub folded: Option<u32>,
}

/// `GET /api/usage/series`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageSeriesResponse {
    /// Every bucket label in the window, in order, including empty ones — a
    /// chart that silently drops quiet days draws a lie about its own x-axis.
    pub buckets: Vec<String>,
    pub series: Vec<SeriesMeta>,
    pub cells: Vec<UsageCell>,
    pub totals: UsageCell,
    /// Totals over the immediately preceding window of the same length, for the
    /// tiles' "vs previous" delta. `None` when that window predates the data.
    pub previous: Option<UsageCell>,
    pub currency: String,
    /// p50 / p95 of total latency and TTFB over the window, in ms. `None` where
    /// the window holds no successful request: an empty window has no p95, and
    /// 0 would draw a gateway that got infinitely fast.
    pub p50_total_ms: Option<f64>,
    pub p95_total_ms: Option<f64>,
    pub p50_ttfb_ms: Option<f64>,
    pub p95_ttfb_ms: Option<f64>,
    /// The same four percentiles **per bucket**, aligned with `buckets`, so a
    /// latency chart plots a real p50/p95 line rather than a mean. Dividing the
    /// rollup's sums would give a mean, and one 30-second outlier drags a mean
    /// four times above the typical request.
    pub latency: Vec<LatencyPoint>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LatencyPoint {
    pub bucket: String,
    pub p50_total_ms: Option<f64>,
    pub p95_total_ms: Option<f64>,
    pub p50_ttfb_ms: Option<f64>,
    pub p95_ttfb_ms: Option<f64>,
}

/// `GET /api/usage/top` — ranked totals for one dimension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageTopResponse {
    pub rows: Vec<UsageCell>,
    pub series: Vec<SeriesMeta>,
    pub currency: String,
}

/// `GET /api/usage/heat` — weekday (0 = Sunday) × hour request counts, local time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageHeatResponse {
    pub cells: Vec<HeatCell>,
    pub max: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HeatCell {
    pub dow: i64,
    pub hour: i64,
    pub requests: i64,
}

/// `GET /api/usage/local` — the local/cloud split and the counterfactual.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageLocalResponse {
    pub local_tokens: i64,
    pub cloud_tokens: i64,
    /// What the local tokens **would** have cost at `reference_alias`'s price.
    /// Not a saving — nobody would have run all of it there (design §2.5).
    /// `None` when no reference alias is configured, which the page must say
    /// rather than quietly picking one.
    pub counterfactual_micro: Option<i64>,
    pub reference_alias: Option<String>,
    pub currency: String,
    /// Throughput and cache facts for the local models in the window.
    pub decode_tok_s: Option<f64>,
    pub kv_reuse: Option<f64>,
    pub draft_acceptance: Option<f64>,
}

/// One key on the Keys & policy table. `GET /api/usage/keys`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeyRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// `key` | `internal` | `agent` | `owner` — an internal identity is
    /// budgetable but can never authenticate anything; an owner key holds
    /// every capability (principals §3.1).
    pub kind: String,
    pub scope_mode: String,
    pub scope_patterns: String,
    /// `all` | `allow` | `deny` over exposed MCP tool names — which tools the
    /// key sees on `/mcp` and may attach in a `/v1/responses` run. Only a
    /// client key's is read: an agent's tools are its manifest's, and an owner
    /// key is not scoped.
    pub tool_scope_mode: String,
    pub tool_scope_patterns: String,
    pub budget_micro: i64,
    pub budget_period: String,
    pub rpm_limit: i64,
    pub tpm_limit: i64,
    pub concurrency_limit: i64,
    pub expires_at: Option<String>,
    pub note: String,
    /// Spend so far in the current budget period.
    pub spent_micro: i64,
    /// What that figure does **not** cover — the same obligation every other
    /// money total on this plane carries. Without it the per-key meter would be
    /// the one number on the page that cannot state its own remainder.
    pub spent_unknown_requests: i64,
    pub spent_unknown_tokens: i64,
    pub requests: i64,
    pub last_used: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeysResponse {
    pub keys: Vec<KeyRow>,
    pub currency: String,
    /// The owner's global budget and what has gone against it this period.
    pub global_budget_micro: i64,
    pub global_spent_micro: i64,
    /// `day` | `month` | `total` — what "this period" means, so a projection
    /// to the period's end can aim at the right horizon instead of assuming a
    /// calendar month.
    pub global_budget_period: String,
    pub global_spent_unknown_requests: i64,
}

/// One price sheet. `GET /api/usage/prices`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PriceRowView {
    pub id: i64,
    pub scope_kind: String,
    pub scope_key: String,
    pub unit: String,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub price_cache_write: Option<f64>,
    pub source: String,
    pub note: Option<String>,
    pub updated_at: String,
}

/// One model that resolves to no price at all — an entry on the "why is my
/// spend incomplete" worklist.
///
/// `requests` is what it has already cost that nobody can name, from the
/// rollup. A configured alias that has never been called is still listed, at
/// `0`: it is about to be the same problem.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UnpricedModel {
    pub name: String,
    pub requests: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PricesResponse {
    pub prices: Vec<PriceRowView>,
    pub currency: String,
    /// Every model the gateway serves that no price sheet covers — **including
    /// the passthrough models an `expose_all` upstream serves**, which are the
    /// ones an owner with no configured aliases actually uses. Ordered by the
    /// requests each has already spent unpriced.
    pub unpriced_models: Vec<UnpricedModel>,
}

/// `GET /api/usage/errors` — refusals and errors by kind.
///
/// Read from the **raw** request rows, so its range is the log-retention
/// window rather than the full rollup history. The response says so; a chart
/// that silently showed 30 days where every other chart on the page shows 12
/// months would read as "the errors stopped".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageErrorsResponse {
    pub buckets: Vec<String>,
    /// One entry per distinct kind, ordered by total descending.
    pub kinds: Vec<ErrorKindSeries>,
    /// How far back the raw rows actually go — the honest x-axis of this card.
    pub retention_days: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorKindSeries {
    pub kind: String,
    /// True when this kind is lmgw refusing, rather than an upstream or a
    /// client failing — the two belong in different colours.
    pub refusal: bool,
    pub total: i64,
    /// Aligned with `buckets`.
    pub counts: Vec<i64>,
}
