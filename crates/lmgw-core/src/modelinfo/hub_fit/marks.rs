//! The marks of a Hugging Face file list and its recommended file
//! (find-a-model design §3.3, WP3, D60 to D69): what `hf_repo` answers with
//! `fit`, reading as few headers as it can while the pick and every mark
//! stay true.
//!
//! A header costs 1 to 16 MiB (§2.2), most of it the tokenizer, and every
//! file of one model shares it. So:
//!
//! 1. **One header first** (D60, D67): the largest file whose listed bytes
//!    fit beside what runs now, else the largest that fits the whole card,
//!    else the smallest, a file whose label names a width the pick takes
//!    first within each; the next in that order when a read fails. That is
//!    the file most likely to be the pick, so its read is rarely wasted.
//!    One such reference per set of files with the same companions (D66):
//!    the files of a directory holding a projector or a drafter by name are
//!    estimated only from a file of that directory, and the files of every
//!    directory holding neither from one of them.
//! 2. **Every other file estimated from it** (WP2's
//!    [`PlanFiles::estimated`], D138): the same shape and companions, its
//!    own bytes charged whole, never smaller than the real footprint, so a
//!    "fits" from it is true. Its width isn't known, so an estimated file is
//!    never the pick (D56).
//! 3. **The band** (D61): a file whose estimate could give another mark
//!    than its own header would is read. Its real footprint is at least its
//!    estimate less the most its host-kept input embeddings could weigh:
//!    their elements (the same in every quant, every part of a split
//!    file's) at 32 bits, F32 being the widest type a converter writes a
//!    weight in, and never more than the file. When the estimate and that
//!    optimistic bound give the same mark by both measures, the header
//!    would give it too; when they differ, the file is read.
//! 4. **The pick's reads** (D62, D67): while an estimated file could still
//!    beat the pick by either measure, the largest such file is read and
//!    the pick taken again. A candidate has at least 4 bits over the
//!    model's quantized layer elements, so its optimistic bound here also
//!    leaves those bytes on the card; a file whose bytes give it under 4
//!    bits over them, or over 8.5 even with every other parameter at 32
//!    bits, isn't read for the pick. The reads stop when nothing could beat
//!    it, so the pick is the one every header would give.
//!
//! The card is read once, and every file of the answer, estimated or read,
//! is measured on that reading (D66). A refusal of the hub's that every
//! read would meet (a gated repo, its rate limit, no connection) stops the
//! reads there: the files keep the marks they have, nothing is recommended,
//! and the list's notes say why (D66). On a metered connection the page's
//! own ask reads nothing, and every file is marked by its size (D68).

use lmgw_api_types::model_find::{
    CardMeasure, FileFit, FilePick, FitMark, FitMeasure, PickRule, PlanFit,
};
use serde_json::{Map, Value};

use crate::hf::{self, HfFile};
use crate::state::SharedState;

use super::super::fit::{self, CardNow};
use super::super::pick;
use super::super::plan_files::{parent_of, PlanFiles};
use super::{file_fit_planned, pick_both, FileFits};

mod bounds;

/// One weights file's marks.
#[derive(Debug, Clone)]
pub struct Marked {
    /// Its path in the repo (the first part of a split file).
    pub file: String,
    /// Its fit, as the list carries it ([`compacted`]): the card only when
    /// it isn't the list's, the notes only those that aren't the list's.
    pub fit: PlanFit,
    /// The file whose header this file's fit was estimated from; `None`
    /// when its own header was read, or it is marked by its size alone.
    pub estimated_from: Option<String>,
}

/// A repo's file list measured on the card (module doc).
#[derive(Debug, Clone, Default)]
pub struct RepoMarks {
    /// Every weights file asked for, in the order asked.
    pub files: Vec<Marked>,
    /// The card the marks were measured against; `None` without one (D9).
    pub card: Option<CardMeasure>,
    pub whole_card: FilePick,
    pub beside: FilePick,
    /// The beside pick, else the whole card's (D120).
    pub recommended: Option<String>,
    /// The header bytes these marks received, a read that failed after it
    /// began included; a header the process had read before adds nothing
    /// (D123).
    pub header_bytes_read: u64,
    /// What holds for the whole list, said once: the hold, admission off,
    /// what other programs use not being known, the estimates' source, why
    /// the reads stopped, no card, a class whose files aren't measured.
    pub notes: Vec<String>,
    /// Why no header was read on a metered connection (D68), when that is
    /// why the marks are by size.
    pub metered: Option<String>,
}

/// What the marks may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reads {
    /// The headers §3.3 reads.
    Headers,
    /// No header: the connection is metered, as the sentence says (D68).
    Metered(String),
}

/// One file while the marks are taken.
struct Entry {
    fits: FileFits,
    /// Estimated: the reference it came from, its optimistic fit for the
    /// marks (D61) and for the pick (D67).
    estimate: Option<Estimate>,
}

struct Estimate {
    from: String,
    /// The reference's index in `refs`.
    by: usize,
    marks: PlanFit,
    pick: PlanFit,
}

/// A reference and the plan parameters its other files are sized with.
struct Reference {
    files: PlanFiles,
    params: Map<String, Value>,
    file: String,
}

/// The marks of `weights` (paths in `repo`, each a weights file of the
/// listing `listing`), for the class `target` as `local_model_plan` takes
/// it (module doc).
pub async fn repo_marks(
    state: &SharedState,
    repo: &str,
    listing: &[HfFile],
    weights: &[String],
    target: Option<&str>,
    reads: Reads,
) -> RepoMarks {
    let mut out = RepoMarks::default();
    if let Err(why) = super::super::hub_class(target) {
        out.notes
            .push(format!("no file was measured on the graphics card: {why}"));
        return out;
    }
    let snap = state.snapshot();
    let card = CardNow::of(state, &snap).await;
    let listed: Vec<_> = weights
        .iter()
        .map(|f| super::super::plan_files::hub::listed_bytes(listing, f))
        .collect();
    let Some(card) = card else {
        out.notes.push(
            "lmgw can't measure the graphics card yet: it has no reading of the card's memory and \
             no GPU memory budget is set, so no file's header was read and the files have no \
             marks"
                .into(),
        );
        out.files = weights
            .iter()
            .zip(&listed)
            .map(|(f, l)| {
                let why = "no header was read: there is no card to measure against".to_string();
                let by = |m| fit::by_size(l.bytes, None, m, why.clone());
                Marked {
                    file: f.clone(),
                    fit: PlanFit {
                        card: None,
                        whole_card: by(FitMeasure::WholeCard),
                        beside: by(FitMeasure::Beside),
                    },
                    estimated_from: None,
                }
            })
            .collect();
        out.whole_card.why = no_pick_line();
        out.beside.why = no_pick_line();
        return out;
    };
    let base = card.dto();
    out.card = Some(base.clone());
    let mut common: Vec<String> = Vec::new();
    for m in [FitMeasure::WholeCard, FitMeasure::Beside] {
        for n in card.notes(m) {
            if !common.contains(&n) {
                common.push(n);
            }
        }
    }

    // A file the listing gives no bytes for is unknown, never read (D8).
    let mut entries: Vec<Option<Entry>> = weights
        .iter()
        .zip(&listed)
        .map(|(f, l)| {
            l.bytes.is_none().then(|| {
                let why = "its bytes aren't in the repo's file list, so its header wasn't read"
                    .to_string();
                let mut fits = by_size(&card, f, None, why);
                for m in [&mut fits.fit.whole_card, &mut fits.fit.beside] {
                    m.notes.extend(l.missing.clone());
                }
                Entry {
                    fits,
                    estimate: None,
                }
            })
        })
        .collect();

    if let Reads::Metered(how) = reads {
        let said = format!(
            "The connection is metered ({how}), so lmgw read no file's header: every file is \
             marked by its size alone, and none is recommended until the headers are read."
        );
        for (i, f) in weights.iter().enumerate() {
            if entries[i].is_none() {
                entries[i] = Some(Entry {
                    fits: by_size(
                        &card,
                        f,
                        listed[i].bytes,
                        "its header wasn't read: the connection is metered".into(),
                    ),
                    estimate: None,
                });
            }
        }
        out.whole_card.why = said.clone();
        out.beside.why = said.clone();
        out.metered = Some(said);
        out.notes = common.clone();
        out.files = finish(entries.into_iter().flatten().collect(), &base, &common);
        return out;
    }

    // 1. A reference per set of files with the same companions (D66).
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, file) in weights.iter().enumerate() {
        let key = companion_key(listing, file);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    let mut stop: Option<String> = None;
    let mut refs: Vec<Reference> = Vec::new();
    let mut ref_of: Vec<Option<usize>> = vec![None; weights.len()];
    for (_, members) in &groups {
        if stop.is_some() {
            break;
        }
        for i in bounds::reference_order(&card, weights, &listed, members) {
            let (fits, params) =
                file_fit_planned(state, repo, &weights[i], listing, target, Some(&card)).await;
            out.header_bytes_read += fits.read_bytes;
            if let Some(why) = &fits.hub_failure {
                stop = Some(why.clone());
            }
            let found = fits.files.clone().zip(params);
            entries[i] = Some(Entry {
                fits,
                estimate: None,
            });
            if let Some((files, params)) = found {
                refs.push(Reference {
                    files,
                    params,
                    file: weights[i].clone(),
                });
                for m in members {
                    ref_of[*m] = Some(refs.len() - 1);
                }
                break;
            }
            if stop.is_some() {
                break;
            }
        }
    }

    // 2. The estimates.
    let mut estimated_any = false;
    for (i, f) in weights.iter().enumerate() {
        let (Some(bytes), None, Some(by)) = (listed[i].bytes, &entries[i], ref_of[i]) else {
            continue;
        };
        let Ok(rel) = hf::dest_rel_path(repo, f) else {
            continue;
        };
        let r = &refs[by];
        let est = r.files.estimated(&rel, bytes);
        let optimistic = |host: u64| {
            let mut opt = est.clone();
            opt.summary.host_input_bytes = host;
            fit_of(&snap, &card, &opt, &r.params)
        };
        estimated_any = true;
        entries[i] = Some(Entry {
            fits: FileFits {
                file: f.clone(),
                bytes: Some(bytes),
                files: None,
                fit: fit_of(&snap, &card, &est, &r.params),
                read_bytes: 0,
                hub_failure: None,
            },
            estimate: Some(Estimate {
                from: r.file.clone(),
                by,
                marks: optimistic(bounds::host_bound(&r.files, bytes)),
                pick: optimistic(bounds::host_bound_for_pick(&r.files, bytes)),
            }),
        });
    }
    let mut entries: Vec<Entry> = entries
        .into_iter()
        .zip(weights)
        .zip(&listed)
        .map(|((e, f), l)| {
            e.unwrap_or_else(|| {
                // A file no read reached: every read of its set failed, or
                // the reads stopped first, so it is marked by its size.
                let why = match &stop {
                    Some(_) => {
                        "its header wasn't read: Hugging Face stopped the reads (the list's \
                                notes say why)"
                    }
                    None => "no header of the files beside it could be read",
                };
                Entry {
                    fits: by_size(
                        &card,
                        f,
                        l.bytes,
                        format!("marked by its size alone: {why}"),
                    ),
                    estimate: None,
                }
            })
        })
        .collect();

    // 3. The band.
    for e in entries.iter_mut() {
        if stop.is_some() {
            break;
        }
        let differs = e.estimate.as_ref().is_some_and(|est| {
            let cons = &e.fits.fit;
            cons.whole_card.mark != est.marks.whole_card.mark
                || cons.beside.mark != est.marks.beside.mark
        });
        if differs {
            read_exactly(state, repo, listing, target, &card, e, &mut out, &mut stop).await;
        }
    }

    // 4. The pick's reads.
    let trained = refs.first().and_then(|r| r.files.summary.context_length);
    let target_tokens = pick::shorter_target(trained);
    loop {
        let all: Vec<FileFits> = entries.iter().map(|e| e.fits.clone()).collect();
        let (whole, beside) = pick_both(&all);
        let bytes_of = |p: &FilePick| {
            p.file
                .as_ref()
                .and_then(|f| all.iter().find(|x| &x.file == f).and_then(|x| x.bytes))
        };
        let next = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let est = e.estimate.as_ref()?;
                let bytes = e.fits.bytes?;
                let r = &refs[est.by].files;
                let width = bounds::may_reach_floor(bytes, r.layer_elements)
                    && bounds::may_be_under_cap(bytes, r);
                let beats = |fit: &FileFit, p: &FilePick| {
                    could_beat(fit, bytes, &e.fits.file, p, bytes_of(p), target_tokens)
                };
                (width && (beats(&est.pick.whole_card, &whole) || beats(&est.pick.beside, &beside)))
                    .then_some((i, bytes))
            })
            .max_by(|(a, ab), (b, bb)| {
                ab.cmp(bb)
                    .then_with(|| entries[*b].fits.file.cmp(&entries[*a].fits.file))
            })
            .map(|(i, _)| i);
        let (Some(i), None) = (next, &stop) else {
            out.whole_card = whole;
            out.beside = beside;
            break;
        };
        read_exactly(
            state,
            repo,
            listing,
            target,
            &card,
            &mut entries[i],
            &mut out,
            &mut stop,
        )
        .await;
    }

    if let Some(why) = &stop {
        let line = format!(
            "No file is recommended: Hugging Face stopped the header reads ({why}), so a file \
             not read could still be the one. Every file is still in the list to choose by \
             hand, and listing the repo again reads the rest."
        );
        common.push(format!(
            "Hugging Face stopped the header reads: {why}. The files not read keep a mark from \
             another file's header or from their size."
        ));
        out.whole_card = no_pick(line.clone(), FitMeasure::WholeCard);
        out.beside = no_pick(line, FitMeasure::Beside);
    } else if refs.is_empty() {
        let why = "No file is recommended: no header of this repo could be read, so no file's \
                   width or conversation length is known. Every file is still in the list to \
                   choose by hand."
            .to_string();
        out.whole_card.why = why.clone();
        out.beside.why = why;
    }
    if estimated_any {
        let from: Vec<&str> = refs.iter().map(|r| r.file.as_str()).collect();
        common.push(format!(
            "The files not read were estimated from the header of {}: the same model, each \
             file's own bytes charged whole, so an estimate is never smaller than the real use; \
             a file's own header is read when it is chosen.",
            from.join(" or ")
        ));
    }
    out.recommended = out
        .beside
        .file
        .clone()
        .or_else(|| out.whole_card.file.clone());
    out.notes = common.clone();
    out.files = finish(entries, &base, &common);
    out
}

/// The entries as the list carries them.
fn finish(entries: Vec<Entry>, base: &CardMeasure, common: &[String]) -> Vec<Marked> {
    entries
        .into_iter()
        .map(|e| Marked {
            file: e.fits.file,
            fit: compacted(e.fits.fit, base, common),
            estimated_from: e.estimate.map(|est| est.from),
        })
        .collect()
}

/// `fit` as a file list carries it (D63): without its card when that is
/// the list's `card`, and without the notes the list says once (`common`):
/// what every file of the list would repeat.
pub fn compacted(mut fit: PlanFit, card: &CardMeasure, common: &[String]) -> PlanFit {
    if fit.card.as_ref() == Some(card) {
        fit.card = None;
    }
    for m in [&mut fit.whole_card, &mut fit.beside] {
        m.notes.retain(|n| !common.contains(n));
    }
    fit
}

/// What sets a file's companions apart (D66): its directory when that
/// holds a projector or a drafter by name beside it, else nothing, shared
/// by every directory that holds neither.
fn companion_key(listing: &[HfFile], file: &str) -> String {
    let parent = parent_of(file);
    let companions = listing.iter().any(|f| {
        f.path != file
            && parent_of(&f.path) == parent
            && !hf::same_split_set(&f.path, file)
            && matches!(
                crate::ops::classify_repo_file(&f.path, "chat"),
                "mmproj" | "drafter"
            )
    });
    match companions {
        true => format!("{parent}/"),
        false => String::new(),
    }
}

/// The pick's line when nothing could be measured.
fn no_pick_line() -> String {
    "No file is recommended: lmgw can't measure the graphics card yet. Every file is still in \
     the list to choose by hand."
        .into()
}

fn no_pick(why: String, measure: FitMeasure) -> FilePick {
    FilePick {
        file: None,
        rule: PickRule::None,
        measure,
        why,
        full_context: None,
    }
}

/// The fit of an estimated file by both measures, on `card` less the rows
/// that serve it, sized with the reference's plan `params`.
fn fit_of(
    snap: &crate::config::Snapshot,
    card: &CardNow,
    files: &PlanFiles,
    params: &Map<String, Value>,
) -> PlanFit {
    let card = card.clone().beside_itself(snap, files.found_in, &files.rel);
    let sizer = super::super::plan_sizer(files.found_in, files, params);
    PlanFit {
        card: Some(card.dto()),
        whole_card: fit::fit(files, &sizer, Some(&card), FitMeasure::WholeCard),
        beside: fit::fit(files, &sizer, Some(&card), FitMeasure::Beside),
    }
}

/// A file marked by its `bytes` alone on `card` (D8), `why` saying why.
fn by_size(card: &CardNow, file: &str, bytes: Option<u64>, why: String) -> FileFits {
    let by = |m| fit::by_size(bytes, Some(card), m, why.clone());
    FileFits {
        file: file.to_string(),
        bytes,
        files: None,
        fit: PlanFit {
            card: Some(card.dto()),
            whole_card: by(FitMeasure::WholeCard),
            beside: by(FitMeasure::Beside),
        },
        read_bytes: 0,
        hub_failure: None,
    }
}

/// Reads `e`'s own header on `card` and takes its exact fit in place of the
/// estimate. A refusal every read would meet keeps the estimate, which
/// holds, and stops the reads (D66).
#[allow(clippy::too_many_arguments)]
async fn read_exactly(
    state: &SharedState,
    repo: &str,
    listing: &[HfFile],
    target: Option<&str>,
    card: &CardNow,
    e: &mut Entry,
    out: &mut RepoMarks,
    stop: &mut Option<String>,
) {
    let (fits, _) = file_fit_planned(state, repo, &e.fits.file, listing, target, Some(card)).await;
    out.header_bytes_read += fits.read_bytes;
    if let Some(why) = fits.hub_failure {
        *stop = Some(why);
        return;
    }
    e.fits = fits;
    e.estimate = None;
}

/// Whether a file whose fit is at best `opt` could be chosen over `pick`
/// (module doc, 4): `bytes` and `file` are the file's, `pick_bytes` the
/// pick's, `target` rule 2's conversation.
fn could_beat(
    opt: &FileFit,
    bytes: u64,
    file: &str,
    pick: &FilePick,
    pick_bytes: Option<u64>,
    target: u64,
) -> bool {
    let whole = opt.mark == FitMark::Fits;
    let enough = whole || (opt.mark == FitMark::FitsShorter && opt.ctx >= Some(target));
    let larger = match (pick.file.as_deref(), pick_bytes) {
        (Some(p), Some(pb)) => bytes > pb || (bytes == pb && file < p),
        _ => true,
    };
    match pick.rule {
        PickRule::WholeContext => whole && larger,
        PickRule::ShorterContext => whole || (enough && larger),
        PickRule::None => enough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(mark: FitMark, ctx: Option<u64>) -> FileFit {
        FileFit {
            mark,
            ctx,
            ..FileFit::default()
        }
    }

    fn pick(file: Option<&str>, rule: PickRule) -> FilePick {
        FilePick {
            file: file.map(str::to_string),
            rule,
            measure: FitMeasure::Beside,
            ..FilePick::default()
        }
    }

    #[test]
    fn a_file_is_read_for_the_pick_only_when_it_could_beat_it() {
        let whole = fit(FitMark::Fits, Some(8192));
        let short = fit(FitMark::FitsShorter, Some(40_000));
        let too_short = fit(FitMark::FitsShorter, Some(1_000));
        let none = fit(FitMark::DoesNotFit, None);
        let r1 = pick(Some("b.gguf"), PickRule::WholeContext);
        // Rule 1's pick: only a larger file that fits whole.
        assert!(could_beat(&whole, 20, "c.gguf", &r1, Some(10), 32_768));
        assert!(!could_beat(&whole, 5, "c.gguf", &r1, Some(10), 32_768));
        assert!(!could_beat(&short, 20, "c.gguf", &r1, Some(10), 32_768));
        // Equal bytes: the earlier name wins.
        assert!(could_beat(&whole, 10, "a.gguf", &r1, Some(10), 32_768));
        assert!(!could_beat(&whole, 10, "c.gguf", &r1, Some(10), 32_768));
        // Rule 2's pick: any file that fits whole, or a larger one that fits
        // the target.
        let r2 = pick(Some("b.gguf"), PickRule::ShorterContext);
        assert!(could_beat(&whole, 5, "c.gguf", &r2, Some(10), 32_768));
        assert!(could_beat(&short, 20, "c.gguf", &r2, Some(10), 32_768));
        assert!(!could_beat(&short, 5, "c.gguf", &r2, Some(10), 32_768));
        assert!(!could_beat(&too_short, 20, "c.gguf", &r2, Some(10), 32_768));
        // No pick: anything that fits the target.
        let r0 = pick(None, PickRule::None);
        assert!(could_beat(&short, 1, "c.gguf", &r0, None, 32_768));
        assert!(!could_beat(&too_short, 1, "c.gguf", &r0, None, 32_768));
        assert!(!could_beat(&none, 1, "c.gguf", &r0, None, 32_768));
    }

    fn f(path: &str) -> HfFile {
        HfFile {
            path: path.into(),
            size: Some(1),
            kind: "file".into(),
            lfs: None,
        }
    }

    #[test]
    fn files_share_a_reference_only_with_the_same_companions() {
        let listing = [
            f("m-Q4_K_M.gguf"),
            f("mmproj-F16.gguf"),
            f("sub/m-Q8_0.gguf"),
            f("BF16/m-BF16-00001-of-00002.gguf"),
            f("BF16/m-BF16-00002-of-00002.gguf"),
            f("drafts/m-Q6_K.gguf"),
            f("drafts/m-DRAFT-0.6B-Q8_0.gguf"),
        ];
        let key = |p: &str| companion_key(&listing, p);
        assert_eq!(key("m-Q4_K_M.gguf"), "/");
        // Neither holds a companion: one set, whatever the directory.
        assert_eq!(key("sub/m-Q8_0.gguf"), "");
        assert_eq!(key("BF16/m-BF16-00001-of-00002.gguf"), "");
        // A split file's own parts aren't its companions.
        assert_eq!(
            companion_key(&listing[3..5], "BF16/m-BF16-00001-of-00002.gguf"),
            ""
        );
        assert_eq!(key("drafts/m-Q6_K.gguf"), "drafts/");
    }

    #[test]
    fn a_list_says_once_what_every_file_would_repeat() {
        let card = CardMeasure {
            capacity_bytes: 24,
            hold: true,
            ..CardMeasure::default()
        };
        let mut file = fit(FitMark::Fits, Some(1));
        file.notes = vec!["hold".into(), "own".into()];
        let pf = PlanFit {
            card: Some(card.clone()),
            whole_card: file.clone(),
            beside: file,
        };
        let c = compacted(pf.clone(), &card, &["hold".to_string()]);
        assert_eq!(c.card, None);
        assert_eq!(c.whole_card.notes, vec!["own".to_string()]);
        // A file whose card isn't the list's (a row on it left out) keeps it.
        let other = CardMeasure {
            capacity_bytes: 25,
            ..card.clone()
        };
        let kept = compacted(pf, &other, &[]);
        assert_eq!(kept.card, Some(card));
        assert_eq!(kept.beside.notes.len(), 2);
    }
}
