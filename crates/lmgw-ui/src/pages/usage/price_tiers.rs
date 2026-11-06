//! OpenAI service tiers on the Prices page (openai egress design §5.6,
//! §5.7): the pinned band of models that answered at a tier they have no
//! price row for, each with the action that writes one.
//!
//! That traffic counts toward no budget until a row of its tier exists
//! (decision 15), so it sits with the unpriced models, not in a footnote.

use leptos::prelude::*;
use lmgw_api_types::{PriceUnit, UnpricedTier};

use crate::fmt::grouped;

/// One model × tier with no price row, as its pinned row prints it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TierGapLine {
    pub name: String,
    pub scope_kind: String,
    pub scope_key: String,
    /// The tier's stored spelling (`flex`), and its label (`Flex`).
    pub tier: String,
    pub tier_label: String,
    /// What the answers reported when it names no price tier (`auto`), empty
    /// when they named none or `tier` is set.
    pub reported: String,
    /// The units to price, as the gateway spells them, in its order.
    pub missing: Vec<String>,
    pub requests: i64,
    /// The first unpriced request's time, as the log wrote it.
    pub since: String,
}

impl TierGapLine {
    /// The answers named no tier a row can price: nothing to enter.
    fn no_tier(&self) -> bool {
        self.tier.is_empty()
    }

    /// The tier cell: the price tier, or that none was reported (and what
    /// was, when the answers said `auto`).
    fn tier_text(&self) -> String {
        if !self.no_tier() {
            self.tier_label.clone()
        } else if self.reported.is_empty() {
            "no tier reported".to_string()
        } else {
            format!("no tier reported ({})", self.reported)
        }
    }

    /// The unit cell: what the row to enter has to price.
    fn units_text(&self) -> String {
        self.missing
            .iter()
            .map(|u| PriceUnit::parse(u).map_or_else(|| u.clone(), |u| u.label().to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn of(u: &UnpricedTier) -> Self {
        Self {
            name: u.name.clone(),
            scope_kind: u.scope_kind.clone(),
            scope_key: u.scope_key.clone(),
            tier: u.service_tier.clone(),
            tier_label: super::price_units::tier_label(&u.service_tier),
            reported: u.reported.clone(),
            missing: u.missing_units.clone(),
            requests: u.requests,
            since: u.since.clone(),
        }
    }
}

/// What the band's head says: the requests left unpriced, and why that
/// matters.
pub(super) fn band_meta(lines: &[TierGapLine]) -> String {
    let reqs: i64 = lines.iter().map(|l| l.requests.max(0)).sum();
    format!(
        "{} req answered at a tier with no price · counted toward no budget until one is set",
        grouped(reqs as u64)
    )
}

/// A model that answered at a tier it has no row for: the tier, the
/// requests since the first, and the action that prices it at that tier.
/// The cells line up with the table's columns (model, unit, tier, …).
#[component]
pub(super) fn TierGapRow(
    g: TierGapLine,
    /// Opens the editor for `(scope_kind, scope_key, tier, unit)`: the first
    /// unit the tier is missing.
    price: Callback<(String, String, String, String)>,
) -> impl IntoView {
    let unit = g
        .missing
        .first()
        .cloned()
        .unwrap_or_else(|| PriceUnit::PerMtok.as_str().to_string());
    let target = (
        g.scope_kind.clone(),
        g.scope_key.clone(),
        g.tier.clone(),
        unit,
    );
    let tier_text = g.tier_text();
    let units_text = g.units_text();
    let no_tier = g.no_tier();
    let since = g.since.get(..10).unwrap_or(&g.since).to_string();
    let tip = if no_tier {
        format!(
            "{} answered with no service tier a price row can be entered for ({}) — \
             unpriced since {}",
            g.name, tier_text, g.since
        )
    } else {
        format!(
            "{} answered at service tier {}, which has no price row for {} in {} — \
             unpriced since {}",
            g.name, g.tier, g.scope_key, units_text, g.since
        )
    };
    view! {
        <tr class="unpriced-row">
            <td class="clip mono-sm" title=tip>{g.name.clone()}</td>
            <td class="dim" title="Billable units with no row at this tier">{units_text}</td>
            <td class="price-tier">{tier_text}</td>
            <td class="num" colspan="2">
                <span class="chip warn">
                    {format!("{} req unpriced", grouped(g.requests.max(0) as u64))}
                </span>
            </td>
            <td class="col-p2 dim" colspan="2">{format!("since {since}")}</td>
            <td class="col-p3"></td>
            <td class="actions">
                <div class="row-acts">
                    <Show when=move || !no_tier>
                        <button
                            class="btn sm"
                            title=format!("Write a {} price for this model", g.tier)
                            on:click={
                                let target = target.clone();
                                move |_| price.run(target.clone())
                            }
                        >
                            {format!("Price {}", g.tier)}
                        </button>
                    </Show>
                </div>
            </td>
        </tr>
    }
}
