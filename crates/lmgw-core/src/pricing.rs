//! What a request was worth (usage-analytics design §2.2/§2.3).
//!
//! Pure arithmetic over a token count and a price sheet — no IO, no state, so
//! the rule this whole feature stands on is testable in isolation:
//!
//! > A request whose price is not known costs `None`. **Never `0`.**
//!
//! A total over several requests is `None` once any of them is: **never a
//! partial sum** ([`sum_micro`]).
//!
//! A zero for an unknown reads as authoritative and is wrong downward, which is
//! the one failure mode that makes an analytics page worse than no analytics
//! page. Local models are a *different* thing: [`PriceSource::FreeLocal`] is a
//! real zero, because the money really is zero (what a local request costs is
//! GPU time, recorded separately as the llama.cpp timings).
//!
//! **Money is integer micro-units** of the configured currency; floats do not
//! add up. The arithmetic falls out clean: prices are quoted per 1M tokens, so
//!
//! ```text
//! cost_micro = tokens * price_per_mtok
//! ```
//!
//! with no scaling in either direction.
//!
//! **Tokens are one unit among several** (billable-units design §2.1): a
//! scope may also be priced per minute of input audio, per 1M input
//! characters, per generated image and per answered request. Each rate is
//! quoted at the scale providers use, so every part is the same kind of plain
//! product ([`units`]), and [`price_request`] sums the parts under the same
//! rule: one unknown part makes the whole request unknown.

use serde::{Deserialize, Serialize};

mod total;
mod units;

pub use total::{CostTotal, RowCost};
pub use units::{price_request, Quantities, Sheet, UnitRate, UnitRates};

/// Where the numbers that priced a request came from.
///
/// Recorded on the row next to the prices themselves, so "why is this NULL"
/// and "which sheet was this charged against" are answerable a month later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// The upstream's own catalog advertised it (`catalog.rs`).
    Catalog,
    /// The owner entered it; always wins over a catalog row for the same scope.
    Manual,
    /// Runs on our own hardware. A real zero.
    FreeLocal,
    /// No price for this scope, or no usage to apply one to.
    #[default]
    Unknown,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Manual => "manual",
            Self::FreeLocal => "free_local",
            Self::Unknown => "unknown",
        }
    }

    /// Not [`std::str::FromStr`] on purpose: every unrecognised string is
    /// [`Unknown`](Self::Unknown) rather than an error, because a price source
    /// read back from a row written by a future version must degrade to "we do
    /// not know" instead of failing the whole query.
    pub fn parse(s: &str) -> Self {
        match s {
            "catalog" => Self::Catalog,
            "manual" => Self::Manual,
            "free_local" => Self::FreeLocal,
            _ => Self::Unknown,
        }
    }
}

/// A price sheet for one scope, in currency units per 1M tokens.
///
/// Every field is optional because a provider may publish some dimensions and
/// not others: OpenRouter quotes prompt/completion and nothing about caching,
/// Gemini publishes no pricing at all.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Prices {
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub price_cache_write: Option<f64>,
    pub source: PriceSource,
}

impl Prices {
    /// A sheet with at least one usable dimension. A row of all-`None` prices
    /// is indistinguishable from no row at all and is treated as such.
    pub fn is_usable(&self) -> bool {
        self.price_in.is_some() || self.price_out.is_some()
    }

    pub fn free_local() -> Self {
        Self {
            price_in: Some(0.0),
            price_out: Some(0.0),
            price_cache_read: Some(0.0),
            price_cache_write: Some(0.0),
            source: PriceSource::FreeLocal,
        }
    }
}

/// The token counts a request reported, in the canonical IR meaning.
///
/// **`prompt_tokens` is the total input for the turn, cache included.** The
/// egress adapters normalise to that (see `ir::Usage`): OpenAI and Gemini
/// already report it that way, Anthropic reports three disjoint counters that
/// get summed. `cached_in` and `cache_write` are therefore *subsets* of
/// `prompt_tokens`, never additions to it.
///
/// `completion` is the total output, reasoning included (the Gemini adapter adds
/// the thoughts Google reports beside the candidates). `reasoning` is a subset
/// of it and informational only — pricing it again would double-charge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsage {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub cached_in: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

impl TokenUsage {
    /// Input tokens billed at the plain input rate: the total minus the two
    /// cache subsets. Saturating, because a provider that reports a cache
    /// count larger than its own total is reporting nonsense and the answer to
    /// nonsense is zero, not a wrapped u64.
    pub fn plain_in(&self) -> u64 {
        self.prompt
            .unwrap_or(0)
            .saturating_sub(self.cached_in.unwrap_or(0))
            .saturating_sub(self.cache_write.unwrap_or(0))
    }
}

/// What a request cost, and under which numbers.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cost {
    /// `None` = unpriced. Callers must carry that forward as NULL and count it
    /// in the unpriced remainder — never coerce it to 0.
    pub total_micro: Option<i64>,
    pub in_micro: Option<i64>,
    pub out_micro: Option<i64>,
    pub source: PriceSource,
    /// The prices **as used**, snapshotted onto the row. Where a cache rate was
    /// missing and the plain input rate stood in for it, that substitution is
    /// visible here as two equal numbers — the row records what happened
    /// rather than a flag that says it happened.
    pub used: Prices,
    /// The part of `total_micro` priced in units other than tokens
    /// (`request_logs.cost_units_micro`). `None` when the sheet has no rate in
    /// any of them, and with `total_micro` when any part is unknown.
    pub units_micro: Option<i64>,
    /// The non-token rates **as used**, snapshotted like [`Self::used`]: each
    /// `None` where the sheet has no rate in that unit
    /// (`request_logs.price_per_*`).
    pub used_units: UnitRates,
}

impl Cost {
    /// Unpriced: no sheet, or nothing to apply one to.
    pub fn unknown() -> Self {
        Self {
            source: PriceSource::Unknown,
            ..Default::default()
        }
    }
}

/// Price one request's **tokens** alone: the token part of [`price_request`],
/// and the whole of it for a sheet that has only a token row (billable-units
/// design §3.2, decision 1). Also what a counterfactual that wants tokens
/// alone calls (`/api/usage/local`'s reference price).
///
/// `prices` is the resolved token sheet for the alias (manual over catalog),
/// or `None` when the scope has no token price at all.
pub fn price_tokens(usage: &TokenUsage, prices: Option<&Prices>) -> Cost {
    let Some(p) = prices.filter(|p| p.is_usable()) else {
        return Cost::unknown();
    };

    // A price with no usage to apply it to is still unpriced: we know the rate
    // and not the quantity. The row keeps `cost_micro` NULL and lands in the
    // unpriced remainder, which is exactly what "not included in this total"
    // means, whatever the reason.
    if usage.prompt.is_none() && usage.completion.is_none() {
        return Cost {
            source: PriceSource::Unknown,
            used: *p,
            ..Default::default()
        };
    }

    // A provider that bills caching but publishes no cache rate bills those
    // tokens at the plain input rate; record that as the rate actually used.
    let rate_in = p.price_in.unwrap_or(0.0);
    let rate_cache_read = p.price_cache_read.unwrap_or(rate_in);
    let rate_cache_write = p.price_cache_write.unwrap_or(rate_in);
    let rate_out = p.price_out.unwrap_or(0.0);

    let in_micro = (usage.plain_in() as f64) * rate_in
        + (usage.cached_in.unwrap_or(0) as f64) * rate_cache_read
        + (usage.cache_write.unwrap_or(0) as f64) * rate_cache_write;
    let out_micro = (usage.completion.unwrap_or(0) as f64) * rate_out;

    let in_micro = in_micro.round() as i64;
    let out_micro = out_micro.round() as i64;

    Cost {
        total_micro: Some(in_micro + out_micro),
        in_micro: Some(in_micro),
        out_micro: Some(out_micro),
        source: p.source,
        used: Prices {
            price_in: Some(rate_in),
            price_out: Some(rate_out),
            price_cache_read: Some(rate_cache_read),
            price_cache_write: Some(rate_cache_write),
            source: p.source,
        },
        units_micro: None,
        used_units: UnitRates::default(),
    }
}

/// Two costs added, for a total over several requests: `None` once either is.
///
/// A total that skips the requests nobody could price reads as the whole
/// and is wrong downward, the same failure as a `0` for an unknown. Who
/// keeps a running total starts it at its first request's cost, not at a
/// `Some(0)` or a `None` of its own.
pub fn sum_micro(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    Some(a?.saturating_add(b?))
}

/// Format micro-units as a plain decimal amount (no currency symbol — the
/// symbol is a display setting, and this is also what the CSV export writes).
pub fn micro_to_units(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

/// Catalog prices arrive per *token* (the Kilo/OpenRouter shape quotes
/// `"0.000003"`); every price stored and displayed here is per 1M tokens.
pub fn per_token_to_per_mtok(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Today's signature, over the new [`super::price_request`] with a
    /// token-only sheet: every test below predates billable units and runs
    /// unchanged against it (billable-units design §10).
    fn price_request(u: &TokenUsage, p: Option<&Prices>) -> Cost {
        super::price_request(u, &Quantities::default(), &Sheet::tokens(p.copied()))
    }

    fn sheet() -> Prices {
        // Claude-shaped: 3 / 15 per Mtok, cache read at 0.1x, write at 1.25x.
        Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            price_cache_read: Some(0.3),
            price_cache_write: Some(3.75),
            source: PriceSource::Catalog,
        }
    }

    #[test]
    fn plain_request_costs_tokens_times_rate() {
        let u = TokenUsage {
            prompt: Some(1_000_000),
            completion: Some(100_000),
            ..Default::default()
        };
        let c = price_request(&u, Some(&sheet()));
        // 1M in at 3.0 = 3_000_000 micro = 3.00; 100k out at 15.0 = 1.50.
        assert_eq!(c.in_micro, Some(3_000_000));
        assert_eq!(c.out_micro, Some(1_500_000));
        assert_eq!(c.total_micro, Some(4_500_000));
        assert_eq!(micro_to_units(c.total_micro.unwrap()), 4.5);
        assert_eq!(c.source, PriceSource::Catalog);
    }

    #[test]
    fn cache_subsets_are_billed_at_their_own_rates_and_never_double_counted() {
        // 1M total input of which 800k was a cache read and 100k a cache write:
        // only 100k is billed at the plain rate.
        let u = TokenUsage {
            prompt: Some(1_000_000),
            completion: Some(0),
            cached_in: Some(800_000),
            cache_write: Some(100_000),
            ..Default::default()
        };
        assert_eq!(u.plain_in(), 100_000);
        let c = price_request(&u, Some(&sheet()));
        // 100k*3 + 800k*0.3 + 100k*3.75 = 300_000 + 240_000 + 375_000
        assert_eq!(c.in_micro, Some(915_000));
    }

    #[test]
    fn missing_cache_rates_fall_back_to_the_input_rate_and_the_row_says_so() {
        let p = Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            price_cache_read: None,
            price_cache_write: None,
            source: PriceSource::Catalog,
        };
        let u = TokenUsage {
            prompt: Some(1_000_000),
            completion: Some(0),
            cached_in: Some(500_000),
            ..Default::default()
        };
        let c = price_request(&u, Some(&p));
        // Everything at 3.0 — no discount invented.
        assert_eq!(c.in_micro, Some(3_000_000));
        // The substitution is visible in the snapshot: read rate == input rate.
        assert_eq!(c.used.price_cache_read, Some(3.0));
    }

    #[test]
    fn unknown_price_is_none_not_zero() {
        let u = TokenUsage {
            prompt: Some(1_000_000),
            completion: Some(100_000),
            ..Default::default()
        };
        let c = price_request(&u, None);
        assert_eq!(
            c.total_micro, None,
            "an unknown price must never price as 0"
        );
        assert_eq!(c.in_micro, None);
        assert_eq!(c.out_micro, None);
        assert_eq!(c.source, PriceSource::Unknown);
    }

    #[test]
    fn an_all_none_sheet_is_no_sheet() {
        let u = TokenUsage {
            prompt: Some(10),
            ..Default::default()
        };
        let c = price_request(&u, Some(&Prices::default()));
        assert_eq!(c.total_micro, None);
        assert_eq!(c.source, PriceSource::Unknown);
    }

    #[test]
    fn known_price_without_usage_is_unpriced_too() {
        let c = price_request(&TokenUsage::default(), Some(&sheet()));
        assert_eq!(c.total_micro, None);
        assert_eq!(c.source, PriceSource::Unknown);
        // but the sheet that would have applied is still recorded
        assert_eq!(c.used.price_in, Some(3.0));
    }

    #[test]
    fn local_is_a_real_zero() {
        let u = TokenUsage {
            prompt: Some(5_000_000),
            completion: Some(1_000_000),
            ..Default::default()
        };
        let c = price_request(&u, Some(&Prices::free_local()));
        assert_eq!(c.total_micro, Some(0));
        assert_eq!(c.source, PriceSource::FreeLocal);
    }

    #[test]
    fn nonsense_cache_counts_saturate_rather_than_wrap() {
        let u = TokenUsage {
            prompt: Some(10),
            cached_in: Some(9_999),
            ..Default::default()
        };
        assert_eq!(u.plain_in(), 0);
    }

    #[test]
    fn reasoning_tokens_are_not_billed_twice() {
        // They are already inside completion_tokens once the egress adapter
        // has normalised the provider's report.
        let u = TokenUsage {
            prompt: Some(0),
            completion: Some(100_000),
            reasoning: Some(90_000),
            ..Default::default()
        };
        let c = price_request(&u, Some(&sheet()));
        assert_eq!(c.out_micro, Some(1_500_000));
    }

    #[test]
    fn catalog_per_token_prices_convert_to_per_mtok() {
        assert_eq!(per_token_to_per_mtok(0.000003), 3.0);
    }

    #[test]
    fn a_sum_with_an_unknown_part_is_unknown_never_partial() {
        assert_eq!(sum_micro(Some(2), Some(3)), Some(5));
        assert_eq!(sum_micro(Some(0), Some(0)), Some(0), "free is a real 0");
        assert_eq!(sum_micro(Some(2), None), None);
        assert_eq!(sum_micro(None, Some(3)), None);
        assert_eq!(sum_micro(None, None), None);
    }
}
