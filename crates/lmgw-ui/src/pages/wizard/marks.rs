//! A file's mark in the Hugging Face file list, in words (find-a-model
//! design §3.3, §3.5, D39, D120): what the chip says and what its tooltip
//! adds, from the fit `hf_repo` with `fit` answers, or a plan's after the
//! file was read exactly, and the lines above the list.
//!
//! The chip leads with the measure the card leads with, beside what runs on
//! the graphics card now (D120). A file that fits only once the other
//! programs close, or lmgw's other models unload, says that; the whole
//! card's numbers are then the tooltip's. The recommended file when nothing
//! qualifies beside what runs now is the whole card's pick, and its chip
//! and the line above the list both say it fits once those make room.

use lmgw_api_types::model_find::{CardMeasure, FileFit, FitMark, FitMeasure, PlanFit};
use lmgw_api_types::RepoFiles;

use crate::fmt::{fmt_bytes, grouped};

/// One file's mark.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mark {
    /// The chip's words.
    pub text: String,
    /// The chip's class: `ok`, `warn` or `off`.
    pub tone: &'static str,
    /// The tooltip: the numbers behind the words, one line each.
    pub tip: String,
}

/// "262 144 tokens".
fn tokens(n: u64) -> String {
    format!("{} tokens", grouped(n))
}

/// The words of a fit by its header (`Fits` or `FitsShorter`). An
/// estimate's shorter conversation is a floor: the file's own header may
/// give a longer one, never a shorter one.
fn fits_words(f: &FileFit, estimated: bool) -> String {
    match (f.mark, f.ctx, f.trained_ctx) {
        (FitMark::FitsShorter, Some(c), Some(t)) => format!(
            "fits · {}{} of {}",
            if estimated { "at least " } else { "" },
            grouped(c),
            tokens(t)
        ),
        (_, Some(c), _) => format!("fits · {}", tokens(c)),
        _ => "fits".into(),
    }
}

/// Who has to make room on `card` for a file that fits only the whole
/// card: other programs only when the card says they hold some, lmgw's
/// models when it has some on the card. What other programs use not being
/// known, only lmgw's models are named when there are any (the beside
/// measure left out nothing else).
pub(crate) fn who(card: Option<&CardMeasure>) -> &'static str {
    let (outside, ours) = card.map_or((None, false), |c| {
        (c.outside_bytes, !c.residents.is_empty())
    });
    who_of(outside, ours)
}

/// [`who`] from what is beside a file: what other programs hold (`None`
/// when not known) and whether lmgw has models on the card.
pub(crate) fn who_of(outside: Option<u64>, ours: bool) -> &'static str {
    match (outside, ours) {
        (Some(o), true) if o > 0 => "other programs close and lmgw's other models unload",
        (_, true) => "lmgw's other models unload",
        _ => "other programs close",
    }
}

/// Which measure a file's chip leads with: beside what runs now (D120),
/// but the whole card for the recommended file when it is the whole card's
/// pick because nothing qualifies beside what runs now, so the chip says
/// what the line above the list says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lead {
    Beside,
    WholeCard,
}

/// The mark of a file with `fit` (module doc), its `card` filled in;
/// `estimated_from` is the file whose header it was estimated from. `None`
/// without a card to measure against (D9): no marks then, the list's line
/// says why.
pub(crate) fn mark(fit: &PlanFit, estimated_from: Option<&str>, lead: Lead) -> Option<Mark> {
    let (b, w) = (&fit.beside, &fit.whole_card);
    if b.mark == FitMark::NoCard {
        return None;
    }
    let est = estimated_from.is_some();
    let who = who(fit.card.as_ref());
    let (text, tone, shown) = match b.mark {
        _ if lead == Lead::WholeCard && matches!(w.mark, FitMark::Fits | FitMark::FitsShorter) => {
            (format!("{} once {who}", fits_words(w, est)), "warn", w)
        }
        FitMark::Fits | FitMark::FitsShorter => (fits_words(b, est), "ok", b),
        FitMark::FitsBySize => ("fits by size · not checked".to_string(), "off", b),
        _ if w.mark.fits() => (
            match w.mark {
                FitMark::FitsBySize => format!("fits by size once {who} · not checked"),
                _ => format!("fits once {who}"),
            },
            "warn",
            w,
        ),
        FitMark::Unknown => ("size unknown · not checked".to_string(), "off", b),
        _ => ("doesn't fit".to_string(), "off", w),
    };
    let mut tip = tip(shown, estimated_from);
    if shown.measure == FitMeasure::WholeCard && b.mark.fits() {
        tip.push_str(&format!(
            "\nBeside what runs now: {}",
            match b.mark {
                FitMark::FitsBySize => "fits by size".to_string(),
                _ => fits_words(b, est),
            }
        ));
    }
    Some(Mark { text, tone, tip })
}

/// The lines above the list once the marks are in (D64, D120): the
/// recommended file and why, saying it fits once others make room when it
/// is the whole card's pick, the fold's sentence, the whole card's pick
/// when it differs, and what holds for the whole list. `short` names a
/// file for a person.
pub(super) fn list_lines(a: &RepoFiles, short: impl Fn(&str) -> String) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let who = who(a.card.as_ref());
    if let Some(p) = &a.picks {
        let beside_pick = a.recommended.is_some() && a.recommended == p.beside.file;
        match (a.recommended.as_deref(), beside_pick) {
            (Some(r), true) => {
                lines.push(format!("Recommended: {}. {}", short(r), p.beside.why));
                if let Some(full) = &p.beside.full_context {
                    lines.push(full.sentence.clone());
                }
                if let Some(w) = p.whole_card.file.as_deref().filter(|w| *w != r) {
                    lines.push(format!("Once {who}: {}.", short(w)));
                }
            }
            (Some(r), false) => {
                lines.push(format!(
                    "Recommended once {who}: {}. {}",
                    short(r),
                    p.whole_card.why
                ));
                if let Some(full) = &p.whole_card.full_context {
                    lines.push(full.sentence.clone());
                }
                lines.push(p.beside.why.clone());
            }
            (None, _) if a.card.is_some() => lines.push(p.beside.why.clone()),
            (None, _) => {}
        }
    }
    lines.extend(a.fit_notes.iter().cloned());
    lines
}

/// The numbers behind `f`'s mark, one line each (§3.3's tooltip).
pub(crate) fn tip(f: &FileFit, estimated_from: Option<&str>) -> String {
    let gb = |b: u64| fmt_bytes(b);
    let mut lines = Vec::new();
    let room = match f.measure {
        FitMeasure::WholeCard => format!("the {} on your graphics card", gb(f.capacity_bytes)),
        FitMeasure::Beside => format!("the {} free beside what runs now", gb(f.capacity_bytes)),
    };
    let by_size = matches!(
        f.mark,
        FitMark::FitsBySize | FitMark::DoesNotFitBySize | FitMark::Unknown
    );
    if by_size {
        lines.push(format!(
            "By its size alone: {} of {room}, with {} kept free",
            gb(f.uses_bytes),
            gb(f.headroom_bytes)
        ));
    } else {
        lines.push(format!(
            "Uses {} of {room}{}: weights {}, the conversation {}, working memory {}; {} kept \
             free",
            gb(f.uses_bytes),
            if f.mark == FitMark::DoesNotFit {
                " even at the shortest conversation"
            } else {
                ""
            },
            gb(f.terms.weights_bytes),
            gb(f.terms.kv_cache_bytes),
            gb(f.terms.compute_bytes),
            gb(f.headroom_bytes)
        ));
    }
    if let Some(b) = &f.beside {
        lines.push(format!(
            "Beside it now: other programs {}, lmgw's models {}",
            b.outside_bytes.map_or("unknown".to_string(), gb),
            gb(b.residents_bytes)
        ));
    }
    if let Some(c) = f.ctx {
        let can_take = match f.trained_ctx {
            Some(t) if t > c => format!(" · the model can take {}", grouped(t)),
            _ => String::new(),
        };
        lines.push(format!(
            "How long a conversation it can remember · context, {}{can_take}",
            tokens(c)
        ));
    }
    if let Some(bits) = f.bits_per_weight {
        lines.push(format!("{bits:.1} bits per weight"));
    }
    if let Some(from) = estimated_from {
        lines.push(format!(
            "Estimated from the header of {from}, never less than the real use. Choosing \
             this file reads its own header."
        ));
    }
    lines.extend(
        f.notes
            .iter()
            .filter(|n| !n.starts_with("estimated from the header of"))
            .cloned(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::model_find::{BesideTerms, CardMeasure, CardResident, FitTerms};

    const GB: u64 = 1 << 30;

    fn file(mark: FitMark, measure: FitMeasure, ctx: Option<u64>) -> FileFit {
        FileFit {
            mark,
            measure,
            ctx,
            trained_ctx: Some(262_144),
            uses_bytes: 21 * GB,
            capacity_bytes: 24 * GB,
            headroom_bytes: GB / 2,
            terms: FitTerms {
                weights_bytes: 16 * GB,
                kv_cache_bytes: 4 * GB,
                compute_bytes: GB,
            },
            beside: (measure == FitMeasure::Beside).then_some(BesideTerms {
                outside_bytes: Some(6 * GB),
                residents_bytes: 0,
            }),
            bits_per_weight: Some(4.85),
            ..FileFit::default()
        }
    }

    fn fit(whole: FileFit, beside: FileFit, residents: bool) -> PlanFit {
        fit_outside(whole, beside, residents, Some(6 * GB))
    }

    fn fit_outside(
        whole: FileFit,
        beside: FileFit,
        residents: bool,
        outside: Option<u64>,
    ) -> PlanFit {
        PlanFit {
            card: Some(CardMeasure {
                capacity_bytes: 24 * GB,
                outside_bytes: outside,
                residents: if residents {
                    vec![CardResident {
                        class: "audio".into(),
                        model: "demo-voice".into(),
                        bytes: 2 * GB,
                    }]
                } else {
                    Vec::new()
                },
                ..CardMeasure::default()
            }),
            whole_card: whole,
            beside,
        }
    }

    #[test]
    fn the_marks_read_as_the_design_writes_them() {
        use FitMark::*;
        use FitMeasure::*;
        let all = fit(
            file(Fits, WholeCard, Some(262_144)),
            file(Fits, Beside, Some(262_144)),
            false,
        );
        let m = mark(&all, None, Lead::Beside).unwrap();
        assert_eq!(
            (m.text.as_str(), m.tone),
            ("fits · 262\u{202F}144 tokens", "ok")
        );
        assert!(m
            .tip
            .contains("Uses 21.0 GiB of the 24.0 GiB free beside what runs now"));
        assert!(m
            .tip
            .contains("weights 16.0 GiB, the conversation 4.0 GiB, working memory 1.0 GiB"));
        assert!(m.tip.contains("other programs 6.0 GiB"), "{}", m.tip);
        assert!(m.tip.contains("4.8 bits per weight") || m.tip.contains("4.9 bits per weight"));

        let short = fit(
            file(Fits, WholeCard, Some(262_144)),
            file(FitsShorter, Beside, Some(65_536)),
            false,
        );
        assert_eq!(
            mark(&short, None, Lead::Beside).unwrap().text,
            "fits · 65\u{202F}536 of 262\u{202F}144 tokens"
        );
        let est = mark(&short, Some("demo-Q4_K_M.gguf"), Lead::Beside).unwrap();
        assert_eq!(
            est.text,
            "fits · at least 65\u{202F}536 of 262\u{202F}144 tokens"
        );
        assert!(est
            .tip
            .contains("Estimated from the header of demo-Q4_K_M.gguf"));
        assert!(est
            .tip
            .contains("context, 65\u{202F}536 tokens · the model can take 262\u{202F}144"));

        let once = |residents| {
            fit(
                file(FitsShorter, WholeCard, Some(131_072)),
                file(DoesNotFit, Beside, None),
                residents,
            )
        };
        let m = mark(&once(false), None, Lead::Beside).unwrap();
        assert_eq!(
            (m.text.as_str(), m.tone),
            ("fits once other programs close", "warn")
        );
        assert!(
            m.tip.contains("of the 24.0 GiB on your graphics card"),
            "{}",
            m.tip
        );
        assert_eq!(
            mark(&once(true), None, Lead::Beside).unwrap().text,
            "fits once other programs close and lmgw's other models unload"
        );

        let none = fit(
            file(DoesNotFit, WholeCard, None),
            file(DoesNotFit, Beside, None),
            false,
        );
        let m = mark(&none, None, Lead::Beside).unwrap();
        assert_eq!((m.text.as_str(), m.tone), ("doesn't fit", "off"));
        assert!(m.tip.contains("even at the shortest conversation"));

        let by_size = fit(
            file(FitsBySize, WholeCard, None),
            file(FitsBySize, Beside, None),
            false,
        );
        let m = mark(&by_size, None, Lead::Beside).unwrap();
        assert_eq!(m.text, "fits by size · not checked");
        assert!(m.tip.starts_with("By its size alone"), "{}", m.tip);

        let no_card = fit(
            file(NoCard, WholeCard, None),
            file(NoCard, Beside, None),
            false,
        );
        assert_eq!(mark(&no_card, None, Lead::Beside), None);
    }

    /// Outside use unknown with lmgw's models on the card: only they are
    /// named (review 6).
    #[test]
    fn who_makes_room_is_what_the_card_says() {
        use FitMark::*;
        use FitMeasure::*;
        let once = |residents, outside| {
            let f = fit_outside(
                file(FitsShorter, WholeCard, Some(131_072)),
                file(DoesNotFit, Beside, None),
                residents,
                outside,
            );
            mark(&f, None, Lead::Beside).unwrap().text
        };
        assert_eq!(once(true, None), "fits once lmgw's other models unload");
        assert_eq!(once(true, Some(0)), "fits once lmgw's other models unload");
        assert_eq!(
            once(true, Some(GB)),
            "fits once other programs close and lmgw's other models unload"
        );
        assert_eq!(once(false, Some(GB)), "fits once other programs close");
    }

    /// The whole card's pick, recommended because nothing qualifies beside
    /// what runs now: its chip and the line both say it fits once the other
    /// programs close, with the whole card's number (review 5, D120).
    #[test]
    fn the_whole_cards_pick_reads_the_same_in_its_chip_and_the_line() {
        use lmgw_api_types::model_find::{FilePick, PickRule};
        use FitMark::*;
        use FitMeasure::*;
        let mut w = file(FitsShorter, WholeCard, Some(39_168));
        w.trained_ctx = Some(40_960);
        let mut b = file(FitsShorter, Beside, Some(23_808));
        b.trained_ctx = Some(40_960);
        let f = fit(w, b, false);
        let chip = mark(&f, None, Lead::WholeCard).unwrap();
        assert_eq!(
            (chip.text.as_str(), chip.tone),
            (
                "fits · 39\u{202F}168 of 40\u{202F}960 tokens once other programs close",
                "warn"
            )
        );
        assert!(chip.tip.contains("on your graphics card"), "{}", chip.tip);
        assert!(
            chip.tip
                .contains("Beside what runs now: fits · 23\u{202F}808 of 40\u{202F}960"),
            "{}",
            chip.tip
        );
        // Led by the beside measure it reads as before.
        assert_eq!(
            mark(&f, None, Lead::Beside).unwrap().text,
            "fits · 23\u{202F}808 of 40\u{202F}960 tokens"
        );
        let a = RepoFiles {
            recommended: Some("q/m-IQ4_XS.gguf".into()),
            card: f.card.clone(),
            picks: Some(lmgw_api_types::RepoPicks {
                whole_card: FilePick {
                    file: Some("q/m-IQ4_XS.gguf".into()),
                    rule: PickRule::ShorterContext,
                    measure: WholeCard,
                    why: "it fits 39 168 tokens.".into(),
                    full_context: None,
                },
                beside: FilePick {
                    measure: Beside,
                    why: "No file of this model fits beside what's on your graphics card.".into(),
                    ..FilePick::default()
                },
            }),
            fit_notes: vec!["a note".into()],
            ..RepoFiles::default()
        };
        let short = |p: &str| p.rsplit('/').next().unwrap().to_string();
        let lines = list_lines(&a, short);
        assert_eq!(
            lines,
            vec![
                "Recommended once other programs close: m-IQ4_XS.gguf. it fits 39 168 tokens."
                    .to_string(),
                "No file of this model fits beside what's on your graphics card.".into(),
                "a note".into(),
            ]
        );
    }
}
