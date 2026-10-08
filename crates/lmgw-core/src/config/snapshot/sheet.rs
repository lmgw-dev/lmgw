//! Price resolution: the [`Sheet`] that prices one resolved route, every
//! unit resolved on its own (billable-units design §2.3).

use crate::pricing::{PriceSource, Prices, Sheet, UnitRate};

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
    pub fn sheet_for(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
    ) -> Sheet {
        // Anything served out of a container on this machine is free, and no
        // price row can make it otherwise. What a local request really costs is
        // GPU time, which is recorded as the llama.cpp timings instead (§2.4).
        if upstream_id.is_some_and(|id| self.is_local_upstream(id)) {
            return Sheet::local();
        }

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
        let resolve = |unit: PriceUnit| {
            chain.iter().find_map(|(scope, key, source)| {
                let key = (*key)?;
                self.prices
                    .iter()
                    .find(|p| {
                        p.scope_kind == *scope
                            && p.source == *source
                            && p.unit == unit
                            && p.scope_key.eq_ignore_ascii_case(key)
                    })
                    .filter(|row| usable(row))
            })
        };

        let mut sheet = Sheet {
            tokens: resolve(PriceUnit::PerMtok).map(token_prices),
            ..Default::default()
        };
        for unit in PriceUnit::ALL.into_iter().filter(|u| !u.is_tokens()) {
            let rate = resolve(unit).and_then(|row| {
                row.price.map(|price| UnitRate {
                    price,
                    source: row.source,
                })
            });
            sheet.set_rate(unit, rate);
        }
        sheet
    }

    /// The token sheet alone ([`Self::sheet_for`]'s `tokens`), or `None`
    /// when the scope has no token price at all — which is *not* zero
    /// (usage-analytics §2.3). For a reader that wants tokens alone: the
    /// local counterfactual (§2.5 there).
    pub fn prices_for(
        &self,
        alias: &str,
        upstream_id: Option<i64>,
        upstream_model: Option<&str>,
    ) -> Option<Prices> {
        self.sheet_for(alias, upstream_id, upstream_model).tokens
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
