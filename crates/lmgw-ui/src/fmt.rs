//! Small display helpers shared across pages.

/// Bytes → "18.6 GB" (decimal units, one decimal below 100).
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else if v < 100.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// Integer with thin-space thousands grouping: 131072 → "131 072".
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

/// Seconds → a short human age ("12s", "4m", "3h 12m", "2d 1h"). Shared by
/// the gateway uptime and the per-model runtime table's started/last-used
/// columns (per-model-containers §8) — ages are as of the last fetch/frame,
/// not ticking live, same convention the gateway uptime already used.
pub fn age(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// Deterministic identity hue (0..360) for a model name — drives the
/// `.model-chip` swatch via `--hue`. FNV-1a so the same alias always gets
/// the same color, everywhere.
pub fn hue_for(name: &str) -> u32 {
    let mut h: u32 = 0x811C9DC5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h % 360
}

/// Compact magnitude for a chart axis or a tile: 1_350_000 -> "1.4M".
/// Counts and tokens only — money is never compacted, because the cents are
/// the part a spend figure is read for.
pub fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}K", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{:.0}", v)
    }
}

/// Fraction -> percent, with one decimal only where it would otherwise read
/// as a flat zero.
pub fn pct(v: f64) -> String {
    if v.abs() < 0.1 && v != 0.0 {
        format!("{:.1}%", v * 100.0)
    } else {
        format!("{:.0}%", v * 100.0)
    }
}

/// "17 of 395", or just "395" when nothing is held back (UX plan §4 counts).
pub fn of(shown: usize, total: usize) -> String {
    if shown == total {
        grouped(total as u64)
    } else {
        format!("{} of {}", grouped(shown as u64), grouped(total as u64))
    }
}

/// A plural noun in the singular, for a count of one: "models" → "model",
/// "corpora" → "corpus", "entries" → "entry". The last word of a phrase
/// ("stopped models") is the one that changes.
pub fn singular(plural: &str) -> String {
    let (head, word) = match plural.rfind(' ') {
        Some(i) => plural.split_at(i + 1),
        None => ("", plural),
    };
    let one = match word {
        "corpora" => "corpus".to_string(),
        "aliases" => "alias".to_string(),
        "docs" => "doc".to_string(),
        w if w.ends_with("ies") => format!("{}y", &w[..w.len() - 3]),
        w if w.ends_with("ches") || w.ends_with("shes") || w.ends_with("xes") => {
            w[..w.len() - 2].to_string()
        }
        w => w.strip_suffix('s').unwrap_or(w).to_string(),
    };
    format!("{head}{one}")
}

/// "1 corpus", "3 corpora", "1 204 models": the count, grouped, and the
/// noun that agrees with it (UX review ux:U-7, no more "1 corpora").
pub fn count_of(n: usize, plural: &str) -> String {
    if n == 1 {
        format!("1 {}", singular(plural))
    } else {
        format!("{} {plural}", grouped(n as u64))
    }
}

/// A price as a person writes it: at most six significant digits, no float
/// noise (0.7999999999 → "0.8"), never an exponent, no trailing zeros.
/// Whole-number digits are never rounded away.
pub fn price(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let mag = v.abs().log10().floor() as i32;
    let decimals = (5 - mag).clamp(0, 12) as usize;
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    if s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

/// An OpenAI-style per-token price string (`"0.0000008"`) as a price per
/// million tokens (`0.8`), with the float noise of the multiplication gone.
pub fn per_mtok(per_token: &str) -> Option<f64> {
    let v: f64 = per_token.trim().parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some((v * 1e6 * 1e9).round() / 1e9)
}

/// How long ago `unix_s` was, as of `now_s`: "just now", "40s ago", "4m ago",
/// "3h ago", "2d ago"; a future time reads "in 5m".
pub fn rel_time_at(unix_s: f64, now_s: f64) -> String {
    let d = now_s - unix_s;
    let (secs, future) = (d.abs().round() as u64, d < 0.0);
    if secs < 10 {
        return "just now".to_string();
    }
    let span = match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    };
    if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

/// [`rel_time_at`] against the clock.
#[allow(dead_code)] // the area phases' timestamps adopt it
pub fn rel_time(unix_s: f64) -> String {
    rel_time_at(unix_s, js_sys::Date::now() / 1000.0)
}

/// `unix_s` as the reader's local date and time, "2026-09-24 14:03:07".
#[allow(dead_code)]
pub fn local_datetime(unix_s: f64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(unix_s * 1000.0));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

/// A request-log timestamp in the reader's own time.
///
/// The log stores `YYYY-MM-DD HH:MM:SS` in UTC (SQLite's `datetime('now')`);
/// printed raw, a row at 23:30 UTC sits under the wrong day for anyone east
/// of Greenwich. `time` is the local clock time, `day` a sortable local day
/// key and `day_label` the separator a list prints ("Thu 24 Sep").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTime {
    pub time: String,
    pub day: String,
    pub day_label: String,
    /// The stored value, said to be UTC, for a tooltip.
    pub utc: String,
}

pub fn log_time(ts: &str) -> LogTime {
    const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let utc = format!("{} UTC", ts.get(0..19).unwrap_or(ts));
    let zoned = ts.ends_with('Z') || ts.get(10..).is_some_and(|t| t.contains('+'));
    let iso = if zoned {
        ts.replacen(' ', "T", 1)
    } else {
        format!("{}Z", ts.get(0..19).unwrap_or(ts).replacen(' ', "T", 1))
    };
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(&iso));
    if d.get_time().is_nan() {
        // Unparseable: show what was stored rather than nothing.
        return LogTime {
            time: ts.get(11..19).unwrap_or(ts).to_string(),
            day: ts.get(0..10).unwrap_or("").to_string(),
            day_label: ts.get(0..10).unwrap_or("").to_string(),
            utc,
        };
    }
    LogTime {
        time: format!(
            "{:02}:{:02}:{:02}",
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        ),
        day: format!(
            "{:04}-{:02}-{:02}",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date()
        ),
        day_label: format!(
            "{} {} {}",
            DOW[(d.get_day() as usize).min(6)],
            d.get_date(),
            MON[(d.get_month() as usize).min(11)]
        ),
        utc,
    }
}

// The area phases' timestamps adopt it; nothing renders one yet.
#[allow(unused_imports)]
pub use rel_time_view::RelTime;

#[allow(dead_code)]
mod rel_time_view {
    use leptos::prelude::*;

    /// A relative time ("4m ago") with the absolute local time in its
    /// tooltip. As of when it was rendered — it does not tick, like
    /// [`super::age`].
    #[component]
    pub fn RelTime(
        /// Unix seconds.
        ts: f64,
    ) -> impl IntoView {
        view! {
            <time class="rel-time" title=super::local_datetime(ts)>
                {super::rel_time(ts)}
            </time>
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_of_one_takes_the_singular() {
        assert_eq!(count_of(1, "corpora"), "1 corpus");
        assert_eq!(count_of(2, "corpora"), "2 corpora");
        assert_eq!(count_of(1, "models"), "1 model");
        assert_eq!(count_of(0, "models"), "0 models");
        assert_eq!(count_of(1, "stopped models"), "1 stopped model");
        assert_eq!(count_of(1, "aliases"), "1 alias");
        assert_eq!(count_of(1, "entries"), "1 entry");
        assert_eq!(count_of(1, "price sheets"), "1 price sheet");
        assert_eq!(count_of(1, "matches"), "1 match");
        assert_eq!(count_of(12_000, "rows"), "12\u{202F}000 rows");
        for w in [
            "files",
            "tools",
            "upstreams",
            "prices",
            "docs",
            "agents",
            "conversations",
        ] {
            assert_eq!(singular(w), &w[..w.len() - 1]);
        }
    }

    #[test]
    fn of_says_the_total_alone_when_nothing_is_held_back() {
        assert_eq!(of(395, 395), "395");
        assert_eq!(of(17, 395), "17 of 395");
        assert_eq!(of(12, 12_000), "12 of 12\u{202F}000");
    }

    #[test]
    fn prices_have_no_float_noise_and_no_exponent() {
        assert_eq!(price(0.7999999999999999), "0.8");
        assert_eq!(price(0.8), "0.8");
        assert_eq!(price(15.0), "15");
        assert_eq!(price(0.0000125), "0.0000125");
        assert_eq!(price(1.23456789), "1.23457");
        assert_eq!(price(1234567.8), "1234568");
        assert_eq!(price(0.0), "0");
        assert_eq!(price(-2.5), "-2.5");
        assert!(!price(1e-9).contains('e'));
    }

    #[test]
    fn per_token_strings_become_per_million() {
        assert_eq!(per_mtok("0.0000008"), Some(0.8));
        assert_eq!(per_mtok("0.0000016"), Some(1.6));
        assert_eq!(per_mtok("0"), Some(0.0));
        assert_eq!(per_mtok(" 0.000015 "), Some(15.0));
        assert_eq!(per_mtok("free"), None);
    }

    #[test]
    fn relative_times_read_like_speech() {
        assert_eq!(rel_time_at(1000.0, 1003.0), "just now");
        assert_eq!(rel_time_at(1000.0, 1040.0), "40s ago");
        assert_eq!(rel_time_at(1000.0, 1000.0 + 4.0 * 60.0), "4m ago");
        assert_eq!(rel_time_at(1000.0, 1000.0 + 3.0 * 3600.0), "3h ago");
        assert_eq!(rel_time_at(1000.0, 1000.0 + 3.0 * 86_400.0), "3d ago");
        assert_eq!(rel_time_at(1000.0 + 300.0, 1000.0), "in 5m");
    }
}
