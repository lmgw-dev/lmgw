//! Price resolution: the [`Sheet`] that prices one resolved route, every
//! unit resolved on its own (billable-units design §2.3).

use crate::pricing::{PriceSource, Prices, Sheet, TierGap, TierSet, UnitRate};

use super::*;

impl Snapshot {
    /// The sheet that prices one resolved route: for every unit, the first
    /// usable row of
    ///
    /// 1. the alias, manual;
    /// 2. the alias, catalog;
    /// 3. the upstream model, manual;
    /// 4. the upstream model, catalog.
    ///
    /// The alias is more specific than the upstream model it resolves to, and
    /// within one scope the owner's manual row beats the catalog's. `alias`
    /// is the one that **answered** — a fallback's
    /// ([`Route::priced_alias`]), with its own upstream and model.
    ///
    /// **Per unit, not per scope** (§2.3): a manual alias row for tokens does
    /// not hide a catalog per-request fee of the model beneath it. Dropping a
    /// fee takes a manual `0` for that unit, which says so.
    ///
    /// A local upstream short-circuits first: [`Sheet::local`], whatever rows
    /// exist. A scope with no usable row in any unit gives a sheet that
    /// prices nothing — which is *unknown*, not zero (usage-analytics §2.3).
    ///
    /// **By service tier** (openai egress design §5.5): every step of the
    /// chain is restricted to rows of `tier`, the tier the answer reported
    /// ([`crate::pricing::ServedTier::price_tier`]). `None` is an answer
    /// that named no price tier, which no row prices. A unit with usable rows
    /// at another tier but none at this one sets [`Sheet::tier_gap`], which
    /// leaves the request unpriced (§5.6): nothing falls back to Standard.
    /// A reader asking what a scope costs rather than what one answer cost
    /// passes [`PriceTier::Standard`].
    pub fn sheet_for(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
        tier: Option<PriceTier>,
    ) -> Sheet {
        let Some(rows) = self.sheet_rows(alias, upstream_id, upstream_model, tier) else {
            return Sheet::local();
        };
        let mut sheet = Sheet {
            tier_gap: self.tier_gap(alias, upstream_id, upstream_model, tier, &rows),
            ..Sheet::default()
        };
        for row in rows {
            if row.unit.is_tokens() {
                sheet.tokens = Some(token_prices(row));
            } else {
                let rate = row.price.map(|price| UnitRate {
                    price,
                    source: row.source,
                });
                sheet.set_rate(row.unit, rate);
            }
        }
        sheet
    }

    /// The rows [`Self::sheet_for`] prices one resolved route by: per unit,
    /// in [`PriceUnit::ALL`]'s order, the first usable row of its chain, a
    /// unit with none left out. `None` for a local upstream, which is free
    /// whatever rows exist. For a reader that names the rows, not their sum:
    /// the Decision lab's price line. Rows of `tier` only; `None` selects none
    /// (§5.5).
    pub fn sheet_rows(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
        tier: Option<PriceTier>,
    ) -> Option<Vec<&PriceRow>> {
        // Anything served out of a container on this machine is free, and no
        // price row can make it otherwise. What a local request really costs is
        // GPU time, which is recorded as the llama.cpp timings instead (§2.4).
        if upstream_id.is_some_and(|id| self.is_local_upstream(id)) {
            return None;
        }
        let Some(tier) = tier else {
            return Some(Vec::new());
        };
        Some(
            PriceUnit::ALL
                .into_iter()
                .filter_map(|unit| self.chain_row(alias, upstream_id, upstream_model, unit, tier))
                .collect(),
        )
    }

    /// [`Sheet::tier_gap`] for a sheet resolved at `tier` from `rows`: every
    /// unit `rows` has no row for, but some other tier has, adds that tier
    /// to what is on file. `None` when no unit is missing that way.
    fn tier_gap(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
        tier: Option<PriceTier>,
        rows: &[&PriceRow],
    ) -> Option<TierGap> {
        let mut on_file = TierSet::default();
        let mut missing = Vec::new();
        for unit in PriceUnit::ALL {
            if rows.iter().any(|r| r.unit == unit) {
                continue;
            }
            for other in PriceTier::ALL.into_iter().filter(|t| Some(*t) != tier) {
                if self
                    .chain_row(alias, upstream_id, upstream_model, unit, other)
                    .is_some()
                {
                    on_file.insert(other);
                    if !missing.contains(&unit) {
                        missing.push(unit);
                    }
                }
            }
        }
        (!on_file.is_empty()).then(|| TierGap::new(on_file, missing))
    }

    /// The first usable row of `unit` at `tier` along the resolution chain
    /// ([`Self::sheet_for`]).
    fn chain_row(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
        unit: PriceUnit,
        tier: PriceTier,
    ) -> Option<&PriceRow> {
        let alias_key = alias.to_lowercase();
        let up_key = match (upstream_id, upstream_model) {
            (Some(id), Some(m)) => Some(upstream_scope_key(id, m)),
            _ => None,
        };
        let chain = [
            (
                PriceScope::Alias,
                Some(alias_key.as_str()),
                PriceSource::Manual,
            ),
            (
                PriceScope::Alias,
                Some(alias_key.as_str()),
                PriceSource::Catalog,
            ),
            (
                PriceScope::UpstreamModel,
                up_key.as_deref(),
                PriceSource::Manual,
            ),
            (
                PriceScope::UpstreamModel,
                up_key.as_deref(),
                PriceSource::Catalog,
            ),
        ];
        // Usability is part of *selecting* a row, not a test applied after. An
        // owner who "undoes" a manual override by clearing its fields instead
        // of deleting it would otherwise have an all-`None` row shadow a
        // perfectly good catalog row, and every request on that alias would
        // record `price_source='unknown'` — a silent hole in the spend total
        // that reads as "the catalog stopped publishing".
        chain.iter().find_map(|(scope, key, source)| {
            let key = (*key)?;
            self.prices
                .iter()
                .find(|p| {
                    p.scope_kind == *scope
                        && p.source == *source
                        && p.unit == unit
                        && p.service_tier == tier
                        && p.scope_key.eq_ignore_ascii_case(key)
                })
                .filter(|row| usable(row))
        })
    }

    /// The Standard token sheet alone ([`Self::sheet_for`]'s `tokens`), or
    /// `None` when the scope has no token price at all — which is *not* zero
    /// (usage-analytics §2.3). For a reader that wants tokens alone and asks
    /// what a scope costs, not what one answer cost: the local
    /// counterfactual (§2.5 there).
    pub fn prices_for(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
    ) -> Option<Prices> {
        self.sheet_for(
            alias,
            upstream_id,
            upstream_model,
            Some(PriceTier::Standard),
        )
        .tokens
    }
}

/// A `per_mtok` row needs an input or an output rate; a row in any other
/// unit needs its `price` (§2.2). A row that fails this is no row.
fn usable(row: &PriceRow) -> bool {
    if row.unit.is_tokens() {
        token_prices(row).is_usable()
    } else {
        row.price.is_some()
    }
}

fn token_prices(row: &PriceRow) -> Prices {
    Prices {
        price_in: row.price_in,
        price_out: row.price_out,
        price_cache_read: row.price_cache_read,
        price_cache_write: row.price_cache_write,
        source: row.source,
    }
}

#[cfg(test)]
mod tests;
