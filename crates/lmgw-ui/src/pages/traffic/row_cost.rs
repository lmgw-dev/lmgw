//! A request row's measured quantities and how it was priced
//! (billable-units design §8.4): the table shows what the row measured, "—"
//! where it measured nothing — never 0 — and the detail shows each priced
//! part as quantity × rate, from the rates the row snapshotted when it was
//! written, so it still explains its cost after a price has changed.
//!
//! The arithmetic is the gateway's own ([`PriceUnit::part_micro`]), and
//! whether an unknown cost is "unpriced" is the rollup's own rule
//! (`RowPricing::in_remainder`), so a detail never says something the Usage
//! page counts differently.

use lmgw_api_types::{PriceUnit, RequestRow, RowPricing};

use crate::fmt::{grouped, price};
use crate::pages::usage::{money_fine, quantity_texts};

/// What the row measured besides tokens — "27.4 s audio · 1.2K chars" — or
/// `None` when it measured nothing.
pub(super) fn measured(r: &RequestRow) -> Option<String> {
    let q = quantity_texts(
        r.audio_in_ms.unwrap_or(0),
        r.chars_in.unwrap_or(0),
        r.images_out.unwrap_or(0),
    );
    if !q.is_empty() {
        return Some(q.join(" · "));
    }
    // A measured 0 (an image answer with an empty `data`) is a measurement:
    // it reads as 0, where an unmeasured quantity reads as a dash.
    [
        (r.audio_in_ms, "0 s audio"),
        (r.chars_in, "0 chars"),
        (r.images_out, "0 images"),
    ]
    .into_iter()
    .find_map(|(v, zero)| (v == Some(0)).then(|| zero.to_string()))
}

/// The row's cost as its detail states it: the amount and where its price
/// came from; "free · local"; "unpriced" and what is unknown, for exactly the
/// rows the Usage page counts in its unpriced remainder; otherwise that
/// nothing was billed. `None` for a live row, which carries no snapshot (a
/// reload brings it).
pub(super) fn cost_line(r: &RequestRow) -> Option<String> {
    let p = r.pricing.as_ref()?;
    let source = p.source.as_deref().unwrap_or("unknown");
    if source == "free_local" {
        return Some("free · local".to_string());
    }
    if let Some(c) = r.cost_micro {
        return Some(format!("{} · {source}", money_fine(c, &p.currency)));
    }
    if !p.in_remainder {
        return Some(if r.class == "tool" {
            "nothing to price — a tool call, not a model call".to_string()
        } else {
            "nothing billed — refused or failed before it spent anything".to_string()
        });
    }
    if !has_rate(p) {
        return Some("unpriced — no price on file for it, which is not zero".to_string());
    }
    let unknown = unknown_parts(r, p);
    Some(if unknown.is_empty() {
        "unpriced — whether the upstream answered is unknown, which is not zero".to_string()
    } else {
        format!(
            "unpriced — its {} {} unknown, which is not zero",
            unknown.join(" and "),
            if unknown.len() == 1 { "is" } else { "are" }
        )
    })
}

fn has_rate(p: &RowPricing) -> bool {
    [
        p.price_in,
        p.price_out,
        p.price_per_audio_minute,
        p.price_per_mchar,
        p.price_per_image,
        p.price_per_request,
    ]
    .iter()
    .any(Option::is_some)
}

/// The priced parts of the row whose count is unknown, besides the request
/// count (which no column stores): the tokens, when a token rate applied and
/// neither count was reported, and each measured unit with a rate and no
/// quantity.
fn unknown_parts(r: &RequestRow, p: &RowPricing) -> Vec<&'static str> {
    let mut out = Vec::new();
    if (p.price_in.is_some() || p.price_out.is_some())
        && r.prompt_tokens.is_none()
        && r.completion_tokens.is_none()
    {
        out.push("token count");
    }
    for (rate, quantity, what) in [
        (p.price_per_audio_minute, r.audio_in_ms, "audio duration"),
        (p.price_per_mchar, r.chars_in, "character count"),
        (p.price_per_image, r.images_out, "image count"),
    ] {
        if rate.is_some() && quantity.is_none() {
            out.push(what);
        }
    }
    out
}

/// The units measured on the row, with the row's rate and quantity of each.
fn measured_units(r: &RequestRow, p: &RowPricing) -> [(PriceUnit, Option<f64>, Option<i64>); 3] {
    [
        (
            PriceUnit::PerAudioMinute,
            p.price_per_audio_minute,
            r.audio_in_ms,
        ),
        (PriceUnit::PerMchar, p.price_per_mchar, r.chars_in),
        (PriceUnit::PerImage, p.price_per_image, r.images_out),
    ]
}

/// Each priced part of the row, as `(what, how)`, in the units' order.
/// Empty for a local row and for one priced in nothing.
pub(super) fn parts(r: &RequestRow) -> Vec<(&'static str, String)> {
    let Some(p) = r.pricing.as_ref() else {
        return Vec::new();
    };
    if p.source.as_deref() == Some("free_local") {
        return Vec::new();
    }
    let cur = p.currency.as_str();
    let amount = |m: Option<i64>| {
        m.map(|m| format!(" = {}", money_fine(m, cur)))
            .unwrap_or_default()
    };
    let n = |v: i64| grouped(v.max(0) as u64);
    let mut out = Vec::new();

    if p.price_in.is_some() || p.price_out.is_some() {
        let rate = |r: Option<f64>| r.map(price).unwrap_or_else(|| "—".into());
        let text = if r.prompt_tokens.is_none() && r.completion_tokens.is_none() {
            format!(
                "not reported — in {}, out {} {} — unknown",
                rate(p.price_in),
                rate(p.price_out),
                PriceUnit::PerMtok.scale()
            )
        } else {
            // Cached input bills at the cache rates, which the row does not
            // snapshot: its part is stated, never as `N × rate`, which would
            // not be true of it.
            let read = r.cached_in_tokens.unwrap_or(0);
            let write = r.cache_write_tokens.unwrap_or(0);
            let input = match r.prompt_tokens {
                None => "in —".to_string(),
                Some(t) if read + write > 0 => format!(
                    "in {} ({} cache read, {} cache write){}, cache rates included",
                    n(t),
                    n(read),
                    n(write),
                    amount(p.cost_in_micro)
                ),
                Some(t) => format!(
                    "in {} × {}{}",
                    n(t),
                    rate(p.price_in),
                    amount(p.cost_in_micro)
                ),
            };
            let output = match r.completion_tokens {
                None => "out —".to_string(),
                Some(t) => format!(
                    "out {} × {}{}",
                    n(t),
                    rate(p.price_out),
                    amount(p.cost_out_micro)
                ),
            };
            format!("{input} · {output} ({})", PriceUnit::PerMtok.scale())
        };
        out.push((PriceUnit::PerMtok.label(), text));
    }

    // The measured units, each `quantity × rate`, with the gateway's own
    // rounding (billable-units §3.3).
    let mut known_units = 0i64;
    for (unit, rate, quantity) in measured_units(r, p) {
        let Some(rate) = rate else { continue };
        let text = match quantity {
            Some(q) => {
                let part = unit.part_micro(q.max(0) as u64, rate);
                known_units += part;
                format!(
                    "{} × {} {}{}",
                    quantity_of(unit, q),
                    price(rate),
                    unit.scale(),
                    amount(Some(part))
                )
            }
            None => format!("not measured × {} {} — unknown", price(rate), unit.scale()),
        };
        out.push((unit.label(), text));
    }

    if let Some(rate) = p.price_per_request {
        out.push((
            PriceUnit::PerRequest.label(),
            fee_line(r, p, rate, known_units),
        ));
    }
    out
}

/// The per-request fee's line. No column stores the request count: on a
/// priced row the fee is the unit part beyond the measured units' (exact,
/// since both are the gateway's integer parts), and its count follows from
/// the rate. On an unpriced row it says from the status whether the fee
/// applied and which part left the row unpriced.
fn fee_line(r: &RequestRow, p: &RowPricing, rate: f64, known_units: i64) -> String {
    let scale = PriceUnit::PerRequest.scale();
    if let (Some(_), Some(units)) = (r.cost_micro, p.cost_units_micro) {
        let fee = units - known_units;
        let cost = money_fine(fee, &p.currency);
        // A synthesis row is several upstream requests: its count is the one
        // whose fee is the part on the row.
        let count = (rate > 0.0)
            .then(|| (fee as f64 / (rate * 1_000_000.0)).round())
            .filter(|c| *c >= 1.0)
            .map(|c| c as u64)
            .filter(|c| PriceUnit::PerRequest.part_micro(*c, rate) == fee);
        return match count {
            Some(c) => format!("{} × {} {scale} = {cost}", grouped(c), price(rate)),
            None => format!("{} {scale} = {cost}", price(rate)),
        };
    }
    let answered = (200..300).contains(&r.status);
    let stopped = r.error_kind.as_deref() == Some("canceled");
    if !answered {
        return format!(
            "{} {scale} — no fee: the upstream did not answer",
            price(rate)
        );
    }
    let unknown = unknown_parts(r, p);
    if unknown.is_empty() {
        // Every other part is known, so the request count is what is not:
        // a call stopped while the upstream had it.
        return format!(
            "{} {scale} — {}, so whether it applies is unknown",
            price(rate),
            if stopped {
                "stopped before the upstream answered"
            } else {
                "the upstream's answer was not recorded"
            }
        );
    }
    format!(
        "{} {scale} — {}; the row is unpriced because its {} {} unknown",
        price(rate),
        if stopped {
            "applies if the upstream answered"
        } else {
            "applies"
        },
        unknown.join(" and "),
        if unknown.len() == 1 { "is" } else { "are" }
    )
}

/// One unit's quantity as the detail names it.
fn quantity_of(unit: PriceUnit, q: i64) -> String {
    match unit {
        PriceUnit::PerAudioMinute => format!("{:.1} s", q as f64 / 1000.0),
        PriceUnit::PerMchar => format!("{} chars", grouped(q.max(0) as u64)),
        PriceUnit::PerImage => format!(
            "{} image{}",
            grouped(q.max(0) as u64),
            if q == 1 { "" } else { "s" }
        ),
        _ => grouped(q.max(0) as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> RequestRow {
        RequestRow {
            log_id: 1,
            ts: String::new(),
            client_key: None,
            ingress_proto: "openai".into(),
            requested_alias: "whisper".into(),
            upstream_name: None,
            upstream_model: None,
            egress_proto: None,
            status: 200,
            ttfb_ms: None,
            total_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            cached_in_tokens: None,
            cache_write_tokens: None,
            streamed: false,
            error_kind: None,
            error_msg: None,
            mcp_tool: None,
            cost_micro: None,
            class: "audio".into(),
            key_id: None,
            max_tokens_clamped: None,
            fallback_reason: None,
            rung: None,
            degraded: None,
            audio_in_ms: None,
            chars_in: None,
            images_out: None,
            pricing: None,
        }
    }

    fn pricing(f: impl FnOnce(&mut RowPricing)) -> Option<RowPricing> {
        let mut p = RowPricing {
            currency: "USD".into(),
            source: Some("manual".into()),
            ..Default::default()
        };
        f(&mut p);
        Some(p)
    }

    #[test]
    fn an_unmeasured_row_is_a_dash_and_a_measured_zero_is_zero() {
        assert_eq!(measured(&row()), None);
        let r = RequestRow {
            audio_in_ms: Some(27_400),
            chars_in: Some(1234),
            ..row()
        };
        assert_eq!(measured(&r).as_deref(), Some("27.4 s audio · 1.2K chars"));
        let r = RequestRow {
            images_out: Some(0),
            ..row()
        };
        assert_eq!(measured(&r).as_deref(), Some("0 images"));
    }

    #[test]
    fn each_priced_part_is_quantity_times_rate() {
        // §3.3's whisper golden plus a fee: 27 s at 0.006/min is 2 700,
        // and the fee is the rest of the unit part.
        let r = RequestRow {
            audio_in_ms: Some(27_000),
            cost_micro: Some(7_700),
            pricing: pricing(|p| {
                p.cost_units_micro = Some(7_700);
                p.price_per_audio_minute = Some(0.006);
                p.price_per_request = Some(0.005);
            }),
            ..row()
        };
        assert_eq!(cost_line(&r).as_deref(), Some("$0.0077 · manual"));
        // Parts and totals keep their digits, so they add up on screen.
        assert_eq!(money_fine(55_100, "USD"), "$0.0551");
        assert_eq!(money_fine(3_000_000, "USD"), "$3.00");
        assert_eq!(money_fine(0, "USD"), "$0.00");
        assert_eq!(
            parts(&r),
            vec![
                (
                    "Input audio",
                    "27.0 s × 0.006 per minute of input audio = $0.0027".to_string()
                ),
                (
                    "Requests",
                    "1 × 0.005 per answered request = $0.005".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_synthesis_row_shows_its_fee_with_its_count() {
        // Three answered clauses at 0.001 a request, 1 234 characters at 15.
        let r = RequestRow {
            chars_in: Some(1_234),
            cost_micro: Some(18_510 + 3_000),
            pricing: pricing(|p| {
                p.cost_units_micro = Some(18_510 + 3_000);
                p.price_per_mchar = Some(15.0);
                p.price_per_request = Some(0.001);
            }),
            ..row()
        };
        assert_eq!(
            parts(&r)[1].1,
            "3 × 0.001 per answered request = $0.003",
            "{:?}",
            parts(&r)
        );
    }

    #[test]
    fn cached_input_is_never_shown_as_tokens_times_the_input_rate() {
        let r = RequestRow {
            class: "chat".into(),
            prompt_tokens: Some(1_000),
            completion_tokens: Some(100),
            cached_in_tokens: Some(600),
            cache_write_tokens: Some(200),
            cost_micro: Some(1_740 + 1_500),
            pricing: pricing(|p| {
                p.price_in = Some(3.0);
                p.price_out = Some(15.0);
                p.cost_in_micro = Some(1_740);
                p.cost_out_micro = Some(1_500);
            }),
            ..row()
        };
        let tokens = &parts(&r)[0].1;
        assert!(
            tokens.starts_with(&format!(
                "in {} (600 cache read, 200 cache write) = $0.00174, cache rates included",
                grouped(1_000)
            )),
            "{tokens}"
        );
        assert!(tokens.contains("out 100 × 15 = $0.0015"), "{tokens}");
    }

    #[test]
    fn unpriced_is_said_exactly_where_the_rollup_counts_it() {
        // A per-character scope and no count: in the remainder, and why.
        let r = RequestRow {
            pricing: pricing(|p| {
                p.source = Some("unknown".into());
                p.in_remainder = true;
                p.price_per_mchar = Some(15.0);
            }),
            ..row()
        };
        assert_eq!(
            cost_line(&r).as_deref(),
            Some("unpriced — its character count is unknown, which is not zero")
        );
        assert_eq!(
            parts(&r),
            vec![(
                "Input characters",
                "not measured × 15 per 1M characters of input text — unknown".to_string()
            )]
        );
        // A tool row and a refusal that spent nothing are not in it.
        let tool = RequestRow {
            class: "tool".into(),
            pricing: pricing(|p| p.source = None),
            ..row()
        };
        assert_eq!(
            cost_line(&tool).as_deref(),
            Some("nothing to price — a tool call, not a model call")
        );
        let held = RequestRow {
            status: 503,
            error_kind: Some("gpu_hold".into()),
            class: "chat".into(),
            pricing: pricing(|p| p.source = Some("unknown".into())),
            ..row()
        };
        assert_eq!(
            cost_line(&held).as_deref(),
            Some("nothing billed — refused or failed before it spent anything")
        );
        // A live row carries no snapshot: no cost line rather than a guess.
        assert_eq!(cost_line(&row()), None);
    }

    #[test]
    fn the_fee_line_follows_the_status_never_doubting_an_answer() {
        let fee_scope = |f: fn(&mut RowPricing)| {
            pricing(|p| {
                p.source = Some("unknown".into());
                p.price_in = Some(3.0);
                p.price_out = Some(15.0);
                p.price_per_request = Some(0.005);
                f(p);
            })
        };
        // An answered 200 whose tokens were not reported: the fee applies,
        // and the tokens are why the row is unpriced.
        let answered = RequestRow {
            class: "chat".into(),
            pricing: fee_scope(|p| p.in_remainder = true),
            ..row()
        };
        assert_eq!(
            parts(&answered)[1].1,
            "0.005 per answered request — applies; the row is unpriced because its token count \
             is unknown"
        );
        assert_eq!(
            cost_line(&answered).as_deref(),
            Some("unpriced — its token count is unknown, which is not zero")
        );
        // Stopped before the answer, its prompt estimated: only the count
        // is unknown.
        let stopped = RequestRow {
            class: "chat".into(),
            prompt_tokens: Some(8_100),
            error_kind: Some("canceled".into()),
            pricing: fee_scope(|p| p.in_remainder = true),
            ..row()
        };
        assert_eq!(
            parts(&stopped)[1].1,
            "0.005 per answered request — stopped before the upstream answered, so whether it \
             applies is unknown"
        );
        // An upstream refusal: no fee, and nothing billed.
        let refused = RequestRow {
            class: "chat".into(),
            status: 400,
            error_kind: Some("upstream".into()),
            pricing: fee_scope(|_| {}),
            ..row()
        };
        assert_eq!(
            parts(&refused)[1].1,
            "0.005 per answered request — no fee: the upstream did not answer"
        );
        assert!(cost_line(&refused).unwrap().starts_with("nothing billed"));
    }
}
