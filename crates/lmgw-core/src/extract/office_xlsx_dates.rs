//! Date and time cells of an xlsx workbook.
//!
//! A date in xlsx is a plain number; only the cell's number format
//! (`xl/styles.xml`: `numFmts` and `cellXfs`) says it is one, and the
//! workbook's `date1904` flag says which day is serial 0. This module reads
//! those two facts and turns the serial into ISO text: `YYYY-MM-DD`,
//! `YYYY-MM-DD HH:MM[:SS]` or `HH:MM[:SS]`. Anything it cannot place (a
//! negative serial, one past year 9999) stays the number as written.
//!
//! The 1900 system counts a day that never existed: serial 60 is
//! `1900-02-29` (Lotus 1-2-3's leap-year bug, kept by Excel), so serials
//! below 61 are one day off the real calendar and 60 itself is rendered as
//! that phantom date.

use std::collections::HashMap;

use super::office::{read_entry_if_present, walk_xml, Budget, OfficeError, Tok, Zip};

/// What a stored style, `xf` or `numFmt` costs against the [`Budget`]: the
/// vector slot and map entry it takes, more than the bytes it is read from.
const STYLE_COST_BYTES: u64 = 32;

/// The last serial rendered as a date: `9999-12-31`.
const LAST_DATE_SERIAL: f64 = 2_958_465.0;

/// A number format that shows a date and/or a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Fmt {
    date: bool,
    time: bool,
    /// The format has a seconds field.
    secs: bool,
    /// The hours are elapsed (`[h]`), not the time of day.
    elapsed: bool,
}

/// Which `cellXfs` styles are date formats, and the workbook's date system.
#[derive(Debug, Default)]
pub(super) struct DateStyles {
    xfs: Vec<Option<Fmt>>,
    date1904: bool,
}

impl DateStyles {
    /// Read `xl/styles.xml` (when there is one) against the budget.
    /// `date1904` comes from `workbook.xml`.
    pub fn load(
        zip: &mut Zip<'_>,
        budget: &mut Budget,
        date1904: bool,
    ) -> Result<Self, OfficeError> {
        let mut out = DateStyles {
            xfs: Vec::new(),
            date1904,
        };
        let Some(xml) = read_entry_if_present(zip, "xl/styles.xml", budget)? else {
            return Ok(out);
        };
        let mut custom: HashMap<u32, Option<Fmt>> = HashMap::new();
        let mut xf_ids: Vec<u32> = Vec::new();
        let mut in_xfs = false;
        walk_xml(&xml, "xl/styles.xml", |t| {
            match t {
                Tok::Start(e) => match e.name.as_str() {
                    "numFmt" => {
                        budget.spend(STYLE_COST_BYTES, "the number formats of xl/styles.xml")?;
                        if let (Some(id), Some(code)) = (
                            e.attr("numFmtId").and_then(|v| v.parse().ok()),
                            e.attr("formatCode"),
                        ) {
                            custom.insert(id, classify(code));
                        }
                    }
                    "cellXfs" => in_xfs = true,
                    "xf" if in_xfs => {
                        budget.spend(STYLE_COST_BYTES, "the cell styles of xl/styles.xml")?;
                        xf_ids.push(e.attr("numFmtId").and_then(|v| v.parse().ok()).unwrap_or(0));
                    }
                    _ => {}
                },
                Tok::End(n) if n == "cellXfs" => in_xfs = false,
                _ => {}
            }
            Ok(())
        })?;
        out.xfs = xf_ids
            .into_iter()
            .map(|id| custom.get(&id).copied().unwrap_or_else(|| builtin(id)))
            .collect();
        Ok(out)
    }

    /// The ISO text of the number `raw` in a cell of style `style`, or `None`
    /// when that style is not a date or the number cannot be one.
    pub fn render(&self, style: usize, raw: &str) -> Option<String> {
        let fmt = self.xfs.get(style).copied().flatten()?;
        render(fmt, raw.trim().parse::<f64>().ok()?, self.date1904)
    }
}

/// The built-in number formats that are dates or times (ECMA-376 18.8.30;
/// the ranges 27-36 and 50-58 are the East Asian locale dates).
fn builtin(id: u32) -> Option<Fmt> {
    let f = |date, time, secs, elapsed| {
        Some(Fmt {
            date,
            time,
            secs,
            elapsed,
        })
    };
    match id {
        14..=17 | 27..=31 | 34..=36 | 50..=58 => f(true, false, false, false),
        18 | 20 | 32 => f(false, true, false, false),
        19 | 21 | 33 => f(false, true, true, false),
        22 => f(true, true, false, false),
        45 | 47 => f(false, true, true, false),
        46 => f(false, true, true, true),
        _ => None,
    }
}

/// Is this custom format code a date or time? The tokens `y m d h s` count
/// outside quotes, `\x` and `_x` / `*x` escapes and `[...]` conditions and
/// locale tags; `[h]`, `[m]` and `[s]` are elapsed time. Only the first
/// section (the one a positive number takes) is read. `m` is a minute next to
/// an `h` or an `s`, a month otherwise.
pub(super) fn classify(code: &str) -> Option<Fmt> {
    let mut letters = String::new();
    let mut elapsed = false;
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            '[' => {
                let inner: String = chars.by_ref().take_while(|c| *c != ']').collect();
                let l = inner.to_ascii_lowercase();
                if let Some(first) = l.chars().next() {
                    if matches!(first, 'h' | 'm' | 's') && l.chars().all(|c| c == first) {
                        elapsed |= first == 'h';
                        // An elapsed minute is always a minute.
                        letters.push(if first == 'm' { 'M' } else { first });
                    }
                }
            }
            ';' => break,
            c => letters.push(c.to_ascii_lowercase()),
        }
    }
    // Only the letters that matter: `am/pm` must not read as a month.
    let letters = letters.replace("am/pm", "").replace("a/p", "");
    let toks: Vec<char> = letters
        .chars()
        .filter(|c| matches!(c, 'y' | 'm' | 'M' | 'd' | 'h' | 's'))
        .collect();
    let mut f = Fmt {
        elapsed,
        ..Fmt::default()
    };
    for (i, c) in toks.iter().enumerate() {
        match c {
            'y' | 'd' => f.date = true,
            'h' => f.time = true,
            's' => {
                f.time = true;
                f.secs = true;
            }
            'M' => f.time = true,
            _ => {
                let before = toks[..i].iter().rev().find(|c| **c != 'm');
                let after = toks[i..].iter().find(|c| **c != 'm');
                if matches!(before, Some('h')) || matches!(after, Some('s')) {
                    f.time = true;
                } else {
                    f.date = true;
                }
            }
        }
    }
    (f.date || f.time).then_some(f)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn render(f: Fmt, serial: f64, date1904: bool) -> Option<String> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let hms = |secs: i64, hours: i64| {
        let (m, s) = ((secs % 3600) / 60, secs % 60);
        if f.secs {
            format!("{hours:02}:{m:02}:{s:02}")
        } else {
            format!("{hours:02}:{m:02}")
        }
    };
    if !f.date {
        if serial > 1e9 {
            return None;
        }
        let total = (serial * 86_400.0).round() as i64;
        let hours = if f.elapsed {
            total / 3600
        } else {
            (total % 86_400) / 3600
        };
        return Some(hms(total, hours));
    }
    if serial > LAST_DATE_SERIAL {
        return None;
    }
    let (day, secs) = if f.time {
        let total = (serial * 86_400.0).round() as i64;
        (total.div_euclid(86_400), total.rem_euclid(86_400))
    } else {
        (serial.floor() as i64, 0)
    };
    let date = if date1904 {
        let (y, m, d) = civil(day - 24_107);
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        match day {
            0 => "1900-01-00".to_string(),
            60 => "1900-02-29".to_string(),
            // Before the phantom leap day the serials are one day early.
            1..=59 => {
                let (y, m, d) = civil(day - 25_568);
                format!("{y:04}-{m:02}-{d:02}")
            }
            _ => {
                let (y, m, d) = civil(day - 25_569);
                format!("{y:04}-{m:02}-{d:02}")
            }
        }
    };
    Some(if f.time {
        format!("{date} {}", hms(secs, secs / 3600))
    } else {
        date
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(code: &str, serial: f64) -> Option<String> {
        render(classify(code)?, serial, false)
    }

    #[test]
    fn custom_codes_are_read_outside_quotes_brackets_and_escapes() {
        for yes in [
            "yyyy-mm-dd",
            "dd/mm/yy",
            "[$-409]d-mmm-yy;@",
            "h:mm AM/PM",
            "[h]:mm:ss",
            "mm:ss.0",
            "\"Due \"dd.mm.",
            "[Red]d",
        ] {
            assert!(classify(yes).is_some(), "{yes}");
        }
        for no in [
            "General",
            "0.00",
            "#,##0.00 \"days\"",
            "0.0E+00",
            "0 \\d\\a\\y\\s",
            "[Red]0.00;[Blue]0.00",
            "\"yyyy\"0",
            "@",
            "0%",
            "_(* #,##0_);_(* (#,##0)",
        ] {
            assert_eq!(classify(no), None, "{no}");
        }
        // `m` next to h or s is a minute; otherwise a month.
        assert_eq!(classify("h:mm"), classify("hh:mm"));
        let f = classify("h:mm").unwrap();
        assert!(f.time && !f.date);
        let f = classify("mm/dd").unwrap();
        assert!(f.date && !f.time);
        let f = classify("mm:ss").unwrap();
        assert!(f.time && !f.date && f.secs);
    }

    #[test]
    fn serials_become_iso_dates_and_times() {
        assert_eq!(r("yyyy-mm-dd", 46295.0).unwrap(), "2026-09-30");
        assert_eq!(r("yyyy-mm-dd", 45000.5).unwrap(), "2023-03-15");
        assert_eq!(r("yyyy-mm-dd h:mm", 45000.5).unwrap(), "2023-03-15 12:00");
        assert_eq!(
            r("yyyy-mm-dd h:mm:ss", 45000.75 + 1.0 / 86_400.0).unwrap(),
            "2023-03-15 18:00:01"
        );
        assert_eq!(r("h:mm", 0.25).unwrap(), "06:00");
        assert_eq!(r("h:mm:ss", 0.5 + 30.0 / 86_400.0).unwrap(), "12:00:30");
        // Elapsed hours run past 24; the time of day does not.
        assert_eq!(r("[h]:mm", 1.5).unwrap(), "36:00");
        assert_eq!(r("h:mm", 1.5).unwrap(), "12:00");
        // Not a date: negative, absurd, not finite.
        assert_eq!(r("yyyy-mm-dd", -1.0), None);
        assert_eq!(r("yyyy-mm-dd", 1e12), None);
        assert_eq!(r("yyyy-mm-dd", f64::NAN), None);
    }

    #[test]
    fn the_1900_leap_year_bug_and_the_1904_system() {
        let d = |s| r("yyyy-mm-dd", s).unwrap();
        assert_eq!(d(1.0), "1900-01-01");
        assert_eq!(d(59.0), "1900-02-28");
        assert_eq!(d(60.0), "1900-02-29");
        assert_eq!(d(61.0), "1900-03-01");
        assert_eq!(d(0.0), "1900-01-00");
        let f = classify("yyyy-mm-dd").unwrap();
        assert_eq!(render(f, 0.0, true).unwrap(), "1904-01-01");
        assert_eq!(render(f, 1462.0, true).unwrap(), "1908-01-02");
        // 1904-01-01 is serial 1462 in the 1900 system and 0 in the 1904 one.
        assert_eq!(d(1462.0), "1904-01-01");
    }

    #[test]
    fn built_in_ids_are_dates_and_times() {
        for id in [14, 15, 16, 17, 22, 27, 36, 50, 58] {
            assert!(builtin(id).is_some_and(|f| f.date), "{id}");
        }
        for id in [18, 19, 20, 21, 45, 46, 47] {
            assert!(builtin(id).is_some_and(|f| f.time && !f.date), "{id}");
        }
        for id in [0, 1, 9, 10, 11, 12, 13, 37, 49, 164] {
            assert_eq!(builtin(id), None, "{id}");
        }
    }
}
