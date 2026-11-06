//! The service tier one upstream answer reported, and what it does to that
//! answer's price (openai egress design §5.2, §5.4–§5.6).
//!
//! OpenAI prices one model at several tiers (Standard, Flex, Fast, …), and
//! its answer says which one served the request. lmgw prices at that tier
//! (decision 10), with the price rows of that tier only. A tier with no row,
//! a value that names no tier and an answer that named none are all
//! **unpriced, with a note**: never priced at Standard (decision 15). Every
//! other provider has one tier, so its rows and answers are Standard.

use crate::config::{PriceTier, PriceUnit, Protocol};

/// What one row knows of the service tier that served it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServedTier {
    /// No tier dimension: every protocol but `openai`, and the `openai`
    /// routes whose answers name no tier (embeddings, audio, images, legacy
    /// completions). Priced at Standard (§5.4).
    #[default]
    Untiered,
    /// An `openai` chat or Responses answer.
    Reported {
        /// The `service_tier` the answer reported, verbatim; `None` when it
        /// named none, which is an unknown tier, not `default` (§5.2).
        served: Option<String>,
        /// The `service_tier` the request asked for, verbatim, when it named
        /// one: what the note says beside the served tier (§5.6).
        requested: Option<String>,
    },
}

impl ServedTier {
    /// The tier of an answer from a route of `protocol` that reports one:
    /// [`Reported`](Self::Reported) on `openai`, [`Untiered`](Self::Untiered)
    /// everywhere else, whatever the answer carried — on an
    /// `openai_compatible` row the dimension does not exist (§5.4).
    pub fn of(protocol: Protocol, served: Option<String>, requested: Option<String>) -> Self {
        match protocol {
            Protocol::Openai => Self::Reported { served, requested },
            _ => Self::Untiered,
        }
    }

    /// The price tier the row is priced at (§5.4): Standard for an untiered
    /// row, the mapped tier for a reported one, `None` for an answer that
    /// named no tier or one with no price tier (`auto`, anything new).
    pub fn price_tier(&self) -> Option<PriceTier> {
        match self {
            Self::Untiered => Some(PriceTier::Standard),
            Self::Reported { served, .. } => served.as_deref().and_then(PriceTier::from_reported),
        }
    }

    /// What `request_logs.service_tier` stores: the reported value, verbatim.
    pub fn reported(&self) -> Option<&str> {
        match self {
            Self::Untiered => None,
            Self::Reported { served, .. } => served.as_deref(),
        }
    }

    /// The tier the answer reported, so the client sees it (§5.7): the raw
    /// value on an `openai` row, nothing anywhere else.
    pub fn relayed(&self) -> Option<String> {
        self.reported().map(str::to_string)
    }
}

/// A set of price tiers, small and `Copy` so a [`super::Sheet`] stays so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierSet(u8);

impl TierSet {
    fn bit(t: PriceTier) -> u8 {
        1 << PriceTier::ALL.iter().position(|x| *x == t).unwrap_or(0)
    }

    pub fn insert(&mut self, t: PriceTier) {
        self.0 |= Self::bit(t);
    }

    pub fn contains(self, t: PriceTier) -> bool {
        self.0 & Self::bit(t) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The tiers in the set, in [`PriceTier::ALL`]'s order.
    pub fn iter(self) -> impl Iterator<Item = PriceTier> {
        PriceTier::ALL
            .into_iter()
            .filter(move |t| self.contains(*t))
    }
}

/// A unit of a scope that has usable rows at some tier, but none at the one
/// that served the request (§5.6): it makes the request unpriced. `on_file`
/// is every tier that does have a row for such a unit, what the note names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierGap {
    pub on_file: TierSet,
    /// Bit `i` set: [`PriceUnit::ALL`]`[i]` has a row at another tier and
    /// none at this one. Read through [`Self::missing_units`].
    missing: u8,
}

impl TierGap {
    pub fn new(on_file: TierSet, missing: impl IntoIterator<Item = PriceUnit>) -> Self {
        let missing = missing
            .into_iter()
            .filter_map(|u| PriceUnit::ALL.iter().position(|x| *x == u))
            .fold(0u8, |m, i| m | (1 << i));
        Self { on_file, missing }
    }

    /// The units with no row at the tier, in [`PriceUnit::ALL`]'s order:
    /// what a row to enter has to price.
    pub fn missing_units(self) -> impl Iterator<Item = PriceUnit> {
        PriceUnit::ALL
            .into_iter()
            .enumerate()
            .filter(move |(i, _)| self.missing & (1 << i) != 0)
            .map(|(_, u)| u)
    }
}

/// What a note or refusal says is on file of `tiers`: "a standard one is",
/// "standard and flex ones are", "none is".
pub fn on_file_words(tiers: TierSet) -> String {
    let tiers: Vec<&str> = tiers.iter().map(PriceTier::as_str).collect();
    match tiers.as_slice() {
        [] => "none is".to_string(),
        [one] => format!("a {one} one is"),
        [rest @ .., last] => format!("{} and {last} ones are", rest.join(", ")),
    }
}

/// How a note for an answer that named no service tier begins; the Prices
/// page's list finds such rows by it ([`crate::store::unpriced_tier_traffic`]).
pub const NO_TIER_NOTE: &str = "the upstream reported no service tier";

/// The row's `price_note` for an answer its tier left unpriced (§5.6), e.g.
/// "served at service tier flex (requested: default); no flex price on file
/// for 'gpt-5.6-sol', a standard one is". `scope` is the model the row
/// names.
pub fn gap_note(served: &ServedTier, gap: &TierGap, scope: &str) -> String {
    let requested = match served {
        ServedTier::Reported {
            requested: Some(r), ..
        } => format!(" (requested: {r})"),
        _ => String::new(),
    };
    let on_file = on_file_words(gap.on_file);
    match (served.reported(), served.price_tier()) {
        // An untiered row is priced at Standard and has none on file.
        _ if *served == ServedTier::Untiered => {
            format!("no standard price on file for '{scope}', {on_file}")
        }
        (Some(raw), Some(tier)) => format!(
            "served at service tier {raw}{requested}; no {tier} price on file for '{scope}', \
             {on_file}"
        ),
        (Some(raw), None) => format!(
            "served at service tier '{raw}'{requested}, which names no price tier; unpriced \
             rather than priced at another tier ('{scope}': {on_file} on file)"
        ),
        (None, _) => format!(
            "{NO_TIER_NOTE}{requested}; unpriced rather than assumed \
             standard ('{scope}': {on_file} on file)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(served: Option<&str>, requested: Option<&str>) -> ServedTier {
        ServedTier::of(
            Protocol::Openai,
            served.map(String::from),
            requested.map(String::from),
        )
    }

    fn gap(tiers: &[PriceTier]) -> TierGap {
        let mut on_file = TierSet::default();
        for t in tiers {
            on_file.insert(*t);
        }
        TierGap::new(on_file, [])
    }

    #[test]
    fn only_the_platform_has_a_tier() {
        assert_eq!(
            ServedTier::of(Protocol::OpenaiCompatible, Some("flex".into()), None),
            ServedTier::Untiered
        );
        assert_eq!(ServedTier::Untiered.price_tier(), Some(PriceTier::Standard));
        assert_eq!(ServedTier::Untiered.reported(), None);
        assert_eq!(
            reported(Some("priority"), None).price_tier(),
            Some(PriceTier::Fast)
        );
        assert_eq!(reported(Some("auto"), None).price_tier(), None);
        assert_eq!(
            reported(None, None).price_tier(),
            None,
            "absent is not default"
        );
    }

    #[test]
    fn the_note_names_the_tier_the_request_and_what_is_on_file() {
        let s = reported(Some("flex"), Some("default"));
        assert_eq!(
            gap_note(&s, &gap(&[PriceTier::Standard]), "gpt-5.6-sol"),
            "served at service tier flex (requested: default); no flex price on file for \
             'gpt-5.6-sol', a standard one is"
        );
        let s = reported(Some("priority"), None);
        assert_eq!(
            gap_note(&s, &gap(&[PriceTier::Standard, PriceTier::Flex]), "m"),
            "served at service tier priority; no fast price on file for 'm', standard and \
             flex ones are"
        );
        assert!(gap_note(&reported(Some("auto"), None), &gap(&[]), "m")
            .contains("'auto', which names no price tier"));
        assert!(gap_note(&reported(None, Some("flex")), &gap(&[]), "m")
            .starts_with("the upstream reported no service tier (requested: flex)"));
        assert_eq!(
            gap_note(&ServedTier::Untiered, &gap(&[PriceTier::Flex]), "m"),
            "no standard price on file for 'm', a flex one is"
        );
    }
}
