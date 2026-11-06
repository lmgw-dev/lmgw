//! What the card shows (find-a-model design §5, D120, D123): one file and
//! its fit by one measure, from `model_find`'s card by size, then from the
//! header's marks (`hf_repo` with `fit` on the card's upload), then from a
//! file chosen by hand in the file list (D34). Pure, so the rules are
//! unit-tested without a browser.

use lmgw_api_types::model_find::{
    CardMeasure, FileFit, FilePick, FitMark, FitMeasure, ModelCard, PlanFit,
};
use lmgw_api_types::RepoFiles;

use crate::pages::wizard::PickedFile;

/// Which file the card shows: the pick, the other measure's pick ("Use
/// this one instead", D120), or one chosen in the file list.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) enum Choice {
    #[default]
    Pick,
    /// The other measure's pick, when the two differ.
    Other,
    ByHand(Box<PickedFile>),
}

/// The other measure's file, offered in one line under the card's (D120).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Other {
    pub file: String,
    pub quant: Option<String>,
    pub fit: FileFit,
}

/// The card's file and everything its lines say about it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Shown {
    /// The upload the file is from.
    pub upload: String,
    pub file: String,
    pub quant: Option<String>,
    /// The class a file chosen by hand was listed for; `None` lets Get it
    /// read it from the header (D83).
    pub target: Option<String>,
    /// The fit the card leads with, by `fit.measure` (D120): what Get it
    /// fits to.
    pub fit: FileFit,
    /// The fit is a header's, not an estimate or a size.
    pub exact: bool,
    /// The card it was measured against, when the marks name it.
    pub card: Option<CardMeasure>,
    /// The other measure's pick, when it differs.
    pub other: Option<Other>,
    /// "Why this file" (§3.2).
    pub why: String,
    /// The smaller file that keeps the whole conversation, when this one
    /// fits a shorter one (D7).
    pub full_context: Option<String>,
    /// The download's bytes: the file and its companions; `None` when not
    /// known.
    pub bytes: Option<u64>,
    /// The header's parameter count, else the listing's.
    pub parameters: Option<u64>,
    pub bits_per_weight: Option<f64>,
    pub by_hand: bool,
}

/// The measure a file leads with: beside what runs now when it fits
/// there, else the whole card (D120); a file that fits neither is said
/// against the whole card, its size against the card's (§4.5).
fn lead(fit: &PlanFit, whole: bool) -> FileFit {
    if !whole && fit.beside.mark.fits() {
        fit.beside.clone()
    } else if whole || fit.whole_card.mark != FitMark::NoCard {
        fit.whole_card.clone()
    } else {
        fit.beside.clone()
    }
}

fn exact(fit: &FileFit, estimated: bool) -> bool {
    !estimated
        && !matches!(
            fit.mark,
            FitMark::FitsBySize | FitMark::DoesNotFitBySize | FitMark::Unknown | FitMark::NoCard
        )
}

/// A file's entry in the marks' answer, its fit with the list's card put
/// back (D63).
fn entry<'a>(
    a: &'a RepoFiles,
    file: &str,
) -> Option<(&'a lmgw_api_types::RepoFile, Option<PlanFit>)> {
    let f = a.files.iter().find(|f| f.file == file)?;
    let fit = f.fit.clone().map(|mut p| {
        if p.card.is_none() {
            p.card = a.card.clone();
        }
        p
    });
    Some((f, fit))
}

/// The file's bytes with the card's companions, by the listing.
fn bytes_with_companions(a: &RepoFiles, card: &ModelCard, size: u64) -> u64 {
    card.companions
        .iter()
        .filter_map(|c| a.files.iter().find(|f| f.file == c.file))
        .map(|f| f.size_bytes)
        .fold(size, u64::saturating_add)
}

/// The pick of `p` that names `file`.
fn pick_of<'a>(a: &'a RepoFiles, file: &str) -> Option<&'a FilePick> {
    let p = a.picks.as_ref()?;
    [&p.beside, &p.whole_card]
        .into_iter()
        .find(|k| k.file.as_deref() == Some(file))
}

/// What the card shows for `card`, with the header's marks when they are in
/// (`marks`, for the card's upload) and the person's `choice`.
pub(super) fn shown(card: &ModelCard, marks: Option<&RepoFiles>, choice: &Choice) -> Shown {
    if let Choice::ByHand(p) = choice {
        return by_hand(card, marks, p);
    }
    match marks.filter(|a| a.repo == card.upload) {
        Some(a) => from_marks(card, a, *choice == Choice::Other),
        None => from_card(card, *choice == Choice::Other),
    }
}

/// The card by size, before the header's marks (D123).
fn from_card(card: &ModelCard, other_chosen: bool) -> Shown {
    let own = Other {
        file: card.file.clone(),
        quant: card.quant.clone(),
        fit: card.fit.clone(),
    };
    let whole = card.whole_card.as_ref().map(|w| Other {
        file: w.file.clone(),
        quant: w.quant.clone(),
        fit: w.fit.clone(),
    });
    // The other pick's companions aren't listed before the marks: its own
    // bytes stand in until they are in.
    let (main, other, bytes) = match whole {
        Some(w) if other_chosen => {
            let b = card.whole_card.as_ref().map(|c| c.bytes);
            (w, Some(own), b)
        }
        w => (own, w, Some(card.download.files_bytes)),
    };
    Shown {
        upload: card.upload.clone(),
        file: main.file,
        quant: main.quant,
        target: None,
        exact: false,
        card: None,
        other,
        why: card.why.clone(),
        full_context: None,
        bytes: bytes.filter(|b| *b > 0),
        parameters: main.fit.parameters.or(card.parameters),
        bits_per_weight: main.fit.bits_per_weight,
        fit: main.fit,
        by_hand: false,
    }
}

/// Whether the header positively says `fit` fits on neither measure: a
/// mark by size, an unknown one or none at all is no such word.
fn fits_nowhere(fit: &PlanFit) -> bool {
    fit.beside.mark == FitMark::DoesNotFit && fit.whole_card.mark == FitMark::DoesNotFit
}

/// The card from the header's marks: the recommended file (D123), or the
/// card's own when nothing is recommended (a metered connection, reads the
/// hub stopped, no file that qualifies).
fn from_marks(card: &ModelCard, a: &RepoFiles, other_chosen: bool) -> Shown {
    // The file a pasted address named stays the card's unless its header
    // positively says it fits nowhere (WP7, D100 revised): an entry without a
    // fit (a header that couldn't be read) or missing from the list keeps it.
    let named = card.named_file.as_deref().filter(|n| {
        !entry(a, n)
            .and_then(|(_, fit)| fit)
            .is_some_and(|p| fits_nowhere(&p))
    });
    let whole_pick = a.picks.as_ref().and_then(|p| p.whole_card.file.clone());
    let beside_pick = a.picks.as_ref().and_then(|p| p.beside.file.clone());
    // The whole card's file, when it isn't the beside one too.
    let is_whole = |f: &str| whole_pick.as_deref() == Some(f) && beside_pick.as_deref() != Some(f);
    let base = named
        .map(str::to_string)
        .or_else(|| a.recommended.clone())
        .unwrap_or_else(|| card.file.clone());
    // The other line's file for the base (D120): under a pick, the other
    // measure's pick; under a named file that is neither, the pick of the
    // measure it doesn't lead with, else the other pick.
    let alt = {
        let base_lead = entry(a, &base)
            .and_then(|(_, fit)| fit)
            .map(|p| lead(&p, is_whole(&base)).measure);
        let order = match named.is_some() && base_lead == Some(FitMeasure::WholeCard) {
            true => [beside_pick.clone(), whole_pick.clone()],
            false => [whole_pick.clone(), beside_pick.clone()],
        };
        order.into_iter().flatten().find(|o| *o != base)
    };
    // "Use this one instead" shows the other line's file, and its own other
    // line is the way back to the base.
    let (file, back) = match alt {
        Some(o) if other_chosen => (o, Some(base)),
        alt => (base, alt),
    };
    let Some((f, fit)) = entry(a, &file) else {
        return from_card(card, other_chosen);
    };
    let main = match &fit {
        Some(p) => lead(p, is_whole(&file)),
        None => card.fit.clone(),
    };
    let other = back.and_then(|o| {
        let (of, ofit) = entry(a, &o)?;
        let ofit = ofit?;
        Some(Other {
            quant: of.quant.clone(),
            fit: lead(&ofit, is_whole(&o)),
            file: o,
        })
    });
    let pick = pick_of(a, &file);
    Shown {
        upload: card.upload.clone(),
        quant: f.quant.clone(),
        target: None,
        exact: exact(&main, f.estimated_from.is_some()),
        card: a.card.clone(),
        other,
        why: pick.map_or_else(|| card.why.clone(), |p| p.why.clone()),
        full_context: pick.and_then(|p| p.full_context.as_ref().map(|c| c.sentence.clone())),
        bytes: Some(bytes_with_companions(a, card, f.size_bytes)).filter(|b| *b > 0),
        parameters: main.parameters.or(card.parameters),
        bits_per_weight: main.bits_per_weight,
        fit: main,
        file,
        by_hand: false,
    }
}

/// A file chosen in the file list (D34): its own fit, as the list read it.
fn by_hand(card: &ModelCard, marks: Option<&RepoFiles>, p: &PickedFile) -> Shown {
    let fit = match &p.fit {
        Some(f) => lead(f, false),
        None => FileFit {
            mark: if card.fit.mark == FitMark::NoCard {
                FitMark::NoCard
            } else {
                FitMark::Unknown
            },
            ..FileFit::default()
        },
    };
    let same_repo = marks.filter(|a| a.repo == p.repo);
    let bytes = match same_repo {
        Some(a) => bytes_with_companions(a, card, p.size_bytes),
        None => p.size_bytes,
    };
    Shown {
        upload: p.repo.clone(),
        file: p.file.clone(),
        quant: p.quant.clone(),
        target: Some(p.target.clone()),
        exact: exact(&fit, false),
        card: p.fit.as_ref().and_then(|f| f.card.clone()),
        other: None,
        why: "You chose this file in the file list.".into(),
        full_context: None,
        bytes: Some(bytes).filter(|b| *b > 0),
        parameters: fit.parameters.or(card.parameters),
        bits_per_weight: fit.bits_per_weight,
        fit,
        by_hand: true,
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::model_find::{CardFile, PickRule};
    use lmgw_api_types::{HfCompanion, RepoFile, RepoPicks};

    use super::*;

    fn ff(mark: FitMark, measure: FitMeasure, ctx: Option<u64>) -> FileFit {
        FileFit {
            mark,
            measure,
            ctx,
            ..FileFit::default()
        }
    }

    fn card() -> ModelCard {
        ModelCard {
            name: "Demo 8B".into(),
            origin: "demo-org/Demo-8B".into(),
            upload: "demo-org/Demo-8B-GGUF".into(),
            file: "Demo-8B-Q4_K_M.gguf".into(),
            quant: Some("Q4_K_M".into()),
            companions: vec![HfCompanion {
                file: "mmproj-F16.gguf".into(),
                role: "mmproj".into(),
            }],
            fit: ff(FitMark::FitsBySize, FitMeasure::Beside, None),
            whole_card: Some(CardFile {
                file: "Demo-8B-Q8_0.gguf".into(),
                quant: Some("Q8_0".into()),
                bytes: 8_500_000_000,
                fit: ff(FitMark::FitsBySize, FitMeasure::WholeCard, None),
            }),
            why: "by size".into(),
            download: lmgw_api_types::model_find::DownloadEstimate {
                files_bytes: 5_900_000_000,
                ..Default::default()
            },
            ..ModelCard::default()
        }
    }

    fn file(name: &str, size: u64, fit: Option<PlanFit>) -> RepoFile {
        RepoFile {
            file: name.into(),
            role: "weights".into(),
            size_bytes: size,
            fit,
            ..RepoFile::default()
        }
    }

    fn plan(beside: FileFit, whole: FileFit) -> PlanFit {
        PlanFit {
            card: None,
            beside,
            whole_card: whole,
        }
    }

    fn marks(rec: &str, beside: Option<&str>, whole: Option<&str>) -> RepoFiles {
        let fits = |m| ff(FitMark::Fits, m, Some(32_768));
        let no = |m| ff(FitMark::DoesNotFit, m, None);
        RepoFiles {
            repo: "demo-org/Demo-8B-GGUF".into(),
            files: vec![
                file(
                    "Demo-8B-Q4_K_M.gguf",
                    5_000_000_000,
                    Some(plan(fits(FitMeasure::Beside), fits(FitMeasure::WholeCard))),
                ),
                file(
                    "Demo-8B-Q8_0.gguf",
                    8_500_000_000,
                    Some(plan(no(FitMeasure::Beside), fits(FitMeasure::WholeCard))),
                ),
                RepoFile {
                    file: "mmproj-F16.gguf".into(),
                    role: "mmproj".into(),
                    size_bytes: 900_000_000,
                    ..RepoFile::default()
                },
            ],
            recommended: Some(rec.into()),
            picks: Some(RepoPicks {
                beside: FilePick {
                    file: beside.map(str::to_string),
                    rule: PickRule::WholeContext,
                    measure: FitMeasure::Beside,
                    why: "beside why".into(),
                    full_context: None,
                },
                whole_card: FilePick {
                    file: whole.map(str::to_string),
                    rule: PickRule::WholeContext,
                    measure: FitMeasure::WholeCard,
                    why: "whole why".into(),
                    full_context: None,
                },
            }),
            ..RepoFiles::default()
        }
    }

    #[test]
    fn before_the_marks_the_card_is_by_size_with_its_other_pick() {
        let s = shown(&card(), None, &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q4_K_M.gguf");
        assert!(!s.exact);
        assert_eq!(s.bytes, Some(5_900_000_000));
        assert_eq!(s.other.as_ref().unwrap().file, "Demo-8B-Q8_0.gguf");
        let o = shown(&card(), None, &Choice::Other);
        assert_eq!(o.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(o.fit.measure, FitMeasure::WholeCard);
        assert_eq!(o.other.unwrap().file, "Demo-8B-Q4_K_M.gguf");
    }

    #[test]
    fn the_marks_replace_the_pick_and_lead_with_beside() {
        let m = marks(
            "Demo-8B-Q4_K_M.gguf",
            Some("Demo-8B-Q4_K_M.gguf"),
            Some("Demo-8B-Q8_0.gguf"),
        );
        let s = shown(&card(), Some(&m), &Choice::Pick);
        assert!(s.exact);
        assert_eq!(s.fit.measure, FitMeasure::Beside);
        assert_eq!(s.why, "beside why");
        assert_eq!(s.bytes, Some(5_900_000_000), "the file and its projector");
        let other = s.other.unwrap();
        assert_eq!(other.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(other.fit.measure, FitMeasure::WholeCard);
        // "Use this one instead": the whole card's pick, fitted to it.
        let o = shown(&card(), Some(&m), &Choice::Other);
        assert_eq!(o.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(o.fit.measure, FitMeasure::WholeCard);
        assert_eq!(o.why, "whole why");
        assert_eq!(o.other.unwrap().file, "Demo-8B-Q4_K_M.gguf");
    }

    /// WP7, D100: the file a pasted address named stays while its header
    /// says it fits (here on the whole card only), and gives way when not.
    #[test]
    fn a_named_file_stays_the_cards_while_its_header_fits_it() {
        let m = marks(
            "Demo-8B-Q4_K_M.gguf",
            Some("Demo-8B-Q4_K_M.gguf"),
            Some("Demo-8B-Q8_0.gguf"),
        );
        let mut c = card();
        c.file = "Demo-8B-Q8_0.gguf".into();
        c.named_file = Some(c.file.clone());
        let s = shown(&c, Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(s.fit.measure, FitMeasure::WholeCard);
        assert_eq!(s.other.unwrap().file, "Demo-8B-Q4_K_M.gguf");
        // Its header says it doesn't fit anywhere: the rules' pick.
        let mut m2 = m;
        m2.files[1].fit = Some(plan(
            ff(FitMark::DoesNotFit, FitMeasure::Beside, None),
            ff(FitMark::DoesNotFit, FitMeasure::WholeCard, None),
        ));
        assert_eq!(
            shown(&c, Some(&m2), &Choice::Pick).file,
            "Demo-8B-Q4_K_M.gguf"
        );
        // A card that wasn't asked a file follows the marks as before.
        c.named_file = None;
        assert_eq!(
            shown(&c, Some(&m2), &Choice::Pick).file,
            "Demo-8B-Q4_K_M.gguf"
        );
    }

    /// A card a pasted address named `file` on, with `m`'s marks.
    fn pasted(file: &str) -> ModelCard {
        let mut c = card();
        c.file = file.into();
        c.named_file = Some(file.into());
        c
    }

    /// The marks with a third weights file that fits only the whole card and
    /// is neither pick.
    fn marks_with_q6(beside: Option<&str>, whole: Option<&str>) -> RepoFiles {
        let mut m = marks("Demo-8B-Q4_K_M.gguf", beside, whole);
        m.files.push(file(
            "Demo-8B-Q6_K.gguf",
            6_600_000_000,
            Some(plan(
                ff(FitMark::DoesNotFit, FitMeasure::Beside, None),
                ff(FitMark::Fits, FitMeasure::WholeCard, Some(32_768)),
            )),
        ));
        m
    }

    /// WP7, D147: the other line is labelled by its own file's measure and
    /// "Use this one instead" leads back to the pasted file, whichever pick
    /// that file is.
    #[test]
    fn a_pasted_whole_card_pick_offers_the_beside_one_and_back() {
        let m = marks(
            "Demo-8B-Q4_K_M.gguf",
            Some("Demo-8B-Q4_K_M.gguf"),
            Some("Demo-8B-Q8_0.gguf"),
        );
        let c = pasted("Demo-8B-Q8_0.gguf");
        let s = shown(&c, Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(s.fit.measure, FitMeasure::WholeCard);
        let o = s.other.unwrap();
        assert_eq!(o.file, "Demo-8B-Q4_K_M.gguf");
        assert_eq!(
            o.fit.measure,
            FitMeasure::Beside,
            "said beside what runs now"
        );
        let t = shown(&c, Some(&m), &Choice::Other);
        assert_eq!(t.file, "Demo-8B-Q4_K_M.gguf");
        assert_eq!(t.fit.measure, FitMeasure::Beside);
        let back = t.other.unwrap();
        assert_eq!(back.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(back.fit.measure, FitMeasure::WholeCard);
    }

    #[test]
    fn a_pasted_beside_pick_offers_the_whole_cards_and_back() {
        let m = marks(
            "Demo-8B-Q4_K_M.gguf",
            Some("Demo-8B-Q4_K_M.gguf"),
            Some("Demo-8B-Q8_0.gguf"),
        );
        let c = pasted("Demo-8B-Q4_K_M.gguf");
        let s = shown(&c, Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q4_K_M.gguf");
        assert_eq!(s.fit.measure, FitMeasure::Beside);
        let o = s.other.unwrap();
        assert_eq!(o.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(o.fit.measure, FitMeasure::WholeCard);
        let t = shown(&c, Some(&m), &Choice::Other);
        assert_eq!(t.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(t.other.unwrap().file, "Demo-8B-Q4_K_M.gguf");
    }

    #[test]
    fn a_pasted_file_that_is_neither_pick_offers_the_beside_one_and_back_to_itself() {
        let m = marks_with_q6(Some("Demo-8B-Q4_K_M.gguf"), Some("Demo-8B-Q8_0.gguf"));
        let c = pasted("Demo-8B-Q6_K.gguf");
        let s = shown(&c, Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q6_K.gguf");
        assert_eq!(s.fit.measure, FitMeasure::WholeCard);
        let o = s.other.unwrap();
        assert_eq!(
            o.file, "Demo-8B-Q4_K_M.gguf",
            "the pick of the other measure"
        );
        assert_eq!(o.fit.measure, FitMeasure::Beside);
        let t = shown(&c, Some(&m), &Choice::Other);
        assert_eq!(t.file, "Demo-8B-Q4_K_M.gguf");
        let back = t.other.unwrap();
        assert_eq!(
            back.file, "Demo-8B-Q6_K.gguf",
            "the way back is the pasted file"
        );
        assert_eq!(back.fit.measure, FitMeasure::WholeCard);
    }

    #[test]
    fn with_both_picks_one_file_a_pasted_one_still_has_its_way_back() {
        let m = marks_with_q6(Some("Demo-8B-Q4_K_M.gguf"), Some("Demo-8B-Q4_K_M.gguf"));
        let c = pasted("Demo-8B-Q6_K.gguf");
        let s = shown(&c, Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q6_K.gguf");
        let o = s.other.unwrap();
        assert_eq!(o.file, "Demo-8B-Q4_K_M.gguf");
        assert_eq!(o.fit.measure, FitMeasure::Beside);
        let t = shown(&c, Some(&m), &Choice::Other);
        assert_eq!(t.file, "Demo-8B-Q4_K_M.gguf");
        assert_eq!(t.fit.measure, FitMeasure::Beside, "the beside pick too");
        assert_eq!(t.other.unwrap().file, "Demo-8B-Q6_K.gguf");
        // Not pasted: one file for both picks has no other line.
        let s = shown(&card(), Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q4_K_M.gguf");
        assert!(s.other.is_none());
    }

    /// A pasted file whose header couldn't be read (no fit in the marks) or
    /// that the list lacks stays the card's: only a header that says it fits
    /// nowhere hands the card to the rules' pick (D100 revised).
    #[test]
    fn a_pasted_file_without_a_header_fit_stays() {
        let mut m = marks_with_q6(Some("Demo-8B-Q4_K_M.gguf"), Some("Demo-8B-Q8_0.gguf"));
        m.files[3].fit = None;
        let c = pasted("Demo-8B-Q6_K.gguf");
        assert_eq!(shown(&c, Some(&m), &Choice::Pick).file, "Demo-8B-Q6_K.gguf");
        // By size alone it doesn't fit: still no word from the header.
        m.files[3].fit = Some(plan(
            ff(FitMark::DoesNotFitBySize, FitMeasure::Beside, None),
            ff(FitMark::DoesNotFitBySize, FitMeasure::WholeCard, None),
        ));
        assert_eq!(shown(&c, Some(&m), &Choice::Pick).file, "Demo-8B-Q6_K.gguf");
    }

    #[test]
    fn nothing_beside_shows_the_whole_cards_pick_on_the_whole_card() {
        let m = marks("Demo-8B-Q8_0.gguf", None, Some("Demo-8B-Q8_0.gguf"));
        let s = shown(&card(), Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q8_0.gguf");
        assert_eq!(s.fit.measure, FitMeasure::WholeCard);
        assert!(s.other.is_none());
    }

    #[test]
    fn marks_of_another_upload_are_not_the_cards() {
        let mut m = marks("Demo-8B-Q8_0.gguf", None, Some("Demo-8B-Q8_0.gguf"));
        m.repo = "someone-demo/Other-GGUF".into();
        let s = shown(&card(), Some(&m), &Choice::Pick);
        assert_eq!(s.file, "Demo-8B-Q4_K_M.gguf");
        assert!(!s.exact);
    }

    #[test]
    fn a_file_chosen_by_hand_keeps_its_own_fit_and_class() {
        let m = marks("Demo-8B-Q4_K_M.gguf", Some("Demo-8B-Q4_K_M.gguf"), None);
        let p = PickedFile {
            repo: "demo-org/Demo-8B-GGUF".into(),
            file: "Demo-8B-Q8_0.gguf".into(),
            target: "chat".into(),
            companions: true,
            fit: m.files[1].fit.clone(),
            size_bytes: 8_500_000_000,
            quant: Some("Q8_0".into()),
        };
        let s = shown(&card(), Some(&m), &Choice::ByHand(Box::new(p)));
        assert!(s.by_hand);
        assert_eq!(s.target.as_deref(), Some("chat"));
        assert_eq!(
            s.fit.measure,
            FitMeasure::WholeCard,
            "fits only the whole card"
        );
        assert_eq!(s.bytes, Some(9_400_000_000));
        assert!(s.other.is_none());
    }
}
