//! The billable quantities on the Usage page (billable-units design §8.3):
//! how a measured quantity reads, what an unpriced remainder holds, and
//! where a window's recording of quantities starts.
//!
//! A quantity is what was **measured** or reported, never estimated, and
//! nothing was recorded before `units_since`: a bucket before it reads 0
//! because nobody counted, not because nothing was processed. So every
//! figure here is labelled *measured*, and a window that starts earlier says
//! from when.

use lmgw_api_types::UsageCell;

use crate::fmt::{compact, grouped};

use super::shared::MONTHS;

/// Input audio as a reader takes it in: seconds below a minute, minutes
/// above (one decimal below a hundred, whole above), compacted past a
/// thousand minutes.
pub(crate) fn audio_text(ms: i64) -> String {
    let ms = ms.max(0) as f64;
    if ms < 60_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else if ms < 6_000_000.0 {
        format!("{:.1} min", ms / 60_000.0)
    } else if ms < 60_000_000.0 {
        format!("{:.0} min", ms / 60_000.0)
    } else {
        format!("{} min", compact(ms / 60_000.0))
    }
}

/// Characters, as a tile or a cell prints them.
pub(super) fn chars_text(n: i64) -> String {
    compact(n.max(0) as f64)
}

/// The non-zero quantities among `(audio_in_ms, chars_in, images_out)`,
/// each with what it is — "4.5 min audio", "1.2K chars", "2 images" — in
/// that order. Empty when nothing was measured.
pub(crate) fn quantity_texts(audio_in_ms: i64, chars_in: i64, images_out: i64) -> Vec<String> {
    let mut out = Vec::new();
    if audio_in_ms > 0 {
        out.push(format!("{} audio", audio_text(audio_in_ms)));
    }
    if chars_in > 0 {
        out.push(format!("{} chars", chars_text(chars_in)));
    }
    if images_out > 0 {
        out.push(format!(
            "{} image{}",
            grouped(images_out as u64),
            if images_out == 1 { "" } else { "s" }
        ));
    }
    out
}

/// What a cell measured: its quantities joined, or `None`.
pub(super) fn measured_of(c: &UsageCell) -> Option<String> {
    let q = quantity_texts(c.audio_in_ms, c.chars_in, c.images_out);
    (!q.is_empty()).then(|| q.join(" · "))
}

/// The §2.3 obligation for a cell, naming everything its remainder holds:
/// "3 requests (0 tokens, 4.5 min audio) unpriced". `None` when nothing is
/// unpriced. The tokens are always named; a quantity only when it is not 0.
pub(super) fn unpriced_of(c: &UsageCell) -> Option<String> {
    if c.cost_unknown_requests <= 0 && c.cost_unknown_tokens <= 0 {
        return None;
    }
    Some(format!(
        "{} requests ({}) unpriced",
        grouped(c.cost_unknown_requests.max(0) as u64),
        remainder_holds(c)
    ))
}

/// What a cell's unpriced remainder holds, for the parenthesis after its
/// request count: "0 tokens, 4.5 min audio".
pub(super) fn remainder_holds(c: &UsageCell) -> String {
    let mut parts = vec![format!(
        "{} tokens",
        compact(c.cost_unknown_tokens.max(0) as f64)
    )];
    parts.extend(quantity_texts(
        c.cost_unknown_audio_in_ms,
        c.cost_unknown_chars_in,
        c.cost_unknown_images_out,
    ));
    parts.join(", ")
}

/// "since 15 Jan 2026" when the window starting at the UTC hour key `from`
/// begins before quantities were recorded (`units_since`, RFC 3339 UTC);
/// `None` when the whole window was recorded, or the gateway does not say.
pub(super) fn since_note(from: &str, units_since: Option<&str>) -> Option<String> {
    let since = units_since?;
    if !starts_before(from, Some(since)) {
        return None;
    }
    let ms = js_sys::Date::parse(since);
    if !ms.is_finite() {
        return Some(format!("since {}", &since[..10]));
    }
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(ms));
    Some(format!(
        "since {} {} {}",
        d.get_date(),
        MONTHS[(d.get_month() as usize).min(11)],
        d.get_full_year()
    ))
}

/// Whether a window starting at the UTC hour key `from` starts before the
/// hour quantities were first recorded in. `false` when the gateway does not
/// say when that was.
pub(super) fn starts_before(from: &str, units_since: Option<&str>) -> bool {
    // Both are UTC: the hour key `YYYY-MM-DDTHH` is a prefix of RFC 3339's.
    units_since
        .and_then(|s| s.get(..13))
        .is_some_and(|since_hour| from < since_hour)
}

/// The UTC hour key the previous window of `[from, to)` starts at — the
/// window of the same length just before it, the one the server compares
/// against (`from − (to − from)`).
pub(super) fn previous_from(from: &str, to: &str) -> Option<String> {
    let ms = |k: &str| {
        let v = js_sys::Date::parse(&format!("{k}:00:00Z"));
        v.is_finite().then_some(v)
    };
    let (f, t) = (ms(from)?, ms(to)?);
    Some(super::shared::utc_hour_key(f - (t - f)))
}

/// The bucket label `units_since` falls in, in the reader's local time and
/// the granularity `like` has (`2026-01-15T12`, `2026-01-15`, `2026-W02`,
/// `2026-01`) — the format the server's buckets use, so a label compares
/// with it as a string. A bucket before it holds nothing recorded.
pub(super) fn since_bucket(units_since: &str, like: &str) -> Option<String> {
    let ms = js_sys::Date::parse(units_since);
    if !ms.is_finite() {
        return None;
    }
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(ms));
    let (y, m, day, h) = (
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
    );
    Some(match like.len() {
        13 => format!("{y:04}-{m:02}-{day:02}T{h:02}"),
        8 => {
            let jan1 = js_sys::Date::new_with_year_month_day(y, 0, 1);
            let today = js_sys::Date::new_with_year_month_day(y, m as i32 - 1, day as i32);
            let yday = ((today.get_time() - jan1.get_time()) / 86_400_000.0).round() as u32;
            format!("{y:04}-W{:02}", week_of_year(yday, d.get_day()))
        }
        7 => format!("{y:04}-{m:02}"),
        _ => format!("{y:04}-{m:02}-{day:02}"),
    })
}

/// SQLite's `%W`: the week of the year, `00`–`53`, week 01 starting on the
/// year's first Monday. `yday` is 0-based, `wday` 0 = Sunday.
fn week_of_year(yday: u32, wday: u32) -> u32 {
    let monday_based = (wday + 6) % 7;
    (yday + 7 - monday_based) / 7
}

/// Each bucket's value, or `None` for a bucket before quantities were
/// recorded — what a quantity sparkline draws from, so it starts where the
/// counting started instead of drawing zeros nobody counted.
pub(super) fn recorded_only(
    buckets: &[String],
    vals: Vec<f64>,
    units_since: Option<&str>,
) -> Vec<f64> {
    let Some(first) = buckets.first() else {
        return vals;
    };
    let Some(since) = units_since.and_then(|s| since_bucket(s, first)) else {
        return vals;
    };
    buckets
        .iter()
        .zip(vals)
        .filter(|(b, _)| b.as_str() >= since.as_str())
        .map(|(_, v)| v)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quantity_reads_in_the_unit_a_reader_expects() {
        assert_eq!(audio_text(27_400), "27.4 s");
        assert_eq!(audio_text(270_000), "4.5 min");
        assert_eq!(audio_text(10_782_000), "180 min");
        assert_eq!(
            quantity_texts(270_000, 1234, 1),
            ["4.5 min audio", "1.2K chars", "1 image"]
        );
        assert!(quantity_texts(0, 0, 0).is_empty());
    }

    #[test]
    fn the_remainder_names_its_tokens_and_only_the_quantities_it_holds() {
        let c = UsageCell {
            cost_unknown_requests: 3,
            cost_unknown_audio_in_ms: 270_000,
            ..Default::default()
        };
        assert_eq!(
            unpriced_of(&c).as_deref(),
            Some("3 requests (0 tokens, 4.5 min audio) unpriced")
        );
        assert_eq!(unpriced_of(&UsageCell::default()), None);
    }

    #[test]
    fn a_window_starts_before_the_recording_by_its_hour() {
        let since = Some("2026-01-15T12:00:00Z");
        assert!(starts_before("2026-01-15T11", since));
        // The hour recording started in is recorded, if only partly.
        assert!(!starts_before("2026-01-15T12", since));
        assert!(!starts_before("2026-01-16T00", since));
        // A gateway that does not say suppresses nothing.
        assert!(!starts_before("2020-01-01T00", None));
    }

    #[test]
    fn week_numbers_match_sqlite() {
        // 2026-01-01 is a Thursday: week 00 until the first Monday, 5 Jan.
        assert_eq!(week_of_year(0, 4), 0);
        assert_eq!(week_of_year(4, 1), 1);
        // 2026-10-07, a Wednesday, day 279: SQLite's %W says 40.
        assert_eq!(week_of_year(279, 3), 40);
    }
}
