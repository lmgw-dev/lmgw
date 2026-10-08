//! Billable units on the price and usage ops (billable-units design §8.2,
//! §8.3, §8.5): what one `price_set` call writes, checked once for both
//! planes, and how a remainder names the quantities it holds.

use crate::config::PriceUnit;
use crate::pricing::{PriceSource, Prices};
use crate::store;

/// The rates one `price_set` call carries, as either plane passes them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PriceRates {
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub price_cache_write: Option<f64>,
    /// The one rate of a unit other than tokens, per the unit's scale.
    pub price: Option<f64>,
}

/// Every unit's spelling, for an error that has to name them.
fn unit_names() -> String {
    PriceUnit::ALL
        .iter()
        .map(|u| u.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// `unit` as given, `per_mtok` when omitted or empty, so a caller from before
/// units writes what it always wrote.
pub(super) fn parse_unit(unit: Option<&str>) -> Result<PriceUnit, String> {
    match unit.map(str::trim).filter(|u| !u.is_empty()) {
        None => Ok(PriceUnit::PerMtok),
        Some(u) => {
            PriceUnit::parse(u).ok_or_else(|| format!("unknown unit '{u}' ({})", unit_names()))
        }
    }
}

/// The row a `price_set` call writes for `unit`: the four token rates of a
/// `per_mtok` row, or the one `price` of any other unit (§2.2), refused by
/// name when the call mixes the two shapes, leaves the row without a rate,
/// or gives a rate that is no price. Each refusal names the unit's scale,
/// so the caller knows what the number it sends is per.
pub(super) fn manual_row(unit: PriceUnit, r: PriceRates) -> Result<(Prices, Option<f64>), String> {
    let named = [
        ("price_in", r.price_in),
        ("price_out", r.price_out),
        ("price_cache_read", r.price_cache_read),
        ("price_cache_write", r.price_cache_write),
        ("price", r.price),
    ];
    if let Some((name, v)) = named
        .iter()
        .find(|(_, v)| v.is_some_and(|v| !v.is_finite() || v < 0.0))
    {
        return Err(format!(
            "{name} must be a finite price of 0 or more, got {}",
            v.unwrap_or_default()
        ));
    }
    if unit.is_tokens() {
        if r.price.is_some() {
            return Err(format!(
                "a per_mtok row takes price_in and price_out (and the two cache rates), {}; \
                 price is the one rate of a unit other than tokens — pass that unit",
                unit.scale()
            ));
        }
        let p = Prices {
            price_in: r.price_in,
            price_out: r.price_out,
            price_cache_read: r.price_cache_read,
            price_cache_write: r.price_cache_write,
            source: PriceSource::Manual,
        };
        if !p.is_usable() {
            return Err(format!(
                "a per_mtok row needs price_in or price_out, {}",
                unit.scale()
            ));
        }
        return Ok((p, None));
    }
    let token_fields: Vec<&str> = named[..4]
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(n, _)| *n)
        .collect();
    if !token_fields.is_empty() {
        return Err(format!(
            "a {unit} row takes one price, {}; {} {} per_mtok's rates",
            unit.scale(),
            token_fields.join(", "),
            if token_fields.len() == 1 { "is" } else { "are" },
        ));
    }
    match r.price {
        Some(v) => Ok((
            Prices {
                source: PriceSource::Manual,
                ..Default::default()
            },
            Some(v),
        )),
        None => Err(format!("a {unit} row needs price, {}", unit.scale())),
    }
}

/// Every unit and what its rate is per, for a hint a caller reads before
/// it writes a price: `per_mtok: per 1M tokens (price_in, …)`, one line.
pub(super) fn unit_table() -> String {
    PriceUnit::ALL
        .iter()
        .map(|u| {
            if u.is_tokens() {
                format!(
                    "{u} {} (price_in, price_out, price_cache_read, price_cache_write)",
                    u.scale()
                )
            } else {
                format!("{u} {} (price)", u.scale())
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Input audio as a reader takes it in: seconds below a minute, minutes
/// above, one decimal.
fn audio_text(ms: i64) -> String {
    if ms < 60_000 {
        format!("{:.1} s audio", ms as f64 / 1000.0)
    } else {
        format!("{:.1} min audio", ms as f64 / 60_000.0)
    }
}

/// The measured quantities among `(audio_in_ms, chars_in, images_out)`
/// that are not zero, each as a reader takes it in, in that order. Empty
/// when nothing was measured: a remainder or a breakdown names only what it
/// holds (§8.3).
fn quantity_texts(audio_in_ms: i64, chars_in: i64, images_out: i64) -> Vec<String> {
    let mut out = Vec::new();
    if audio_in_ms > 0 {
        out.push(audio_text(audio_in_ms));
    }
    if chars_in > 0 {
        out.push(format!("{chars_in} chars"));
    }
    if images_out > 0 {
        out.push(format!(
            "{images_out} image{}",
            if images_out == 1 { "" } else { "s" }
        ));
    }
    out
}

/// What the unpriced remainder of `c` holds, for the parenthesis after its
/// request count: its tokens always, then each non-zero quantity —
/// "0 tokens, 4.5 min audio".
pub(super) fn remainder_holds(c: &store::UsageCell) -> String {
    let mut parts = vec![format!("{} tokens", c.cost_unknown_tokens)];
    parts.extend(quantity_texts(
        c.cost_unknown_audio_in_ms,
        c.cost_unknown_chars_in,
        c.cost_unknown_images_out,
    ));
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> PriceRates {
        PriceRates::default()
    }

    #[test]
    fn an_omitted_unit_is_tokens_and_an_unknown_one_is_named() {
        assert_eq!(parse_unit(None), Ok(PriceUnit::PerMtok));
        assert_eq!(parse_unit(Some(" ")), Ok(PriceUnit::PerMtok));
        assert_eq!(parse_unit(Some("per_mchar")), Ok(PriceUnit::PerMchar));
        let e = parse_unit(Some("per_second")).unwrap_err();
        assert!(
            e.contains("per_second") && e.contains("per_audio_minute"),
            "{e}"
        );
    }

    #[test]
    fn a_token_row_takes_the_four_rates_and_refuses_price() {
        let (p, price) = manual_row(
            PriceUnit::PerMtok,
            PriceRates {
                price_in: Some(3.0),
                price_out: Some(15.0),
                ..rates()
            },
        )
        .unwrap();
        assert_eq!(price, None);
        assert_eq!((p.price_in, p.price_out), (Some(3.0), Some(15.0)));
        assert_eq!(p.source, PriceSource::Manual);

        let e = manual_row(
            PriceUnit::PerMtok,
            PriceRates {
                price: Some(1.0),
                ..rates()
            },
        )
        .unwrap_err();
        assert!(e.contains("per 1M tokens"), "{e}");
        let e = manual_row(PriceUnit::PerMtok, rates()).unwrap_err();
        assert!(
            e.contains("price_in or price_out") && e.contains("per 1M tokens"),
            "{e}"
        );
    }

    #[test]
    fn a_unit_row_takes_one_price_and_refuses_the_token_rates() {
        let (p, price) = manual_row(
            PriceUnit::PerAudioMinute,
            PriceRates {
                price: Some(0.006),
                ..rates()
            },
        )
        .unwrap();
        assert_eq!(price, Some(0.006));
        assert_eq!(p.price_in, None);
        assert_eq!(p.source, PriceSource::Manual);

        let e = manual_row(
            PriceUnit::PerMchar,
            PriceRates {
                price: Some(15.0),
                price_in: Some(1.0),
                ..rates()
            },
        )
        .unwrap_err();
        assert!(
            e.contains("per 1M characters of input text") && e.contains("price_in"),
            "{e}"
        );
        let e = manual_row(PriceUnit::PerImage, rates()).unwrap_err();
        assert!(
            e.contains("needs price") && e.contains("per generated image"),
            "{e}"
        );
    }

    #[test]
    fn a_negative_rate_is_no_price() {
        let e = manual_row(
            PriceUnit::PerRequest,
            PriceRates {
                price: Some(-0.01),
                ..rates()
            },
        )
        .unwrap_err();
        assert!(e.contains("price must be"), "{e}");
        // A manual 0 is a statement, and allowed: it drops a fee (§2.3).
        assert!(manual_row(
            PriceUnit::PerRequest,
            PriceRates {
                price: Some(0.0),
                ..rates()
            },
        )
        .is_ok());
    }

    #[test]
    fn a_remainder_names_its_tokens_and_only_the_quantities_it_holds() {
        let c = store::UsageCell {
            cost_unknown_requests: 3,
            cost_unknown_audio_in_ms: 270_000,
            ..Default::default()
        };
        assert_eq!(remainder_holds(&c), "0 tokens, 4.5 min audio");
        let c = store::UsageCell {
            cost_unknown_tokens: 12,
            cost_unknown_chars_in: 1234,
            cost_unknown_images_out: 1,
            ..Default::default()
        };
        assert_eq!(remainder_holds(&c), "12 tokens, 1234 chars, 1 image");
        assert_eq!(audio_text(27_400), "27.4 s audio");
    }
}
