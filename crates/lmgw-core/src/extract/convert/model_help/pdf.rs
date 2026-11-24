//! The model stage of a PDF (design §3 tier 1, §7.2, §7.7): after the
//! tools have read the file into the block model, the pages
//! [`select`](super::select) picks are rendered and read by the
//! page-reading model, each as its turn in the request's model-call pool
//! comes ([`page`](super::page)). A page the model read has the model's
//! blocks and the source `vision`; a page it could not read keeps what the
//! tools made of it, and a warning names the page and why (`model_held`,
//! `model_cannot_see`, `model_not_allowed`, `part_failed`).
//!
//! The stage holds no slot of the tool pool while a page waits for the
//! model: the file gave its own back before this runs, and a page takes
//! one only for its render (`pdftoppm` through `run.rs`). It holds the
//! route the request opened only while it runs
//! ([`Keep`](super::session::Keep)).
//!
//! **The file's deadline.** Each page is a lane of the file's clock
//! (`pool::clock`): waiting for the route, a turn or a render's slot does
//! not count, a render and a call in flight do. A stage with pages takes
//! the file's end over (`FileClock::hand_over`): at the deadline the calls
//! in flight are dropped, the pages answered stay, the rest keep what the
//! tools read with a `part_failed` note naming `document_timeout`, and the
//! file is finished from that (`partial`) instead of failing.
//!
//! **A page's pictures stay.** The figures the tools found on a page the
//! model read (the layout model's picture regions) are kept: one alike a
//! picture of the answer takes that picture's place, the others follow the
//! answer's text ([`keep_figures`]). poppler's raster pictures are placed
//! afterwards into whatever the page holds (`pictures::pdf::place`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use lmgw_api_types::documents::convert::{ConvertWarning, ErrorCode, WarningCode};
use lmgw_api_types::documents::model_help::ModelCalls;

use super::page::{self, By, Outcome, Page};
use super::select::{self, Ask, PageFacts, Recognised};
use super::session::Keep;
use super::Help;
use crate::erased::Erased;
use crate::extract::convert::caps::ToolId;
use crate::extract::convert::doc::render;
use crate::extract::convert::doc::{Block, BlockKind, Doc, Part, Rect, Source};
use crate::extract::convert::ocr::spool::Shared;
use crate::extract::convert::pages::page_ranges;
use crate::extract::convert::pool::clock;
use crate::extract::convert::tools::{Fact, ToolFacts};
use crate::extract::convert::watch::Watched;
use crate::extract::pdf;

/// What the stage did to a file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stage {
    pub warnings: Vec<ConvertWarning>,
    /// The models that answered, for the file's `stats.models`.
    pub models: Vec<ModelCalls>,
    /// Calls sent, the failed ones included.
    pub calls: u32,
    /// A page that should have gone to the model did not get its answer.
    pub partial: bool,
    /// What a file fails with when none of its pages was read and the
    /// model is why: the gravest of its pages' reasons ([`Stage::fails`]).
    /// `None` when every page was read, or only a render failed.
    pub failure: Option<ErrorCode>,
    /// The pages the model read (what text recognition made of them is no
    /// longer in the output).
    pub read: BTreeSet<u32>,
    /// Exported pictures not described for being smaller than
    /// `describe_min_area` (`describe`), for the file's stats.
    pub pictures_small: u32,
}

impl Stage {
    /// Add another file's calls to this one's (an e-mail's attachment
    /// read with the model), or the picture-description stage's: one entry
    /// per model that answered.
    pub fn absorb(&mut self, calls: u32, models: Vec<ModelCalls>) {
        self.calls = self.calls.saturating_add(calls);
        for m in models {
            match self
                .models
                .iter_mut()
                .find(|e| (&e.alias, &e.answered_for) == (&m.alias, &m.answered_for))
            {
                Some(e) => {
                    e.calls = e.calls.saturating_add(m.calls);
                    e.parts_read = e.parts_read.saturating_add(m.parts_read);
                    e.pictures_described =
                        e.pictures_described.saturating_add(m.pictures_described);
                    e.files_transcribed = e.files_transcribed.saturating_add(m.files_transcribed);
                }
                None => self.models.push(m),
            }
        }
    }

    /// Why the model did not read the pages, for the error of a file none
    /// of whose pages was read (a failed file carries no warnings): each of
    /// the stage's warnings, which name the pages.
    pub fn said(&self) -> String {
        let said: Vec<&str> = self.warnings.iter().map(|w| w.message.as_str()).collect();
        said.join("; ")
    }

    /// One more reason a page was not read, for [`Stage::failure`]. The
    /// order is what a caller can act on first: the deadline it set, then
    /// its key's policy, then a model that is paused, then one that failed.
    fn fails(&mut self, code: ErrorCode) {
        let rank = |c: ErrorCode| match c {
            ErrorCode::DeadlineExceeded => 3,
            ErrorCode::ModelNotAllowed => 2,
            ErrorCode::ModelHeld => 1,
            _ => 0,
        };
        if self.failure.is_none_or(|have| rank(code) > rank(have)) {
            self.failure = Some(code);
        }
    }
}

/// The `pdftoppm` beside the `pdftotext` the snapshot found.
fn pdftoppm(facts: &ToolFacts) -> Option<PathBuf> {
    let Some(Fact::Found(f)) = facts.fact(ToolId::Poppler) else {
        return None;
    };
    f.watched.iter().find_map(|w| match w {
        Watched::Binary { name, at: Some(at) } if name == pdf::PDFTOPPM => Some(at.path.clone()),
        _ => None,
    })
}

/// One page's text as the tools read it, for the model to copy from.
fn page_text(part: &Part) -> String {
    render::text(
        &Doc {
            parts: vec![part.clone()],
        },
        "\n\n",
    )
}

fn warning(code: WarningCode, message: String, mut parts: Vec<u32>) -> ConvertWarning {
    parts.sort_unstable();
    ConvertWarning {
        code,
        message,
        tool: None,
        parts,
        option: None,
    }
}

fn pages_said(pages: &[u32]) -> String {
    let mut sorted = pages.to_vec();
    sorted.sort_unstable();
    if sorted.len() == 1 {
        format!("page {}", page_ranges(&sorted))
    } else {
        format!("pages {}", page_ranges(&sorted))
    }
}

/// What a file's warning about pages nothing read adds about model help:
/// without it, that a page-reading model would read them; with it, nothing
/// (the stage's own warnings say why the model did not).
pub fn unread_hint(model_help: bool) -> &'static str {
    if model_help {
        ""
    } else {
        "; a page-reading model would read them too (choose one under Settings → Documents, \
         \"Read pages with a model\", and leave the request's model_help on)"
    }
}

/// What the pages named keep.
const KEPT: &str = "a page with text keeps the text the tools read, a page without stays unread";

/// The pages that were not read, by why.
#[derive(Default)]
struct Unread {
    held: (Vec<u32>, String),
    blind: BTreeMap<String, Vec<u32>>,
    refused: (Vec<u32>, String),
    failed: BTreeMap<String, Vec<u32>>,
}

impl Unread {
    /// The warnings, and what the file fails with if nothing was read.
    fn into_stage(self, stage: &mut Stage) {
        let (held, held_why) = self.held;
        if !held.is_empty() {
            stage.fails(ErrorCode::ModelHeld);
            stage.warnings.push(warning(
                WarningCode::ModelHeld,
                format!(
                    "{} would have been read by the page-reading model, which is paused \
                     ({held_why}); {KEPT}",
                    pages_said(&held)
                ),
                held,
            ));
        }
        for (fallback, pages) in self.blind {
            stage.fails(ErrorCode::ModelHeld);
            stage.warnings.push(warning(
                WarningCode::ModelCannotSee,
                format!(
                    "{} would have been read by the page-reading model, which cannot run now; \
                     its fallback '{fallback}' answers in its place and cannot read pages (it \
                     takes no pictures), so nothing was sent to it; {KEPT}",
                    pages_said(&pages)
                ),
                pages,
            ));
        }
        let (refused, refused_why) = self.refused;
        if !refused.is_empty() {
            stage.fails(ErrorCode::ModelNotAllowed);
            stage.warnings.push(warning(
                WarningCode::ModelNotAllowed,
                format!(
                    "{} would have been read by the page-reading model, which this key may not \
                     use now ({refused_why}); {KEPT}",
                    pages_said(&refused)
                ),
                refused,
            ));
        }
        for (why, pages) in self.failed {
            stage.warnings.push(warning(
                WarningCode::PartFailed,
                format!(
                    "the page-reading model did not read {}: {why}; {KEPT}",
                    pages_said(&pages)
                ),
                pages,
            ));
        }
    }
}

/// Read the pages of `doc` (a PDF's, as the tools read the file `spool`
/// holds) that `help` picks with the page-reading model, in place; `ocr`
/// is what text recognition said of the pages it read.
///
/// Erased ([`crate::erased`]): a file's whole model stage behind one
/// future, so the conversion above it (a request's handler, a job's task)
/// does not prove `Send` through every page's gate, send and row.
pub fn read_pages<'a>(
    help: &'a Help,
    spool: &'a Shared,
    doc: &'a mut Doc,
    facts: &'a ToolFacts,
    ocr: Recognised<'a>,
) -> Erased<'a, Stage> {
    Box::pin(stage(help, spool, doc, facts, ocr))
}

async fn stage(
    help: &Help,
    spool: &Shared,
    doc: &mut Doc,
    facts: &ToolFacts,
    ocr: Recognised<'_>,
) -> Stage {
    let chosen = &*help.chosen;
    let mut stage = Stage::default();
    let picked: Vec<(usize, u32, Ask)> = doc
        .parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            select::pick(PageFacts::of(p, ocr), help.pages).map(|ask| (i, p.number, ask))
        })
        .collect();
    if picked.is_empty() {
        return stage;
    }
    let numbers: Vec<u32> = picked.iter().map(|p| p.1).collect();
    // The key's scope, before anything is opened for it (D6): a key that
    // may not use the model never loads it.
    let snap = chosen.state.snapshot();
    let key = chosen.ctx.current_key_name(&snap);
    if let Err(e) = crate::policy::check_scope(&snap, key.as_deref(), &chosen.alias) {
        stage.partial = true;
        stage.fails(ErrorCode::ModelNotAllowed);
        stage.warnings.push(warning(
            WarningCode::ModelNotAllowed,
            format!(
                "{} would have been read by the page-reading model, which this key may not \
                 use ({e}); {KEPT}",
                pages_said(&numbers)
            ),
            numbers,
        ));
        return stage;
    }
    drop(snap);
    let bin = pdftoppm(facts);
    // The file's clock: from here on the stage ends the file's work at its
    // deadline and keeps what was read (module doc).
    let clock = clock::current();
    if let Some(c) = &clock {
        c.hand_over();
    }
    // The stage's hold on the route its pages open: given back when the
    // stage ends, so the request holds no claim between its files.
    let keep = Keep::default();
    // Every picked page asks for its turn at once, in page order; each
    // future is boxed, holds no image until its turn comes, and is a lane
    // of the file's clock, which this lane waits for.
    let all = clock::paused();
    let done = futures::future::join_all(picked.iter().map(|(i, number, ask)| {
        let part = &doc.parts[*i];
        let text = match ask {
            Ask::ImageAndText => page_text(part),
            Ask::Image => String::new(),
        };
        let one = Page {
            chosen,
            keep: &keep,
            spool,
            bin: bin.as_ref(),
            number: *number,
            ask: *ask,
            text,
            size: part.size,
        };
        Box::pin(clock::beside(page::read(one, clock.clone())))
    }))
    .await;
    drop(all);
    drop(keep);

    let mut by: BTreeMap<By, (u32, u32)> = BTreeMap::new();
    let mut unread = Unread::default();
    let mut noted: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut scaled: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for ((i, page, _), d) in picked.into_iter().zip(done) {
        let read = matches!(d.outcome, Outcome::Read { .. });
        stage.partial |= !read;
        if let Some(b) = d.sent {
            let e = by.entry(b).or_default();
            e.0 += 1;
            e.1 += u32::from(read);
            stage.calls += 1;
        }
        if let Some(said) = d.scaled {
            scaled.entry(said).or_default().push(page);
        }
        match d.outcome {
            Outcome::Read { blocks, note } => {
                let part = &mut doc.parts[i];
                let tools = std::mem::replace(&mut part.blocks, blocks);
                keep_figures(&mut part.blocks, tools);
                part.source = Some(Source::Vision);
                stage.read.insert(page);
                if let Some(n) = note {
                    noted.entry(n).or_default().push(page);
                }
            }
            Outcome::Held(why) => {
                unread.held.0.push(page);
                unread.held.1 = why;
            }
            Outcome::Blind(fallback) => unread.blind.entry(fallback).or_default().push(page),
            Outcome::NotAllowed(why) => {
                unread.refused.0.push(page);
                unread.refused.1 = why;
            }
            Outcome::Failed(why) => {
                stage.fails(ErrorCode::ModelFailed);
                unread.failed.entry(why).or_default().push(page);
            }
            Outcome::Unrendered(why) => unread.failed.entry(why).or_default().push(page),
            Outcome::Late(why) => {
                stage.fails(ErrorCode::DeadlineExceeded);
                unread.failed.entry(why).or_default().push(page);
            }
        }
    }
    unread.into_stage(&mut stage);
    for (note, pages) in noted {
        stage.warnings.push(warning(
            WarningCode::ModelAnswerMalformed,
            format!("{}: {note}", pages_said(&pages)),
            pages,
        ));
    }
    for (said, pages) in scaled {
        stage.warnings.push(warning(
            WarningCode::PageScaled,
            format!("{} {said}", pages_said(&pages)),
            pages,
        ));
    }
    stage.models = by
        .into_iter()
        .map(|(b, (calls, parts_read))| ModelCalls {
            bound_from: b.bound_from(),
            alias: b.alias,
            answered_for: b.answered_for,
            calls,
            parts_read,
            pictures_described: 0,
            files_transcribed: 0,
            in_flight_bound: b.bound,
        })
        .collect();
    stage
}

/// The tools' figures of a page the model read (module doc): each alike a
/// figure of the answer (their boxes overlap by half of the larger one)
/// takes that figure's place, the rest follow the answer's blocks, before
/// a page footer. The answer's own figures without a box stay (a Markdown
/// answer's picture line).
pub(super) fn keep_figures(answer: &mut Vec<Block>, tools: Vec<Block>) {
    let mut end = answer
        .iter()
        .position(|b| b.kind == BlockKind::PageFooter)
        .unwrap_or(answer.len());
    for fig in tools.into_iter().filter(|b| b.kind == BlockKind::Figure) {
        let alike = fig.bbox.and_then(|r| {
            answer
                .iter()
                .position(|b| b.kind == BlockKind::Figure && b.bbox.is_some_and(|a| alike(&a, &r)))
        });
        match alike {
            Some(j) => answer[j] = fig,
            None => {
                answer.insert(end, fig);
                end += 1;
            }
        }
    }
}

/// Whether two boxes overlap by half of the larger one.
fn alike(a: &Rect, b: &Rect) -> bool {
    let area = |r: &Rect| f64::from((r.x1 - r.x0).max(0.0)) * f64::from((r.y1 - r.y0).max(0.0));
    let i = Rect {
        x0: a.x0.max(b.x0),
        y0: a.y0.max(b.y0),
        x1: a.x1.min(b.x1),
        y1: a.y1.min(b.y1),
    };
    let large = area(a).max(area(b));
    i.x1 > i.x0 && i.y1 > i.y0 && large > 0.0 && area(&i) >= 0.5 * large
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An e-mail's attachments read with the model: their calls add up in
    /// the message's stats, one entry per answering model.
    #[test]
    fn attachments_calls_add_up_per_answering_model() {
        let calls = |alias: &str, n| ModelCalls {
            alias: alias.into(),
            answered_for: None,
            calls: n,
            parts_read: n,
            pictures_described: 0,
            files_transcribed: 0,
            in_flight_bound: 1,
            bound_from: lmgw_api_types::documents::model_help::BoundFrom::ToolPool,
        };
        let mut s = Stage::default();
        s.absorb(2, vec![calls("example-vision", 2)]);
        s.absorb(
            3,
            vec![calls("example-vision", 1), calls("example-backup", 2)],
        );
        s.absorb(0, Vec::new());
        assert_eq!(s.calls, 5);
        let got: Vec<(&str, u32)> = s
            .models
            .iter()
            .map(|m| (m.alias.as_str(), m.calls))
            .collect();
        assert_eq!(got, [("example-vision", 3), ("example-backup", 2)]);
    }

    /// A page the model read keeps the tools' figures: one alike a figure
    /// of a DocTags answer takes its place (with the tools' box, source and
    /// inside text), the others follow the answer's text, before its footer.
    #[test]
    fn a_page_the_model_read_keeps_the_tools_figures() {
        let r = |x0, y0, x1, y1| Some(Rect { x0, y0, x1, y1 });
        let b = |kind, text: &str, bbox, source| Block::new(kind, text.into(), bbox, source);
        let tools = vec![
            b(
                BlockKind::Paragraph,
                "tool text",
                r(0.0, 0.0, 100.0, 10.0),
                Source::TextLayer,
            ),
            b(
                BlockKind::Figure,
                "Apple",
                r(10.0, 100.0, 210.0, 300.0),
                Source::Onnx,
            ),
            b(
                BlockKind::Figure,
                "",
                r(10.0, 400.0, 110.0, 500.0),
                Source::Onnx,
            ),
        ];
        // A DocTags answer: a picture where the first figure is, a footer.
        let mut answer = vec![
            b(
                BlockKind::Heading(1),
                "Title",
                r(0.0, 0.0, 100.0, 20.0),
                Source::Vision,
            ),
            b(
                BlockKind::Figure,
                "",
                r(12.0, 102.0, 205.0, 295.0),
                Source::Vision,
            ),
            b(
                BlockKind::Paragraph,
                "text",
                r(0.0, 320.0, 100.0, 340.0),
                Source::Vision,
            ),
            b(
                BlockKind::PageFooter,
                "3",
                r(0.0, 800.0, 20.0, 810.0),
                Source::Vision,
            ),
        ];
        keep_figures(&mut answer, tools.clone());
        let kinds: Vec<(&BlockKind, Source)> = answer.iter().map(|b| (&b.kind, b.source)).collect();
        assert_eq!(
            kinds,
            [
                (&BlockKind::Heading(1), Source::Vision),
                (&BlockKind::Figure, Source::Onnx),
                (&BlockKind::Paragraph, Source::Vision),
                (&BlockKind::Figure, Source::Onnx),
                (&BlockKind::PageFooter, Source::Vision),
            ]
        );
        assert_eq!(answer[1].text, "Apple");
        assert_eq!(answer[1].bbox, tools[1].bbox);
        // A Markdown answer has no boxes: the figures follow its text.
        let mut answer = vec![b(BlockKind::Paragraph, "text", None, Source::Vision)];
        keep_figures(&mut answer, tools);
        assert_eq!(answer.len(), 3);
        assert!(answer[1..].iter().all(|b| b.kind == BlockKind::Figure));
    }

    /// S6: what a file with nothing read fails with is the reason a caller
    /// can act on first.
    #[test]
    fn a_stage_s_failure_is_the_gravest_reason_its_pages_met() {
        let mut s = Stage::default();
        assert_eq!(s.failure, None);
        s.fails(ErrorCode::ModelFailed);
        assert_eq!(s.failure, Some(ErrorCode::ModelFailed));
        s.fails(ErrorCode::ModelHeld);
        s.fails(ErrorCode::ModelFailed);
        assert_eq!(s.failure, Some(ErrorCode::ModelHeld));
        s.fails(ErrorCode::ModelNotAllowed);
        s.fails(ErrorCode::ModelHeld);
        assert_eq!(s.failure, Some(ErrorCode::ModelNotAllowed));
        s.fails(ErrorCode::DeadlineExceeded);
        assert_eq!(s.failure, Some(ErrorCode::DeadlineExceeded));
    }

    #[test]
    fn unread_pages_are_named_by_why_and_a_blind_fallback_by_its_name() {
        let mut stage = Stage::default();
        let mut unread = Unread::default();
        unread.blind.insert("example-text-only".into(), vec![3, 2]);
        unread.failed.insert("it refused".into(), vec![5]);
        unread.into_stage(&mut stage);
        let w = &stage.warnings[0];
        assert_eq!(
            (w.code, &w.parts),
            (WarningCode::ModelCannotSee, &vec![2, 3])
        );
        assert!(w.message.starts_with("pages 2-3 would have been read"));
        assert!(
            w.message.contains(
                "its fallback 'example-text-only' answers in its place and cannot read pages"
            ),
            "{}",
            w.message
        );
        assert!(w.message.contains("nothing was sent to it"));
        assert_eq!(stage.warnings[1].code, WarningCode::PartFailed);
        // A fallback that cannot see is a model that cannot run now.
        assert_eq!(stage.failure, Some(ErrorCode::ModelHeld));
        assert!(stage
            .said()
            .contains("; the page-reading model did not read page 5"));
    }
}
