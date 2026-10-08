//! Billable units (docs/design/2026-10-07-billable-units-design.md §2.1):
//! what a price row counts, and the scale its rate is quoted in.
//!
//! One table for the gateway, the dashboard and the API document, so a unit
//! is spelled, labelled and scaled the same way wherever it appears.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The unit of one `prices` row.
///
/// Each names what is counted and its scale, not a provider's word for it:
/// "$0.37 per hour" is entered as `0.0061667` per minute. The scale is the one
/// providers quote, so a cost stays a plain product in integer micro-units.
///
/// Video seconds and web-search calls have no unit yet: no route carries
/// them, and a unit nothing can measure would leave every request priced in
/// it unpriced.
// Billable-units design §3.3 (scales) and §4.6 (`per_video_second` and
// `per_web_search`, reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PriceUnit {
    /// Tokens, at four rates (input, output, cache read, cache write), per 1M
    /// tokens.
    #[default]
    PerMtok,
    /// Duration of **input** audio, per minute.
    PerAudioMinute,
    /// Characters of **input** text, per 1M characters.
    PerMchar,
    /// **Generated** images, per image. An input image is a different
    /// quantity and would get a unit of its own.
    PerImage,
    /// Upstream requests that were answered, per request.
    PerRequest,
}

impl PriceUnit {
    /// Every unit, tokens first.
    pub const ALL: [Self; 5] = [
        Self::PerMtok,
        Self::PerAudioMinute,
        Self::PerMchar,
        Self::PerImage,
        Self::PerRequest,
    ];

    /// The stored and wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PerMtok => "per_mtok",
            Self::PerAudioMinute => "per_audio_minute",
            Self::PerMchar => "per_mchar",
            Self::PerImage => "per_image",
            Self::PerRequest => "per_request",
        }
    }

    /// `None` for anything that is not one of [`ALL`](Self::ALL)'s
    /// spellings, so a caller refuses an unknown unit by name instead of
    /// pricing it as another one.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.as_str() == s)
    }

    /// A short name for what is counted.
    pub fn label(self) -> &'static str {
        match self {
            Self::PerMtok => "Tokens",
            Self::PerAudioMinute => "Input audio",
            Self::PerMchar => "Input characters",
            Self::PerImage => "Generated images",
            Self::PerRequest => "Requests",
        }
    }

    /// What one rate is per, as a price field's label says it.
    pub fn scale(self) -> &'static str {
        match self {
            Self::PerMtok => "per 1M tokens",
            Self::PerAudioMinute => "per minute of input audio",
            Self::PerMchar => "per 1M characters of input text",
            Self::PerImage => "per generated image",
            Self::PerRequest => "per answered request",
        }
    }

    /// The token unit, whose row carries four rates; every other unit's row
    /// carries one `price`.
    pub fn is_tokens(self) -> bool {
        self == Self::PerMtok
    }

    /// One part of a cost in micro-units: `quantity × rate × scale`, rounded
    /// once. The scale is the one providers quote, so tokens and
    /// characters (per 1M) need no constant at all. The gateway prices with
    /// this and the dashboard explains a row with it, so the two cannot
    /// round apart.
    pub fn part_micro(self, quantity: u64, rate: f64) -> i64 {
        let q = quantity as f64;
        let micro = match self {
            Self::PerMtok | Self::PerMchar => q * rate,
            Self::PerAudioMinute => q * rate * 1_000_000.0 / 60_000.0,
            Self::PerImage | Self::PerRequest => q * rate * 1_000_000.0,
        };
        micro.round() as i64
    }
}

impl fmt::Display for PriceUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_round_trips_through_its_spelling_and_serde() {
        for u in PriceUnit::ALL {
            assert_eq!(PriceUnit::parse(u.as_str()), Some(u));
            assert_eq!(
                serde_json::to_value(u).unwrap(),
                serde_json::json!(u.as_str())
            );
            assert_eq!(u.to_string(), u.as_str());
        }
    }

    #[test]
    fn the_dropped_placeholders_and_reserved_units_do_not_parse() {
        for s in [
            "per_second",
            "per_char",
            "per_video_second",
            "per_web_search",
            "",
        ] {
            assert_eq!(PriceUnit::parse(s), None, "{s}");
        }
    }
}
