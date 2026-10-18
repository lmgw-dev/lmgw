//! `lmgw__prices` and `lmgw__usage`.
//!
//! Both are tool-only: no HTTP route answers them (the dashboard reads
//! `GET /api/usage/prices` and `GET /api/usage/series`, whose shapes are
//! the dashboard's own). A price sheet is the dashboard's [`PriceRowView`],
//! so the tool's rows cannot drift from the Prices page's; what the tools
//! add is the worklist of unpriced models, the usage cell with its cost
//! already formatted beside the micro-units, and a `hint` each.

use serde::{Deserialize, Serialize};

use crate::{PriceRowView, UnpricedModel};

/// `lmgw__prices`: every price sheet on file, and the models that resolve to
/// no price at all, with a hint on how to read them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Prices {
    /// The currency every price is quoted in (Settings).
    pub currency: String,
    /// One row per scope and unit, the rows `GET /api/usage/prices` lists.
    pub sheets: Vec<PriceRowView>,
    /// The worklist for `lmgw__price_set`: every enabled alias, and every
    /// passthrough model the usage rollup has seen, that no unit prices.
    /// Busiest first.
    pub unpriced_models: Vec<UnpricedModel>,
    /// How to read `sheets` and `unpriced_models`, and the units.
    pub hint: String,
}

/// `lmgw__usage`: what was spent, on what, over a window of whole UTC days.
///
/// The call's `to` is an inclusive date; the window it answers is the half-open
/// instant range `[from, to)`, so `to` here is the first instant after the
/// last day asked for. A `from` of `<N>d` is the last N whole days ending
/// with `to` (`to` and the N-1 days before it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Usage {
    /// The window's first instant, inclusive, ISO 8601 UTC:
    /// `2026-09-11T00:00:00Z`.
    pub from: String,
    /// The window's end, **exclusive**, ISO 8601 UTC: the start of the day
    /// after the last day asked for, `2026-10-12T00:00:00Z` for a call with
    /// `to=2026-10-11`.
    pub to: String,
    /// The `class` filter that was applied; absent when the call named none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub class: Option<String>,
    /// The `alias` filter that was applied (trimmed); absent when the call
    /// named none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub alias: Option<String>,
    /// Always `UTC`: the tool has no browser to read a timezone from.
    pub timezone: String,
    pub currency: String,
    /// When this install started recording the billable quantities
    /// (`audio_in_ms`, `chars_in`, `images_out`), RFC 3339 UTC; absent when
    /// the bookkeeping holds no such row. A window before it reads 0 for
    /// those because nothing was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub units_since: Option<String>,
    /// The whole window, ungrouped (`series` is `null`).
    pub totals: UsageCellView,
    /// What `breakdown` is split by, as it was applied (`alias` when the call
    /// named nothing): `alias`, `key`, `class`, `upstream` or `none`.
    pub group_by: String,
    /// One cell per series, the dearest first; the tail past the call's
    /// `limit` is folded into a series named `Other`. Empty for
    /// `group_by=none`.
    pub breakdown: Vec<UsageCellView>,
    /// How to read the cost strings and the measured quantities.
    pub hint: String,
}

/// One cell of `lmgw__usage`: a total or one series of the breakdown.
///
/// Money is both ways: `cost` is the text a person reads, with the unpriced
/// remainder stated beside it, and `cost_micro` the integer micro-units it
/// was formatted from. Latency and decode speed are averaged from the
/// rollup's sums (`avg_*` are `null` when nothing was timed).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageCellView {
    /// The series key (an alias, a key id, a class, an upstream id, or
    /// `Other`); `null` on the totals.
    pub series: Option<String>,
    pub requests: i64,
    /// **Total** input, cache included.
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cached: i64,
    pub tokens_cache_write: i64,
    pub tokens_reasoning: i64,
    /// Input audio, in milliseconds, as measured.
    pub audio_in_ms: i64,
    /// Input characters, as measured.
    pub chars_in: i64,
    /// Generated images.
    pub images_out: i64,
    /// The cost as text with its currency, and its unpriced remainder
    /// stated inline when there is one: `12.84 USD` or
    /// `12.84 USD · 3 unpriced requests (0 tokens, 4.5 min audio)`.
    pub cost: String,
    /// The cost in integer micro-units of the currency.
    pub cost_micro: i64,
    /// What `cost_micro` does not cover: the requests, and the tokens and
    /// measured quantities they held.
    pub unpriced_requests: i64,
    pub unpriced_tokens: i64,
    pub unpriced_audio_in_ms: i64,
    pub unpriced_chars_in: i64,
    pub unpriced_images_out: i64,
    /// Requests that ended in a client or upstream error.
    pub errors: i64,
    /// Requests lmgw refused.
    pub refusals: i64,
    /// Mean time to first byte, in milliseconds; `null` when none was
    /// timed.
    pub avg_ttfb_ms: Option<f64>,
    /// Mean total time, in milliseconds; `null` when none was timed.
    pub avg_total_ms: Option<f64>,
    /// Local decode speed, tokens per second; `null` for a window with no
    /// local decode.
    pub decode_tok_s: Option<f64>,
}
