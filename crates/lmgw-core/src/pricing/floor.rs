//! Budget lower bounds for the unpriced rows a client can cause (openai
//! egress design §13, decision 22).
//!
//! An unpriced row adds nothing to a budget's spend, which is right for its
//! cost, its totals and its rollups, and a hole wherever a client can choose
//! to be unpriced. So such a row keeps a lower bound that only budgets read
//! (`Cost::budget_floor_micro`, migration 0094), priced on a sheet that is
//! known to cost no more than the real answer did:
//!
//! - **A tier gap above Standard** (§13.2): an `openai` answer served at
//!   `fast` (or `priority`), `ultrafast` or `scale` on a scope with no row at
//!   that tier, or one that named no price tier (`auto`, none at all), floors
//!   at the standard price, since each of those costs at least Standard, or
//!   at the flex price where the scope has no standard one (flex is the
//!   cheapest tier, so it bounds every tier). A served `default` with a gap
//!   floors at flex. A request that asked for flex and was answered at no
//!   tier floors at the flex price only, and a served `flex` or a value lmgw
//!   does not know has no floor.
//! - **An answer whose usage never came** (§13.4): a stream cut before the
//!   upstream reported usage floors from lmgw's own count of what it sent and
//!   relayed ([`LocalCount`]), on the same sheet.
//!
//! The row stays unpriced in both: cost NULL, its note saying what the
//! budgets counted.

use crate::config::PriceTier;
use crate::pricing::{micro_to_units, ServedTier, TokenUsage};

/// The sheets whose price bounds an `openai` row that `served` left
/// unpriced from below, in the order they are tried: the tightest bound
/// first, flex last, or none when nothing bounds it (§13.2). Flex is the
/// cheapest tier there is, so its price bounds every tier but one lmgw
/// does not know; Standard bounds the tiers that cost at least Standard.
/// Read only for a row that is unpriced because of its tier, or that
/// reported no usage.
pub fn tier_floor(served: &ServedTier) -> Vec<PriceTier> {
    let ServedTier::Reported { served, requested } = served else {
        return Vec::new();
    };
    let asked_flex = requested.as_deref() == Some("flex");
    match served.as_deref() {
        // No price tier named: the answer may have been anything the
        // project serves, but no tier is cheaper than flex. Only a request
        // that asked for flex can have been answered at it (decided
        // 2026-10-24, §13.6: flex served unasked stays unfloored), so
        // Standard bounds the others.
        None | Some("auto") if asked_flex => vec![PriceTier::Flex],
        None | Some("auto") => vec![PriceTier::Standard, PriceTier::Flex],
        Some(v) => match PriceTier::from_reported(v) {
            Some(PriceTier::Fast | PriceTier::Ultrafast | PriceTier::Scale) => {
                vec![PriceTier::Standard, PriceTier::Flex]
            }
            // Standard has no cheaper bound than flex.
            Some(PriceTier::Standard) => vec![PriceTier::Flex],
            // Flex is the cheapest tier, its own sheet is the one that has
            // the gap, and a value §5.4 does not map is not floored until
            // it does.
            Some(PriceTier::Flex) | None => Vec::new(),
        },
    }
}

/// Why a stream's row has no usage: it ended before the upstream reported
/// any. The head of its price note, before what the budgets counted.
pub const STREAM_NO_USAGE: &str = "stream ended before the upstream reported usage";

/// [`STREAM_NO_USAGE`] for a stream its client left.
pub const CLIENT_LEFT_NO_USAGE: &str = "the client left before the upstream reported usage";

/// [`STREAM_NO_USAGE`] for an in-process call its consumer stopped.
pub const STOPPED_NO_USAGE: &str = "the caller stopped it before the upstream reported usage";

/// Why a whole answer's row has no usage: its body reported none.
pub const ANSWER_NO_USAGE: &str = "the upstream's answer reported no usage";

/// Why the row of a request its client left after the upstream answered
/// in full has no usage: the answer reported none.
pub const CLIENT_LEFT_ANSWER_NO_USAGE: &str = "the client left; the answer reported no usage";

/// Why the row of a request its client left before the upstream answered
/// has no usage: it was sent, and nothing came back to read.
pub const CLIENT_LEFT_UNANSWERED: &str =
    "the client left before the upstream answered; the upstream reported no usage";

/// The sheets a [`LocalCount`] is priced on, the first that prices it
/// wins (§13.4, §13.6): the tier the answer named, when it named one (a
/// platform chat stream names it on every chunk, so a cut one still does),
/// then [`tier_floor`]'s bounds for the tier it named or the none it named
/// (a cut Responses stream names it only in its terminal event), flex
/// last.
pub fn count_floor_tiers(served: &ServedTier) -> Vec<PriceTier> {
    let mut tiers: Vec<PriceTier> = served.price_tier().into_iter().collect();
    if matches!(served, ServedTier::Untiered) {
        return tiers;
    }
    for t in tier_floor(served) {
        if !tiers.contains(&t) {
            tiers.push(t);
        }
    }
    tiers
}

/// lmgw's own count of an answer whose usage never came (§13.4): the prompt
/// as it was sent and the output text it relayed, each counted once with
/// the platform's tokenizer. Both are below the real numbers: images,
/// files, encrypted reasoning and hosted tool definitions count as nothing,
/// and reasoning tokens are never relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalCount {
    pub prompt: u64,
    pub output: u64,
    /// Estimated at ~4 characters a token instead of counted with a
    /// tokenizer: a row lmgw knows no tokenizer for (not `openai`).
    pub estimated: bool,
    /// How many kinds of the prompt's parts the count leaves out because
    /// lmgw cannot count them locally (images, documents, files, encrypted
    /// thinking: `proxy::count_messages::estimate::uncounted`); the row's
    /// note says so ([`floor_clause`]).
    pub uncounted: usize,
}

impl LocalCount {
    /// The count as usage on the floor sheet: the whole prompt at the
    /// cache-read rate (the plain input rate where the sheet has none),
    /// since the cut answer may have read its prompt from the cache and a
    /// floor at the plain rate could then exceed the real cost (decided
    /// 2026-10-24, §13.6).
    pub fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt: Some(self.prompt),
            cached_in: Some(self.prompt),
            completion: Some(self.output),
            ..Default::default()
        }
    }
}

/// The clause a floored row's note ends with: what the budgets counted, at
/// which price, as a lower bound. `count` is the local count it was taken
/// from, for a row with no usage of its own.
pub fn floor_clause(
    tier: PriceTier,
    micro: i64,
    count: Option<&LocalCount>,
    currency: &str,
) -> String {
    let amount = format!("{:.6} {currency}", micro_to_units(micro));
    let left_out = match count {
        Some(c) if c.uncounted > 0 => format!(
            "; the prompt count leaves out {} kind{} of part lmgw cannot count locally \
             (images, documents, files, encrypted thinking)",
            c.uncounted,
            if c.uncounted == 1 { "" } else { "s" }
        ),
        _ => String::new(),
    };
    match count {
        Some(c) if c.estimated => format!(
            "budgets count an estimate of {} prompt and {} output tokens (~4 characters a token) \
             at the {tier} price, {amount}, a lower bound{left_out}",
            c.prompt, c.output
        ),
        Some(c) => format!(
            "budgets count a local count of {} prompt and {} output tokens at the {tier} \
             price, {amount}, a lower bound{left_out}",
            c.prompt, c.output
        ),
        None => format!("budgets count it at the {tier} price, {amount}, a lower bound"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Protocol;

    fn reported(served: Option<&str>, requested: Option<&str>) -> ServedTier {
        ServedTier::of(
            Protocol::Openai,
            served.map(String::from),
            requested.map(String::from),
        )
    }

    #[test]
    fn every_tier_that_costs_at_least_standard_floors_at_standard_then_flex() {
        for served in [
            Some("fast"),
            Some("priority"),
            Some("ultrafast"),
            Some("scale"),
            Some("auto"),
            None,
        ] {
            assert_eq!(
                tier_floor(&reported(served, None)),
                [PriceTier::Standard, PriceTier::Flex],
                "{served:?}"
            );
            assert_eq!(
                tier_floor(&reported(served, Some("default"))),
                [PriceTier::Standard, PriceTier::Flex],
                "{served:?}"
            );
        }
    }

    #[test]
    fn a_served_default_floors_at_flex() {
        assert_eq!(
            tier_floor(&reported(Some("default"), None)),
            [PriceTier::Flex]
        );
        assert_eq!(
            tier_floor(&reported(Some("default"), Some("default"))),
            [PriceTier::Flex]
        );
    }

    #[test]
    fn flex_and_what_lmgw_does_not_know_have_no_floor() {
        assert!(tier_floor(&reported(Some("flex"), None)).is_empty());
        assert!(tier_floor(&reported(Some("flex"), Some("flex"))).is_empty());
        assert!(tier_floor(&reported(Some("turbo"), None)).is_empty());
        assert!(tier_floor(&ServedTier::Untiered).is_empty());
    }

    #[test]
    fn no_tier_named_with_flex_asked_floors_at_flex() {
        for served in [Some("auto"), None] {
            assert_eq!(
                tier_floor(&reported(served, Some("flex"))),
                [PriceTier::Flex],
                "{served:?}"
            );
        }
        // Fast answered although flex was asked: Fast costs at least
        // Standard whatever was asked.
        assert_eq!(
            tier_floor(&reported(Some("priority"), Some("flex"))),
            [PriceTier::Standard, PriceTier::Flex]
        );
    }

    #[test]
    fn a_count_is_priced_at_the_tier_named_then_at_its_floors() {
        assert_eq!(
            count_floor_tiers(&reported(Some("default"), None)),
            [PriceTier::Standard, PriceTier::Flex]
        );
        assert_eq!(
            count_floor_tiers(&reported(Some("priority"), None)),
            [PriceTier::Fast, PriceTier::Standard, PriceTier::Flex]
        );
        assert_eq!(
            count_floor_tiers(&reported(Some("flex"), Some("flex"))),
            [PriceTier::Flex]
        );
        assert_eq!(
            count_floor_tiers(&reported(None, None)),
            [PriceTier::Standard, PriceTier::Flex]
        );
        assert_eq!(
            count_floor_tiers(&reported(None, Some("flex"))),
            [PriceTier::Flex]
        );
        assert_eq!(
            count_floor_tiers(&ServedTier::Untiered),
            [PriceTier::Standard]
        );
    }

    #[test]
    fn a_local_count_reads_its_whole_prompt_from_the_cache() {
        let u = LocalCount {
            prompt: 1834,
            output: 212,
            estimated: false,
            uncounted: 0,
        }
        .usage();
        assert_eq!(
            (u.prompt, u.cached_in, u.completion, u.plain_in()),
            (Some(1834), Some(1834), Some(212), 0)
        );
    }

    #[test]
    fn the_clause_says_what_budgets_count() {
        assert_eq!(
            floor_clause(PriceTier::Standard, 4_100, None, "USD"),
            "budgets count it at the standard price, 0.004100 USD, a lower bound"
        );
        let c = LocalCount {
            prompt: 1834,
            output: 212,
            estimated: false,
            uncounted: 0,
        };
        assert_eq!(
            floor_clause(PriceTier::Flex, 1_100, Some(&c), "EUR"),
            "budgets count a local count of 1834 prompt and 212 output tokens at the flex \
             price, 0.001100 EUR, a lower bound"
        );
        let images = LocalCount { uncounted: 2, ..c };
        assert_eq!(
            floor_clause(PriceTier::Flex, 1_100, Some(&images), "EUR"),
            "budgets count a local count of 1834 prompt and 212 output tokens at the flex \
             price, 0.001100 EUR, a lower bound; the prompt count leaves out 2 kinds of part \
             lmgw cannot count locally (images, documents, files, encrypted thinking)"
        );
    }
}
