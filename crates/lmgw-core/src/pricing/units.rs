//! Billable units (billable-units design §3): what a request processed
//! besides tokens ([`Quantities`]), what one scope costs in every unit
//! ([`Sheet`]), and the one function every row and run meter prices with
//! ([`price_request`]).
//!
//! Each unit's rate is quoted at the scale providers use (§3.3), so a part
//! is the same kind of plain product tokens are, in integer micro-units:
//!
//! ```text
//! per_mtok          tokens × rate                (today's token part)
//! per_mchar         chars_in × rate
//! per_audio_minute  audio_in_ms × rate × 10⁶ / 60 000
//! per_image         images_out × rate × 10⁶
//! per_request       requests × rate × 10⁶
//! ```
//!
//! Each part is rounded once, and the parts are summed as integers.

use crate::config::PriceUnit;

use super::{price_tokens, Cost, PriceSource, Prices, TokenUsage};

/// What a request processed besides tokens (billable-units design §3.1,
/// §4.1). Each `None` unless lmgw measured it or the provider reported it:
/// **never estimated** — no duration from a byte count, no characters from a
/// token count, no image count from the request's `n`. `None` is not 0: on a
/// scope priced in that unit it makes the row unpriced (§3.2 rule 4).
///
/// Built by the route that measures (`Quantities { chars_in: Some(n),
/// ..Default::default() }`) and handed to the row writer in
/// `LogParams::quantities` / `InProcessLog::quantities`; the writer fills
/// `requests` when the caller left it `None` (§4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quantities {
    /// Duration of the **input** audio, in milliseconds (`per_audio_minute`).
    /// A provider's `seconds` converts by rounding (§4.1).
    pub audio_in_ms: Option<u64>,
    /// Characters of **input** text as sent, Unicode scalar values
    /// (`per_mchar`).
    pub chars_in: Option<u64>,
    /// Images in the answer (`per_image`): generated images only.
    pub images_out: Option<u64>,
    /// Upstream requests answered for this row (`per_request`): 1 for most
    /// rows, the answered clause count for a synthesis row (§4.5). No column
    /// stores it; the fee lands in `cost_units_micro`.
    pub requests: Option<u64>,
}

impl Quantities {
    /// One answered upstream request and nothing measured: what a relay
    /// hands its row writer when it knows the upstream answered (its 2xx
    /// headers arrived) on a row the writer's default would leave `None` — a
    /// stream its client or consumer stopped mid-way is a `canceled` row
    /// (§4.5).
    pub fn answered() -> Self {
        Self {
            requests: Some(1),
            ..Default::default()
        }
    }

    /// `self` — what the provider reported — with every quantity it lacks
    /// taken from `measured`, lmgw's own count of the same thing. Reported
    /// wins: the provider bills on its own figure, and the row explains a
    /// bill (§4.1).
    pub fn over(self, measured: Quantities) -> Self {
        Self {
            audio_in_ms: self.audio_in_ms.or(measured.audio_in_ms),
            chars_in: self.chars_in.or(measured.chars_in),
            images_out: self.images_out.or(measured.images_out),
            requests: self.requests.or(measured.requests),
        }
    }
}

/// One non-token unit's rate on a [`Sheet`], per the unit's scale
/// ([`PriceUnit::scale`]), and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitRate {
    pub price: f64,
    pub source: PriceSource,
}

/// The non-token rates a [`Cost`] used, snapshotted onto the row's
/// `price_per_*` columns (§3.4). `None` where the sheet had no rate.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UnitRates {
    pub audio_minute: Option<f64>,
    pub mchar: Option<f64>,
    pub image: Option<f64>,
    pub request: Option<f64>,
}

impl UnitRates {
    /// The rate used for `unit`; always `None` for [`PriceUnit::PerMtok`],
    /// whose four rates are [`Cost::used`].
    pub fn get(&self, unit: PriceUnit) -> Option<f64> {
        match unit {
            PriceUnit::PerMtok => None,
            PriceUnit::PerAudioMinute => self.audio_minute,
            PriceUnit::PerMchar => self.mchar,
            PriceUnit::PerImage => self.image,
            PriceUnit::PerRequest => self.request,
        }
    }

    fn set(&mut self, unit: PriceUnit, price: f64) {
        let slot = match unit {
            PriceUnit::PerMtok => return,
            PriceUnit::PerAudioMinute => &mut self.audio_minute,
            PriceUnit::PerMchar => &mut self.mchar,
            PriceUnit::PerImage => &mut self.image,
            PriceUnit::PerRequest => &mut self.request,
        };
        *slot = Some(price);
    }
}

/// Everything that prices one scope, each unit resolved on its own (§2.3):
/// [`Snapshot::sheet_for`](crate::config::Snapshot::sheet_for)'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sheet {
    /// Runs on our own hardware: a real 0 whatever was processed (§3.2
    /// rule 1, Q1).
    pub local: bool,
    /// The token sheet, unchanged from before units ([`Prices`]).
    pub tokens: Option<Prices>,
    pub audio_minute: Option<UnitRate>,
    pub mchar: Option<UnitRate>,
    pub image: Option<UnitRate>,
    pub request: Option<UnitRate>,
}

impl Sheet {
    /// A local scope's sheet: free, and no rows. `tokens` is
    /// [`Prices::free_local`] so a token-only reader
    /// ([`Snapshot::prices_for`](crate::config::Snapshot::prices_for)) sees
    /// what it always saw.
    pub fn local() -> Self {
        Self {
            local: true,
            tokens: Some(Prices::free_local()),
            ..Default::default()
        }
    }

    /// A sheet with a token row and nothing else.
    pub fn tokens(prices: Option<Prices>) -> Self {
        Self {
            tokens: prices,
            ..Default::default()
        }
    }

    /// The rate for one non-token unit; always `None` for
    /// [`PriceUnit::PerMtok`], whose rates are [`Self::tokens`].
    pub fn rate(&self, unit: PriceUnit) -> Option<UnitRate> {
        match unit {
            PriceUnit::PerMtok => None,
            PriceUnit::PerAudioMinute => self.audio_minute,
            PriceUnit::PerMchar => self.mchar,
            PriceUnit::PerImage => self.image,
            PriceUnit::PerRequest => self.request,
        }
    }

    /// Set one non-token unit's rate; a no-op for [`PriceUnit::PerMtok`].
    pub fn set_rate(&mut self, unit: PriceUnit, rate: Option<UnitRate>) {
        let slot = match unit {
            PriceUnit::PerMtok => return,
            PriceUnit::PerAudioMinute => &mut self.audio_minute,
            PriceUnit::PerMchar => &mut self.mchar,
            PriceUnit::PerImage => &mut self.image,
            PriceUnit::PerRequest => &mut self.request,
        };
        *slot = rate;
    }

    /// Whether anything prices this scope: it is local, or at least one unit
    /// has a usable rate. A scope that is not is unpriced, which is what the
    /// unpriced-model worklist lists (§8.2).
    pub fn is_priced(&self) -> bool {
        self.local
            || self.tokens.is_some_and(|p| p.is_usable())
            || PriceUnit::ALL.iter().any(|u| self.rate(*u).is_some())
    }

    /// Each non-token unit this sheet has a rate for, with the request's
    /// quantity of it.
    fn unit_rows(self, q: &Quantities) -> impl Iterator<Item = (PriceUnit, Option<u64>, UnitRate)> {
        [
            (PriceUnit::PerAudioMinute, q.audio_in_ms),
            (PriceUnit::PerMchar, q.chars_in),
            (PriceUnit::PerImage, q.images_out),
            (PriceUnit::PerRequest, q.requests),
        ]
        .into_iter()
        .filter_map(move |(unit, n)| self.rate(unit).map(|r| (unit, n, r)))
    }
}

/// One unit's part in micro-units: `quantity × rate × scale`, rounded once
/// (§3.3). The scale is the one providers quote, so tokens and characters
/// (per 1M) need no constant at all.
///
/// [`PriceUnit::part_micro`], the one the dashboard explains a row with.
pub(super) fn part_micro(unit: PriceUnit, quantity: u64, rate: f64) -> i64 {
    unit.part_micro(quantity, rate)
}

/// Price one request over every unit (billable-units design §3.2):
///
/// 1. a **local** sheet is a real 0, whatever was processed (Q1);
/// 2. a sheet with no usable rate in any unit is unpriced;
/// 3. tokens, when the sheet has a token row, are priced by
///    [`price_tokens`], unchanged — with neither prompt nor completion
///    reported, the token part is unknown;
/// 4. each other unit with a rate gives its [`part_micro`], or is unknown
///    when its quantity is `None` — except at a rate of 0, which is a known
///    0 whatever the quantity;
/// 5. **any part unknown → the whole request unknown**: NULL, never 0, never
///    a partial sum. The rates that would have applied are still snapshotted;
/// 6. otherwise the total is the integer sum of the parts.
///
/// The source is `manual` if any rate used was manual; otherwise the token
/// row's, or `catalog` for a sheet without one. A sheet with only a token row
/// gives exactly [`price_tokens`]'s answer, whatever `q` carries.
pub fn price_request(tokens: &TokenUsage, q: &Quantities, sheet: &Sheet) -> Cost {
    if sheet.local {
        return Cost {
            total_micro: Some(0),
            in_micro: Some(0),
            out_micro: Some(0),
            source: PriceSource::FreeLocal,
            used: Prices::free_local(),
            units_micro: None,
            used_units: UnitRates::default(),
        };
    }

    let token_sheet = sheet.tokens.filter(Prices::is_usable);
    let token_part = token_sheet.map(|p| price_tokens(tokens, Some(&p)));

    let mut any_unit = false;
    let mut units_known = true;
    let mut units_micro = 0i64;
    let mut used_units = UnitRates::default();
    let mut manual = token_sheet.is_some_and(|p| p.source == PriceSource::Manual);
    for (unit, quantity, rate) in sheet.unit_rows(q) {
        any_unit = true;
        used_units.set(unit, rate.price);
        manual |= rate.source == PriceSource::Manual;
        match quantity {
            Some(n) => units_micro = units_micro.saturating_add(part_micro(unit, n, rate.price)),
            // 0 × anything is a known 0, not an estimate: a manual 0 drops a
            // fee (§2.3) without making a row of unknown quantity unpriced.
            None if rate.price == 0.0 => {}
            None => units_known = false,
        }
    }

    let Some(token_part) = token_part.or(any_unit.then(Cost::default)) else {
        return Cost::unknown();
    };
    // A sheet without a token row has no token part to be unknown.
    let tokens_known = token_sheet.is_none() || token_part.total_micro.is_some();
    if !(tokens_known && units_known) {
        return Cost {
            source: PriceSource::Unknown,
            used: token_part.used,
            used_units,
            ..Default::default()
        };
    }

    let source = if manual {
        PriceSource::Manual
    } else {
        token_sheet.map_or(PriceSource::Catalog, |p| p.source)
    };
    let units_micro = any_unit.then_some(units_micro);
    Cost {
        total_micro: Some(
            token_part
                .total_micro
                .unwrap_or(0)
                .saturating_add(units_micro.unwrap_or(0)),
        ),
        in_micro: token_part.in_micro,
        out_micro: token_part.out_micro,
        source,
        used: token_part.used,
        units_micro,
        used_units,
    }
}

#[cfg(test)]
mod tests;
