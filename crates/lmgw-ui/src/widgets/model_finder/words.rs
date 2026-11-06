//! The finder's words (find-a-model design §5, D39): the card's lines from
//! its fit, in everyday words first with the term as the hint, the times,
//! the rate limit's wait and the id the card hands Get it. Pure, so the
//! words are unit-tested without a browser.

use lmgw_api_types::model_find::{CardKind, FileFit, FitMark, FitMeasure};
use lmgw_api_types::model_get::{GetWaiting, WaitReason};

use crate::fmt::{fmt_bytes, fmt_bytes_of, fmt_bytes_so_far, grouped};

/// The GPU hold's line (D10), the one `modelinfo::fit` puts on a fit's
/// notes: the card shows it once, from the live hold, and the fold leaves
/// the fit's copy out.
pub(super) use lmgw_api_types::model_find::HOLD_LINE;

/// "262 144 tokens".
pub(super) fn tokens(n: u64) -> String {
    format!("{} tokens", grouped(n))
}

/// The model's id Get it asks for (D28, D84): the original's repo name in
/// lower case. `Qwen/Qwen3.6-27B` → `qwen3.6-27b`.
pub(super) fn model_id_for(origin: &str) -> String {
    origin
        .rsplit('/')
        .next()
        .unwrap_or(origin)
        .trim()
        .to_lowercase()
}

/// The class a card's kind lists under in the file list (D29).
pub(super) fn target_of(kind: CardKind) -> &'static str {
    match kind {
        CardKind::Chat => "chat",
        CardKind::Embed | CardKind::Rerank | CardKind::Decision => "aux",
    }
}

/// "27 billion parameters", "1.5 billion parameters", "600 million
/// parameters": the count in words, one decimal under ten billion.
pub(super) fn parameters(n: u64) -> String {
    let (v, unit) = match n {
        n if n >= 1_000_000_000_000 => (n as f64 / 1e12, "trillion"),
        n if n >= 1_000_000_000 => (n as f64 / 1e9, "billion"),
        n if n >= 1_000_000 => (n as f64 / 1e6, "million"),
        n => return format!("{} parameters", grouped(n)),
    };
    let shown = if v < 10.0 && (v * 10.0).round() % 10.0 != 0.0 {
        format!("{v:.1}")
    } else {
        format!("{}", v.round() as u64)
    };
    format!("{shown} {unit} parameters")
}

/// What the card can do, with its size, as one line (D22): "Reads pictures
/// · thinks before it answers · 27 billion parameters".
pub(super) fn can_line(can: &[String], params: Option<u64>) -> String {
    let mut parts: Vec<String> = can.iter().filter(|c| !c.is_empty()).cloned().collect();
    if let Some(p) = params {
        parts.push(parameters(p));
    }
    let mut line = parts.join(" · ");
    if let Some(first) = line.get(..1) {
        line = first.to_uppercase() + &line[1..];
    }
    line
}

/// How sure a mark is: by the header, or by size while the header is
/// being read, or by size with no read coming (a metered connection, a
/// header that couldn't be read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Checked {
    Header,
    Checking,
    BySize,
}

/// The card's chip (§5): its words and its tone (`ok`, `warn`, `off`).
/// `who` says who makes room for a fit on the whole card (`marks::who`).
pub(super) fn chip(fit: &FileFit, who: &str, checked: Checked) -> (String, &'static str) {
    let whole = fit.measure == FitMeasure::WholeCard;
    match fit.mark {
        FitMark::Fits | FitMark::FitsShorter if whole => (format!("fits once {who}"), "warn"),
        FitMark::Fits | FitMark::FitsShorter => ("fits your graphics card".into(), "ok"),
        FitMark::FitsBySize => {
            let tail = match checked {
                Checked::Checking => " · checking…",
                _ => " · not checked",
            };
            if whole {
                (format!("fits by size once {who}{tail}"), "warn")
            } else {
                (format!("fits by size{tail}"), "off")
            }
        }
        FitMark::DoesNotFit | FitMark::DoesNotFitBySize => {
            ("doesn't fit your graphics card".into(), "off")
        }
        FitMark::Unknown => ("size unknown · not checked".into(), "off"),
        FitMark::NoCard => ("can't measure your graphics card yet".into(), "off"),
    }
}

/// "How long a conversation it can remember · context, 131 072 tokens"
/// (D39), or an embedder's purpose in its place (§5).
pub(super) fn context_line(kind: CardKind, fit: &FileFit, checked: Checked) -> String {
    const HEAD: &str = "How long a conversation it can remember · context";
    match kind {
        CardKind::Embed | CardKind::Rerank => {
            return "For searching your files (Knowledge)".into();
        }
        CardKind::Decision => return "For answering decisions (the Decision lab)".into(),
        CardKind::Chat => {}
    }
    match (fit.mark, fit.ctx, fit.trained_ctx) {
        (FitMark::FitsShorter, Some(c), Some(t)) => {
            format!("{HEAD}, {} of {}", grouped(c), tokens(t))
        }
        (FitMark::Fits | FitMark::FitsShorter, Some(c), _) => format!("{HEAD}, {}", tokens(c)),
        (_, _, Some(t)) if checked == Checked::Header => {
            format!("{HEAD}: the model can take {}", tokens(t))
        }
        _ => match checked {
            Checked::Checking => format!("{HEAD}: checking…"),
            _ => format!("{HEAD}: not checked yet"),
        },
    }
}

/// "Uses 17.0 GiB of the 18.0 GiB free beside what runs now" (§5): the
/// footprint with the headroom, against the measure's room, in the
/// graphics card's units (D143). By size it is the files and the headroom,
/// so "Needs about". `None` without a card or a size to say it with.
pub(super) fn uses_line(fit: &FileFit) -> Option<String> {
    if matches!(fit.mark, FitMark::NoCard | FitMark::Unknown) || fit.capacity_bytes == 0 {
        return None;
    }
    let uses = fmt_bytes(fit.uses_bytes.saturating_add(fit.headroom_bytes));
    let room = fmt_bytes(fit.capacity_bytes);
    if matches!(fit.mark, FitMark::DoesNotFit | FitMark::DoesNotFitBySize) {
        return Some(match fit.measure {
            FitMeasure::Beside => format!(
                "Needs {uses} on your graphics card, more than the {room} free beside what runs now"
            ),
            FitMeasure::WholeCard => {
                format!("Needs {uses} on your graphics card, more than the {room} it has")
            }
        });
    }
    let by_size = fit.mark == FitMark::FitsBySize;
    let what = if by_size { "Needs about" } else { "Uses" };
    Some(match fit.measure {
        FitMeasure::Beside => {
            format!("{what} {uses} of the {room} free on your graphics card beside what runs now")
        }
        FitMeasure::WholeCard => format!("{what} {uses} of the {room} your graphics card has"),
    })
}

/// "Q5_K_M, 262 144 tokens": the other measure's pick in a few words.
pub(super) fn other_words(quant: Option<&str>, file: &str, fit: &FileFit) -> String {
    let name = quant.map_or_else(|| short_file(file), str::to_string);
    match fit.ctx {
        Some(c) => format!("{name}, {}", tokens(c)),
        None => name,
    }
}

/// A file's name without its directories.
pub(super) fn short_file(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// "about 4 minutes", rounded up to the minute, as `hf::rate::sentence`
/// says it ("less than a minute" under one).
pub(super) fn about(secs: u64) -> String {
    if secs < 60 {
        return "less than a minute".into();
    }
    let minutes = secs.div_ceil(60);
    let plural = |n: u64, w: &str| {
        if n == 1 {
            format!("1 {w}")
        } else {
            format!("{n} {w}s")
        }
    };
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("about {}", plural(m, "minute")),
        (h, 0) => format!("about {}", plural(h, "hour")),
        (h, m) => format!("about {} {}", plural(h, "hour"), plural(m, "minute")),
    }
}

/// The seconds `bytes` take at `rate` bytes per second, rounded up.
pub(super) fn secs_at(bytes: u64, rate: Option<f64>) -> Option<u64> {
    rate.filter(|r| *r > 0.0)
        .map(|r| (bytes as f64 / r).ceil() as u64)
}

/// "15.6 GiB download, about 5 minutes on your connection" (§5, D23).
pub(super) fn download_line(bytes: u64, secs: Option<u64>, engine: Option<u64>) -> String {
    let mut line = format!("{} download", fmt_bytes(bytes));
    if let Some(e) = engine.filter(|e| *e > 0) {
        line.push_str(&format!(
            ", and {} the first time for what runs it",
            fmt_bytes(e)
        ));
    }
    match secs {
        Some(s) => line.push_str(&format!(", {} on your connection", about(s))),
        None => line.push_str(" · the time shows once lmgw has measured a download"),
    }
    line
}

/// D125's line: "Needs 15.6 GiB on the disk, 8.5 GiB is free", when short.
pub(super) fn disk_short(needs: u64, free: Option<u64>) -> Option<String> {
    let free = free?;
    (needs > free).then(|| {
        format!(
            "Needs {} on the disk, {} is free",
            fmt_bytes(needs),
            fmt_bytes(free)
        )
    })
}

/// The rate limit's notice while it runs (§4.5), with the number the Find
/// button counts down, and what the hub allows; once it has run out, that
/// Find asks again.
pub(super) fn limit_sentence(secs: Option<u64>, allowance: Option<&str>) -> String {
    let lead = match secs {
        Some(s) => format!("Hugging Face asks lmgw to wait {}.", wait_words(s)),
        None => "Hugging Face's limit has reset: Find asks again.".to_string(),
    };
    match allowance.filter(|a| !a.is_empty()) {
        Some(a) => format!("{lead} {a}"),
        None => lead,
    }
}

/// The Find button's tooltip while it waits: what the hub allows, as the
/// answer said it (with or without the token).
pub(super) fn limit_tip(allowance: Option<&str>) -> String {
    let lead = "Hugging Face limits the requests lmgw sends; Find waits until it allows more.";
    match allowance.filter(|a| !a.is_empty()) {
        Some(a) => format!("{lead} {a}"),
        None => lead.to_string(),
    }
}

/// The wait in words, the same number as [`limit_wait`]: "40 seconds",
/// "3 minutes".
fn wait_words(secs: u64) -> String {
    match secs {
        1 => "1 second".into(),
        s if s < 120 => format!("{s} seconds"),
        s => format!("{} minutes", s.div_ceil(60)),
    }
}

/// The wait the Find button shows while the hub's limit is spent (§4.5):
/// "Hugging Face's limit resets in 40 s".
pub(super) fn limit_wait(secs: u64) -> String {
    if secs >= 120 {
        format!("Hugging Face's limit resets in {} min", secs.div_ceil(60))
    } else {
        format!("Hugging Face's limit resets in {secs} s")
    }
}

/// Whether one more search would meet the hub's limit: an answer that was
/// limited, or fewer requests left than the last search took (§4.5).
pub(super) fn must_wait(limited: bool, remaining: Option<u64>, calls: u32) -> bool {
    limited || remaining.is_some_and(|r| r < u64::from(calls.max(1)))
}

/// One file's progress line: "Qwen3-8B-Q4_K_M.gguf · 2.1 of 5.0 GiB"; a
/// file that was on the disk before the get says so, and one with no bytes
/// yet only its name.
pub(super) fn file_progress(file: &str, done: u64, total: Option<u64>, here: bool) -> String {
    if here {
        return format!("{} · already on the disk", short_file(file));
    }
    match total {
        Some(t) if t > 0 => format!("{} · {}", short_file(file), fmt_bytes_of(done.min(t), t)),
        _ if done == 0 => short_file(file),
        _ => format!("{} · {}", short_file(file), fmt_bytes_so_far(done)),
    }
}

/// The time left at the measured rate: "about 3 minutes left".
pub(super) fn time_left(left: u64, rate: Option<f64>) -> Option<String> {
    secs_at(left, rate).map(|s| match s {
        s if s < 60 => "less than a minute left".to_string(),
        s => format!("{} left", about(s)),
    })
}

/// D9's answer saved as the GPU memory budget: GiB, as lmgw says every
/// size (D160; a card sold as "24 GB" has 24 GiB), as MiB, the unit
/// Settings → GPU keeps it in.
pub(super) fn budget_mib(gib: f64) -> Option<u64> {
    (gib.is_finite() && gib > 0.0).then(|| (gib * 1024.0).round() as u64)
}

/// The stage line of a paused get, from the server's `waiting` (WP7,
/// D106, D108): one sentence per distinct reason, in the order the files
/// come, and no file names (the rows below carry them). The words are the
/// finder's: no button it doesn't have.
pub(super) fn paused_line(waiting: &[GetWaiting]) -> String {
    let mut said: Vec<String> = Vec::new();
    for w in waiting {
        let sentence = match w.reason {
            WaitReason::Metered => "Paused while the connection is metered. It goes on once \
                                    the connection isn't, or from the Downloads page."
                .to_string(),
            WaitReason::NotStarted => "Paused until a download starts.".to_string(),
            WaitReason::Other => match w.note.as_deref().map(str::trim) {
                Some(n) if !n.is_empty() => {
                    let n = n.trim_end_matches('.');
                    format!("Paused: {n}.")
                }
                _ => "Paused.".to_string(),
            },
        };
        if !said.contains(&sentence) {
            said.push(sentence);
        }
    }
    match said.is_empty() {
        true => "Paused.".to_string(),
        false => said.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(mark: FitMark, measure: FitMeasure) -> FileFit {
        FileFit {
            mark,
            measure,
            ..FileFit::default()
        }
    }

    #[test]
    fn the_id_is_the_originals_name_in_lower_case() {
        assert_eq!(model_id_for("Qwen/Qwen3.6-27B"), "qwen3.6-27b");
        assert_eq!(model_id_for("demo-model"), "demo-model");
    }

    #[test]
    fn parameters_in_words() {
        assert_eq!(parameters(27_000_000_000), "27 billion parameters");
        assert_eq!(parameters(1_540_000_000), "1.5 billion parameters");
        assert_eq!(parameters(8_030_000_000), "8 billion parameters");
        assert_eq!(parameters(596_000_000), "596 million parameters");
        assert_eq!(parameters(1_000_000_000_000), "1 trillion parameters");
        assert_eq!(parameters(5_000), "5\u{202F}000 parameters");
    }

    #[test]
    fn the_can_line_starts_with_a_capital() {
        let can = vec![
            "reads pictures".to_string(),
            "thinks before it answers".to_string(),
        ];
        assert_eq!(
            can_line(&can, Some(27_000_000_000)),
            "Reads pictures · thinks before it answers · 27 billion parameters"
        );
        assert_eq!(can_line(&[], Some(600_000_000)), "600 million parameters");
        assert_eq!(can_line(&[], None), "");
    }

    #[test]
    fn the_chip_leads_with_beside_and_says_who_makes_room() {
        let who = "other programs close";
        assert_eq!(
            chip(
                &fit(FitMark::Fits, FitMeasure::Beside),
                who,
                Checked::Header
            ),
            ("fits your graphics card".into(), "ok")
        );
        assert_eq!(
            chip(
                &fit(FitMark::FitsShorter, FitMeasure::WholeCard),
                who,
                Checked::Header
            )
            .0,
            "fits once other programs close"
        );
        assert_eq!(
            chip(
                &fit(FitMark::FitsBySize, FitMeasure::Beside),
                who,
                Checked::Checking
            )
            .0,
            "fits by size · checking…"
        );
        assert_eq!(
            chip(
                &fit(FitMark::FitsBySize, FitMeasure::Beside),
                who,
                Checked::BySize
            )
            .0,
            "fits by size · not checked"
        );
        assert_eq!(
            chip(
                &fit(FitMark::DoesNotFit, FitMeasure::WholeCard),
                who,
                Checked::Header
            )
            .0,
            "doesn't fit your graphics card"
        );
        assert_eq!(
            chip(
                &fit(FitMark::NoCard, FitMeasure::WholeCard),
                who,
                Checked::BySize
            )
            .0,
            "can't measure your graphics card yet"
        );
    }

    #[test]
    fn the_context_line_is_the_fit_and_an_embedders_purpose() {
        let mut f = fit(FitMark::FitsShorter, FitMeasure::Beside);
        f.ctx = Some(65_536);
        f.trained_ctx = Some(262_144);
        assert_eq!(
            context_line(CardKind::Chat, &f, Checked::Header),
            "How long a conversation it can remember · context, 65\u{202F}536 of 262\u{202F}144 tokens"
        );
        f.mark = FitMark::Fits;
        f.ctx = Some(131_072);
        assert_eq!(
            context_line(CardKind::Chat, &f, Checked::Header),
            "How long a conversation it can remember · context, 131\u{202F}072 tokens"
        );
        let by_size = fit(FitMark::FitsBySize, FitMeasure::Beside);
        assert_eq!(
            context_line(CardKind::Chat, &by_size, Checked::Checking),
            "How long a conversation it can remember · context: checking…"
        );
        assert_eq!(
            context_line(CardKind::Embed, &by_size, Checked::Checking),
            "For searching your files (Knowledge)"
        );
    }

    #[test]
    fn the_uses_line_names_the_measure() {
        const GIB: u64 = 1 << 30;
        let mut f = fit(FitMark::Fits, FitMeasure::Beside);
        f.uses_bytes = 16 * GIB + GIB / 2;
        f.headroom_bytes = GIB / 2;
        f.capacity_bytes = 18 * GIB;
        assert_eq!(
            uses_line(&f).unwrap(),
            "Uses 17.0 GiB of the 18.0 GiB free on your graphics card beside what runs now"
        );
        f.measure = FitMeasure::WholeCard;
        f.mark = FitMark::FitsBySize;
        assert_eq!(
            uses_line(&f).unwrap(),
            "Needs about 17.0 GiB of the 18.0 GiB your graphics card has"
        );
        assert!(uses_line(&fit(FitMark::NoCard, FitMeasure::Beside)).is_none());
        f.mark = FitMark::DoesNotFitBySize;
        assert_eq!(
            uses_line(&f).unwrap(),
            "Needs 17.0 GiB on your graphics card, more than the 18.0 GiB it has"
        );
        // D9's "24" is GiB, as the question asks: the card then says 24.0 GiB.
        f.capacity_bytes = budget_mib(24.0).unwrap() * 1024 * 1024;
        assert!(uses_line(&f).unwrap().ends_with("the 24.0 GiB it has"));
    }

    #[test]
    fn times_round_up_to_the_minute() {
        assert_eq!(about(59), "less than a minute");
        assert_eq!(about(61), "about 2 minutes");
        assert_eq!(about(3600), "about 1 hour");
        assert_eq!(about(3601), "about 1 hour 1 minute");
        assert_eq!(secs_at(1_000, Some(100.0)), Some(10));
        assert_eq!(secs_at(1_000, None), None);
        assert_eq!(
            download_line(16_800_000_000, Some(290), None),
            "15.6 GiB download, about 5 minutes on your connection"
        );
        assert_eq!(
            time_left(30_000, Some(1_000.0)).as_deref(),
            Some("less than a minute left")
        );
    }

    #[test]
    fn the_disk_line_only_when_short() {
        assert_eq!(
            disk_short(16_800_000_000, Some(9_100_000_000)).as_deref(),
            Some("Needs 15.6 GiB on the disk, 8.5 GiB is free")
        );
        assert!(disk_short(1, Some(2)).is_none());
        assert!(disk_short(1, None).is_none());
    }

    #[test]
    fn the_wait_starts_below_one_more_search() {
        assert!(must_wait(true, Some(400), 6));
        assert!(must_wait(false, Some(5), 6));
        assert!(!must_wait(false, Some(6), 6));
        assert!(!must_wait(false, None, 6));
        assert!(must_wait(false, Some(0), 0));
        assert_eq!(limit_wait(40), "Hugging Face's limit resets in 40 s");
        assert_eq!(limit_wait(150), "Hugging Face's limit resets in 3 min");
    }

    #[test]
    fn the_limits_notice_says_the_buttons_number() {
        let a = "Without an account it allows 500 requests every 5 minutes.";
        assert_eq!(
            limit_sentence(Some(38), Some(a)),
            format!("Hugging Face asks lmgw to wait 38 seconds. {a}")
        );
        assert_eq!(
            limit_sentence(Some(150), None),
            "Hugging Face asks lmgw to wait 3 minutes."
        );
        assert!(limit_wait(150).ends_with("3 min"));
        assert_eq!(
            limit_sentence(None, None),
            "Hugging Face's limit has reset: Find asks again."
        );
        assert!(!limit_tip(Some(
            "With your Hugging Face token it allows 1000 requests every 5 minutes."
        ))
        .contains("without an account"));
    }

    #[test]
    fn a_files_line_says_what_is_known_of_it() {
        assert_eq!(
            file_progress("d/Demo-Q4.gguf", 2_100_000_000, Some(5_000_000_000), false),
            "Demo-Q4.gguf · 1.9 of 4.7 GiB"
        );
        assert_eq!(
            file_progress("d/Demo-Q4.gguf", 2_100_000_000, None, false),
            "Demo-Q4.gguf · 1.9 GiB"
        );
        assert_eq!(
            file_progress("mmproj.gguf", 0, None, true),
            "mmproj.gguf · already on the disk"
        );
        assert_eq!(file_progress("mmproj.gguf", 0, None, false), "mmproj.gguf");
    }

    #[test]
    fn a_paused_line_says_each_reason_once_without_the_files() {
        let w = |file: &str, reason, note: Option<&str>| GetWaiting {
            file: file.into(),
            reason,
            note: note.map(str::to_string),
        };
        // The note is never read for a reason the server typed: the
        // metered pause's own note (the core test `a_waiting_rows_note_is_
        // its_reason` maps the real one) says "Resume", a button the finder
        // lacks.
        let metered = "Paused: note-demo; Resume goes on";
        let line = paused_line(&[
            w("Demo-8B-Q4_K_M.gguf", WaitReason::Metered, Some(metered)),
            w(
                "mmproj-Demo-8B-F16.gguf",
                WaitReason::Metered,
                Some(metered),
            ),
        ]);
        assert_eq!(
            line,
            "Paused while the connection is metered. It goes on once the connection isn't, \
             or from the Downloads page."
        );
        assert!(
            !line.contains("Resume") && !line.contains(".gguf"),
            "{line}"
        );
        assert_eq!(
            paused_line(&[
                w("a.gguf", WaitReason::Metered, Some(metered)),
                w("b.gguf", WaitReason::NotStarted, None),
            ]),
            "Paused while the connection is metered. It goes on once the connection isn't, \
             or from the Downloads page. Paused until a download starts."
        );
        // Another note is said whole, its "; " kept.
        assert_eq!(
            paused_line(&[w(
                "a.gguf",
                WaitReason::Other,
                Some("the hub answered 503; lmgw tries again.")
            )]),
            "Paused: the hub answered 503; lmgw tries again."
        );
        assert_eq!(paused_line(&[]), "Paused.");
    }

    #[test]
    fn the_budget_is_kept_in_mib() {
        assert_eq!(budget_mib(24.0), Some(24_576));
        assert_eq!(budget_mib(0.0), None);
        assert_eq!(budget_mib(f64::NAN), None);
    }
}
