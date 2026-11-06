//! A budget can only be enforced on what is priced (openai egress design
//! §13.3, decision 24, the owner's ruling of 2026-10-24).
//!
//! An unpriced row adds nothing to a budget's spend unless something bounds
//! it from below (`pricing::tier_floor`, `pricing::LocalCount`). A caller
//! with a budget that sends to a cloud model whose scope does not price what
//! the request is billed in ([`Need`]: a model turn needs an input and an
//! output token price, an encoder call an input one, and a per-request fee
//! alone serves only an image or audio call) would spend what no budget sees, while the budget looks
//! kept. So such a request is refused before the send, with an error that
//! names the model and the missing price: asking a user to set every price
//! is better than a budget that looks kept while an unpriced model spends
//! tokens it never sees.
//!
//! Flex without a flex price is one case of this: flex is the cheapest
//! tier, so no other tier's price bounds a flex answer, and only a flex row
//! prices it. A tier above the one priced is not refused: the cheaper price
//! bounds it (`pricing::count_floor_tiers`). A local route is free, and a
//! caller without a budget is never refused.

use crate::config::{ApiKey, PriceTier, PriceUnit, Route, Snapshot};
use crate::error::GatewayError;
use crate::pricing::{ServedTier, Sheet, TierSet};
use crate::telemetry::RequestClass;

/// What a price row on a scope must name for a request of `class` to be
/// counted by a budget (decision 24, the owner's ruling of 2026-10-24: a
/// budget is enforced only on what is priced):
///
/// - a model turn needs both a token input and a token output price;
/// - an encoder call (embeddings, rerank) needs a token input price;
/// - an image call needs a price per image or per request: its answer's
///   token usage is not read (billable-units §4.8);
/// - an audio call needs a price in any unit it can be billed in: tokens,
///   input audio, input characters or a request.
///
/// A per-request fee alone satisfies only the image and audio calls: it
/// bounds nothing of a model turn's size. A manual 0 is a price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Turn,
    Encoder,
    Image,
    Audio,
}

impl Need {
    fn of(class: RequestClass) -> Option<Self> {
        match class {
            RequestClass::Chat => Some(Self::Turn),
            RequestClass::Aux => Some(Self::Encoder),
            RequestClass::Image => Some(Self::Image),
            RequestClass::Audio => Some(Self::Audio),
            RequestClass::Tool => None,
        }
    }

    /// What `sheet` lacks of it; empty: the need is met.
    fn lacks(self, sheet: &Sheet) -> Vec<&'static str> {
        let tokens = sheet.tokens.unwrap_or_default();
        match self {
            Self::Turn => match (tokens.price_in, tokens.price_out) {
                (Some(_), Some(_)) => Vec::new(),
                (None, None) => vec!["input and output token price"],
                (None, Some(_)) => vec!["input token price"],
                (Some(_), None) => vec!["output token price"],
            },
            Self::Encoder => {
                if tokens.price_in.is_some() {
                    Vec::new()
                } else {
                    vec!["input token price"]
                }
            }
            Self::Image => {
                if sheet.rate(PriceUnit::PerImage).is_some()
                    || sheet.rate(PriceUnit::PerRequest).is_some()
                {
                    Vec::new()
                } else {
                    vec!["price per image or per request"]
                }
            }
            Self::Audio => {
                let met = tokens.is_usable()
                    || [
                        PriceUnit::PerAudioMinute,
                        PriceUnit::PerMchar,
                        PriceUnit::PerRequest,
                    ]
                    .iter()
                    .any(|u| sheet.rate(*u).is_some());
                if met {
                    Vec::new()
                } else {
                    vec!["price per token, audio minute, character or request"]
                }
            }
        }
    }
}

/// Refuse a send of `class` on `route`, asking for the `requested` service
/// tier, by the caller `key` (`None`: no key, or none the snapshot knows),
/// when a budget applies (the key's, or the global one: decided
/// 2026-10-24, §13.6) and the scope `alias` resolves to on `route` prices
/// none of the units `class` is billed in, at the tier asked for or at one
/// whose price bounds it from below.
///
/// Run where the route is final and before the send: the hold, the
/// outside-VRAM swap and a candidate walk can each change the route, and
/// only the route names the scope.
pub fn check_priced(
    snap: &Snapshot,
    key: Option<&ApiKey>,
    alias: &str,
    route: &Route,
    class: RequestClass,
    requested: Option<&str>,
) -> Result<(), GatewayError> {
    let Some(need) = Need::of(class) else {
        return Ok(());
    };
    if snap.is_local_upstream(route.upstream.id) {
        return Ok(());
    }
    let scope = match key.filter(|k| k.policy.budget_micro > 0) {
        Some(k) => k.described(),
        None if snap.settings.global_budget_micro > 0 => "this gateway".to_string(),
        None => return Ok(()),
    };
    // The tiers whose price bounds the answer from below: the one asked
    // for, then the cheaper bounds (§13.2); Standard for a route without
    // tiers. A value no tier maps to bounds nothing of its own, so the
    // tiers of an answer that names none stand in.
    let asked = ServedTier::of(
        route.upstream.protocol,
        requested.map(str::to_string),
        requested.map(str::to_string),
    );
    let mut tiers = crate::pricing::count_floor_tiers(&asked);
    if tiers.is_empty() {
        tiers =
            crate::pricing::count_floor_tiers(&ServedTier::of(route.upstream.protocol, None, None));
    }
    let sheet = |tier: PriceTier| {
        snap.sheet_for(
            route.priced_alias(alias),
            Some(route.upstream.id),
            Some(&route.upstream_model),
            Some(tier),
        )
    };
    if tiers.iter().any(|t| need.lacks(&sheet(*t)).is_empty()) {
        return Ok(());
    }
    // What the refusal names: the tier asked for, what it lacks, and the
    // tiers that have all of it.
    let tier = asked.price_tier().unwrap_or(PriceTier::Standard);
    let flex = tier == PriceTier::Flex;
    let mut on_file = TierSet::default();
    for t in PriceTier::ALL {
        if need.lacks(&sheet(t)).is_empty() {
            on_file.insert(t);
        }
    }
    let lacking = need.lacks(&sheet(tiers.first().copied().unwrap_or(tier)));
    let mut other = crate::pricing::on_file_words(on_file);
    // A token row on an image call is on file, and not read.
    if need == Need::Image && PriceTier::ALL.iter().any(|t| sheet(*t).tokens.is_some()) {
        other = "token prices are not read for image calls".to_string();
    }
    Err(GatewayError::BudgetUnpriced {
        scope,
        target: format!(
            "'{}' on upstream '{}'",
            route.upstream_model, route.upstream.name
        ),
        missing: format!("{tier} {} on file ({other})", lacking.join(" or ")),
        or_default: flex && !on_file.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Prices;

    fn tokens(price_in: Option<f64>, price_out: Option<f64>) -> Sheet {
        Sheet {
            tokens: Some(Prices {
                price_in,
                price_out,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_turn_needs_input_and_output_an_encoder_input_only() {
        let only_in = tokens(Some(1.0), None);
        assert_eq!(Need::Turn.lacks(&only_in), ["output token price"]);
        assert!(Need::Encoder.lacks(&only_in).is_empty());
        assert_eq!(
            Need::Turn.lacks(&Sheet::default()),
            ["input and output token price"]
        );
        // A manual 0 is a price.
        assert!(Need::Turn.lacks(&tokens(Some(0.0), Some(0.0))).is_empty());
    }

    #[test]
    fn a_tool_row_needs_nothing_and_a_model_class_has_a_need() {
        assert!(Need::of(RequestClass::Tool).is_none());
        for class in [
            RequestClass::Chat,
            RequestClass::Aux,
            RequestClass::Audio,
            RequestClass::Image,
        ] {
            assert!(Need::of(class).is_some(), "{class:?}");
        }
    }
}
