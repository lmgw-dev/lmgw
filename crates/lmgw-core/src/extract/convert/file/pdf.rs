//! A PDF through poppler (`pdf.poppler`): the two geometry runs over the
//! span of pages the request needs (`-f`/`-l` from the first to the last
//! page of `pages`), then the layout pass ([`pdf_layout`]) and the pages
//! picked, the text-less ones told apart. The file takes one slot of the
//! tool pool for all three, or two when they are free ([`runs`]: the runs
//! side by side), so the runs' output is only held while the file has its
//! slot (the pool charges it, `pool::charge::PDF_FILE`), and a file never
//! holds one slot while it waits for another.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertWarning, ErrorCode, PartsBySource, WarningCode};

use super::super::caps::{self, Format, ToolId};
use super::super::doc::{Doc, Source};
use super::super::layout_model;
use super::super::model_help;
use super::super::model_help::select::Recognised;
use super::super::ocr::pages::PdfOcr;
use super::super::ocr::spool::{Shared, Spool};
use super::super::options::Plan;
use super::super::pages::page_ranges;
use super::super::pdf_layout::{self, Band, Repeated};
use super::super::pool;
use super::super::report;
use super::super::tools::{Fact, ToolFacts};
use super::super::watch::Watched;
use super::{fail, panicked, warning, Body, Failure, Read};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::pdf::{self, geometry, PdfError};

mod runs;

/// The `pdftotext` the snapshot found (the handler resolved, so it is there).
fn pdftotext(facts: &ToolFacts) -> PathBuf {
    match facts.fact(ToolId::Poppler) {
        Some(Fact::Found(f)) => f.located.path.clone(),
        _ => PathBuf::from(pdf::PDFTOTEXT),
    }
}

/// The `pdftohtml` beside it, when the probe found one.
fn pdftohtml(facts: &ToolFacts) -> Option<PathBuf> {
    let Some(Fact::Found(f)) = facts.fact(ToolId::Poppler) else {
        return None;
    };
    f.watched.iter().find_map(|w| match w {
        Watched::Binary { name, at: Some(at) } if name == geometry::PDFTOHTML => {
            Some(at.path.clone())
        }
        _ => None,
    })
}

/// What the assembly made of the pages.
pub(super) struct Assembled {
    total: u32,
    /// The asked-for pages past the last one, as ranges.
    beyond: Vec<String>,
    /// How many pages were asked for and exist.
    picked: u32,
    /// The picked pages, text-less ones included (with no blocks).
    doc: Doc,
    with_text: u32,
    /// The picked pages that have no text, ascending.
    without: Vec<u32>,
    /// The repeated lines the layout left out of the picked pages.
    repeated: Vec<Repeated>,
    /// For each page of `doc`, the [`Assembled::repeated`] lines left out
    /// of it (their indices).
    removed: Vec<Vec<u32>>,
}

impl Assembled {
    /// Count the pages read and unread again, after a later reader (the
    /// page-reading model) filled pages in.
    fn recount(&mut self) {
        self.without = self
            .doc
            .parts
            .iter()
            .filter(|p| p.source.is_none())
            .map(|p| p.number)
            .collect();
        self.with_text = self.picked.saturating_sub(self.without.len() as u32);
    }

    /// The repeated lines with the pages each is left out of, and how many
    /// more pages the page-reading model read since: the model transcribes
    /// a page whole, so nothing is left out of those, and they are not
    /// counted as if it were.
    fn repeated_now(&self) -> (Vec<(&Repeated, u32)>, u32) {
        let mut on_pages = vec![0u32; self.repeated.len()];
        let mut by_model = 0u32;
        for (part, ids) in self.doc.parts.iter().zip(&self.removed) {
            if ids.is_empty() {
                continue;
            }
            if part.source == Some(Source::Vision) {
                by_model += 1;
                continue;
            }
            for id in ids {
                if let Some(n) = on_pages.get_mut(*id as usize) {
                    *n += 1;
                }
            }
        }
        let lines = self
            .repeated
            .iter()
            .zip(on_pages)
            .filter(|(_, n)| *n > 0)
            .collect();
        (lines, by_model)
    }

    /// How many pages each source read.
    fn by_source(&self) -> PartsBySource {
        let mut by = PartsBySource::default();
        for p in &self.doc.parts {
            match p.source {
                Some(Source::Vision) => by.vision += 1,
                Some(Source::Ocr) => by.ocr += 1,
                Some(Source::Native) => by.native += 1,
                Some(Source::TextLayer | Source::Onnx) => by.text_layer += 1,
                Some(Source::Asr) => by.asr += 1,
                None => {}
            }
        }
        by
    }
}

/// How the font run went.
pub(super) enum Fonts {
    Read(Vec<u8>),
    /// `pdftohtml` is not there.
    Missing,
    /// It failed: poppler's words.
    Failed(String),
}

pub(super) async fn read(bytes: Bytes, plan: &Plan, facts: &ToolFacts) -> Result<Read, Failure> {
    // The file spooled once before its first run: every poppler run of it
    // reads the spool by position (`ocr::spool`), none is sent it whole.
    let spool = Arc::new(Spool::of(plan.spool_dir.as_deref(), bytes).await);
    // The pictures in runs of their own beside the text's (picture export,
    // `pictures::pdf`): the text never waits for them.
    let pictures = super::pictured::pdf_images(spool.clone(), plan, facts, pdftohtml(facts));
    let text = Box::pin(read_text(spool, plan, facts));
    // Each side in a lane of the file's clock: it counts while either works.
    let (r, images) = {
        let _both = pool::clock::paused();
        let pictures = async { Ok(pool::clock::beside(pictures).await) };
        futures::future::try_join(pool::clock::beside(text), pictures).await?
    };
    match images {
        Some(images) => super::pictured::pdf_place(r, images, plan).await,
        None => Ok(r),
    }
}

/// The file's text: its runs and layout in the file's slot.
async fn read_text(src: Arc<Spool>, plan: &Plan, facts: &ToolFacts) -> Result<Read, Failure> {
    let failed = |code, message: String| {
        let mut f = fail(Some(Format::Pdf), code, message);
        f.tool = Some(ToolId::Poppler);
        f
    };
    let span = plan.pages.as_ref().and_then(|p| p.span());
    let first = span.map_or(1, |s| s.0);
    let (text_bin, html) = (pdftotext(facts), pdftohtml(facts));
    // The layout model's regions, where it is chosen: before the file's
    // slot, which the model stage does not hold (`layout_model`).
    let regions = layout_model::stage(&src, plan.pages.as_ref(), facts).await;
    // The file's slot (or two): both runs and the layout (module doc).
    let runs::Ran { slot, bbox, fonts } =
        runs::geometry(&src, &text_bin, html.as_ref(), span).await;
    let bbox = match bbox {
        Ok(xml) => Ok(xml),
        Err(e @ (PdfError::ToolMissing | PdfError::NoReader)) => {
            return Err(failed(ErrorCode::ToolMissing, e.to_string()))
        }
        Err(PdfError::OutputTooLarge { .. }) => {
            return Err(failed(ErrorCode::OutputTooLarge, layout_too_large()))
        }
        Err(PdfError::NotStarted(m)) => return Err(failed(ErrorCode::ToolNotStarted, m)),
        // Past the wide limit too (`runs`), or not run again for the
        // deadline: the message names the limit.
        Err(e @ PdfError::OutOfMemory { .. }) => {
            return Err(failed(ErrorCode::MemoryLimit, e.to_string()))
        }
        // A first page past the last: no page asked for is there.
        Err(e) if first > 1 => match geometry::last_page_of(&e) {
            Some(total) => Err(total),
            None => return Err(failed(ErrorCode::Unreadable, e.to_string())),
        },
        Err(e) => return Err(failed(ErrorCode::Unreadable, e.to_string())),
    };
    let p = plan.clone();
    let assembled = pool::blocking_on(slot, move || match bbox {
        Ok(xml) => assemble_with(&xml, fonts, first, span, &p, regions.as_ref()),
        Err(total) => Ok((nothing_picked(total, &p), Vec::new())),
    })
    .await
    .map_err(|e| panicked(Some(Format::Pdf), e))?;
    let (mut a, font_notes) = assembled.map_err(|why| failed(ErrorCode::Unreadable, why))?;
    // The pages read one by one after this (text recognition, then the
    // page-reading model) render from the file's spool too.
    let spool = Shared::of(src);
    // Text recognition, after the file's own slot was given back: the
    // pages `ocr` asks for, each on a slot of its own (`ocr.rs`).
    let mut ocr = super::super::ocr::pages::pdf_pages(&spool, &mut a.doc.parts, plan, facts).await;
    // Tier 1, last: the pages the request's model help picks from what the
    // readers before it left (`model_help::select`), read by the
    // page-reading model in place (no slot is held here).
    let helped = match &plan.model_help {
        Some(help) => {
            let said = Recognised {
                poor: Some(&ocr.poor),
                empty: Some(&ocr.empty),
            };
            let stage = model_help::pdf::read_pages(help, &spool, &mut a.doc, facts, said).await;
            // A page recognition found no word on, that the model did not
            // read either, is a page nothing read.
            for p in &mut a.doc.parts {
                if p.source == Some(Source::Ocr) && ocr.empty.contains(&p.number) {
                    p.source = None;
                }
            }
            a.recount();
            stage
        }
        None => Default::default(),
    };
    drop(spool);
    // Recognition's page notes, without the pages the model read again.
    ocr.finish(&helped.read);
    // A file no page of which was read fails; when the model is why, with
    // the model's code and reasons, and with the calls that were made.
    let mut r = words(a, plan, facts, ocr).map_err(|mut f| {
        model_failure(&helped, &mut f);
        f
    })?;
    r.warnings.extend(font_notes);
    r.warnings.extend(helped.warnings.iter().cloned());
    r.partial |= helped.partial;
    r.model = helped;
    Ok(r)
}

/// A file none of whose pages was read, after the page-reading model was
/// to read them: the stage's code instead of "nothing reads scanned pages"
/// (a held model is a 503 that clears with the hold, not a 415), the
/// stage's reasons naming the pages, and the calls made for it kept in its
/// `stats` (they were paid for). A missing tool's hint stays in the message.
fn model_failure(helped: &model_help::pdf::Stage, f: &mut Failure) {
    f.model = (helped.calls, helped.models.clone());
    let said = helped.said();
    if said.is_empty() {
        return;
    }
    // Unreadable here is text recognition's: it read none of the pages.
    let unread = matches!(
        f.error.code,
        ErrorCode::NoReader | ErrorCode::ToolMissing | ErrorCode::Unreadable
    );
    match helped.failure.filter(|_| unread) {
        Some(code) => {
            f.error.message = match f.error.code {
                ErrorCode::ToolMissing => format!("{said}; without the model: {}", f.error.message),
                ErrorCode::Unreadable if f.error.message.is_empty() => format!(
                    "no page of this PDF has a text layer, and text recognition found no text \
                     on them; {said}"
                ),
                ErrorCode::Unreadable => format!(
                    "no page of this PDF has a text layer, and text recognition read none of \
                     them ({}); {said}",
                    f.error.message
                ),
                _ => format!("no page of this PDF has a text layer; {said}"),
            };
            f.error.code = code;
            f.error.tool = None;
        }
        None => {
            f.error.message.push_str("; ");
            f.error.message.push_str(&said);
        }
    }
}

/// Why a PDF's layout output was too large, with what to do.
pub(super) fn layout_too_large() -> String {
    format!(
        "this PDF's layout output (pdftotext -bbox-layout: every word with its box, about 17 \
         times the size of its text) passed MAX_UNCOMPRESSED_BYTES ({}) and the run was \
         stopped; ask for fewer pages at a time with pages",
        crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
    )
}

/// The file's answer when the request's first page is past its last.
fn nothing_picked(total: u32, plan: &Plan) -> Assembled {
    let beyond = plan
        .pages
        .as_ref()
        .map(|s| s.pick(total).beyond)
        .unwrap_or_default();
    Assembled {
        total,
        beyond,
        picked: 0,
        doc: Doc::default(),
        with_text: 0,
        without: Vec::new(),
        repeated: Vec::new(),
        removed: Vec::new(),
    }
}

/// The ending every font note shares.
const FONTS_FROM_BOXES: &str =
    "so headings were told by the size of their lines alone and bold headings were not seen";

/// The notes the font run gives: missing or failed, output that stopped
/// being readable, or picked pages it gave no runs for.
fn font_notes(fonts: &Fonts, layout: &pdf_layout::Layout, picked: &[u32]) -> Vec<ConvertWarning> {
    let with_text = layout.doc.parts.iter().any(|p| p.source.is_some());
    let mut parts = Vec::new();
    let (code, message) = match fonts {
        Fonts::Missing => (
            WarningCode::ToolMissing,
            format!("pdftohtml (part of poppler-utils) was not found, {FONTS_FROM_BOXES}"),
        ),
        Fonts::Failed(why) => (
            WarningCode::PartFailed,
            format!("pdftohtml could not read this file's fonts ({why}), {FONTS_FROM_BOXES}"),
        ),
        Fonts::Read(_) => {
            parts = layout
                .pages_without_fonts
                .iter()
                .copied()
                .filter(|n| picked.binary_search(n).is_ok())
                .collect();
            match (&layout.fonts_error, layout.sizes_from_boxes) {
                (Some(why), _) => (
                    WarningCode::PartFailed,
                    format!(
                        "pdftohtml's font output for this file {why}; on the pages it had not \
                         reached, {FONTS_FROM_BOXES}"
                    ),
                ),
                (None, true) if with_text => (
                    WarningCode::PartFailed,
                    format!("pdftohtml found no text in this file, {FONTS_FROM_BOXES}"),
                ),
                (None, false) if !parts.is_empty() => (
                    WarningCode::PartFailed,
                    format!(
                        "pdftohtml gave no fonts for pages {}, {FONTS_FROM_BOXES} on them",
                        page_ranges(&parts)
                    ),
                ),
                _ => return Vec::new(),
            }
        }
    };
    if !with_text {
        return Vec::new();
    }
    let mut w = warning(code, message, parts);
    w.tool = Some(ToolId::Poppler.as_str().to_string());
    vec![w]
}

/// [`assemble_with`] without a layout model's regions.
#[cfg(test)]
pub(super) fn assemble(
    bbox: &[u8],
    fonts: Fonts,
    first: u32,
    span: Option<(u32, u32)>,
    plan: &Plan,
) -> Result<(Assembled, Vec<ConvertWarning>), String> {
    assemble_with(bbox, fonts, first, span, plan, None)
}

/// The layout of the run's pages (the first is `first`; `span` is what was
/// asked of poppler), the picked ones kept; the pages a layout model read
/// are laid out from `regions`. CPU-bound: runs in a slot.
pub(super) fn assemble_with(
    bbox: &[u8],
    fonts: Fonts,
    first: u32,
    span: Option<(u32, u32)>,
    plan: &Plan,
    regions: Option<&layout_model::Regions>,
) -> Result<(Assembled, Vec<ConvertWarning>), String> {
    let fonts_xml = match &fonts {
        Fonts::Read(x) => Some(x.as_slice()),
        _ => None,
    };
    let layout = pdf_layout::layout_with(bbox, fonts_xml, first, regions)?;
    let got = u32::try_from(layout.doc.parts.len()).unwrap_or(u32::MAX);
    // poppler stops at the last page: fewer pages than asked means the file
    // ends there; as many means it has at least the span's last.
    let total = match span {
        None => got,
        Some((f, l)) if got < l.saturating_sub(f).saturating_add(1) => {
            f.saturating_add(got).saturating_sub(1)
        }
        Some((_, l)) => l,
    };
    let mut a = Assembled {
        total,
        beyond: Vec::new(),
        picked: 0,
        doc: Doc::default(),
        with_text: 0,
        without: Vec::new(),
        repeated: Vec::new(),
        removed: Vec::new(),
    };
    // A bitmap over the run's pages, so each asks "picked?" in constant time.
    let wanted = match plan.pages.as_ref().map(|s| s.pick(total)) {
        None => None,
        Some(p) => {
            let mut w = vec![false; layout.doc.parts.len()];
            for n in &p.pages {
                if let Some(b) = n.checked_sub(first).and_then(|i| w.get_mut(i as usize)) {
                    *b = true;
                }
            }
            a.beyond = p.beyond;
            Some(w)
        }
    };
    let picked: Vec<u32> = layout
        .doc
        .parts
        .iter()
        .enumerate()
        .filter(|(i, _)| wanted.as_ref().is_none_or(|w| w[*i]))
        .map(|(_, p)| p.number)
        .collect();
    let mut notes = font_notes(&fonts, &layout, &picked);
    if let Some(regions) = regions {
        notes.extend(layout_model::notes(regions, &layout.model, &picked));
    }
    drop(fonts);
    // The repeated lines are named only for the picked pages they left
    // ([`Assembled::repeated_now`]).
    let mut removed = layout.removed;
    for (i, part) in layout.doc.parts.into_iter().enumerate() {
        if wanted.as_ref().is_some_and(|w| !w[i]) {
            continue;
        }
        a.removed
            .push(removed.get_mut(i).map(std::mem::take).unwrap_or_default());
        a.picked += 1;
        if part.source.is_some() {
            a.with_text += 1;
        } else {
            a.without.push(part.number);
        }
        a.doc.parts.push(part);
    }
    a.repeated = layout.repeated;
    Ok((a, notes))
}

/// The `header_removed` warning: each repeated line once, with the
/// picked pages it was left out of. `by_model` pages that had such lines
/// were read by the page-reading model since: they are not counted, and
/// the warning says what became of the lines there.
fn repeated_warning(r: &[(&Repeated, u32)], by_model: u32) -> ConvertWarning {
    let lines: Vec<String> = r
        .iter()
        .map(|(x, pages)| {
            let at = match x.band {
                Band::Top => "top",
                Band::Bottom => "bottom",
            };
            let numbered = if x.numbered {
                ", its page number changing with the page"
            } else {
                ""
            };
            format!("\"{}\" ({at}, {pages} pages{numbered})", x.text)
        })
        .collect();
    let model = match by_model {
        0 => String::new(),
        n => format!(
            "; {n} more page(s) with such lines were read by the page-reading model, which \
             writes a page as it sees it, so there the lines are part of the text where the \
             model wrote them (pages a DocTags model read keep them as page_header and \
             page_footer blocks)"
        ),
    };
    let mut w = warning(
        WarningCode::HeaderRemoved,
        format!(
            "{} line(s) repeated at the top or bottom of the pages were left out of the text as \
             page headers and footers (json keeps them as page_header and page_footer blocks): \
             {}{model}",
            r.len(),
            lines.join("; ")
        ),
        Vec::new(),
    );
    w.tool = Some(ToolId::Poppler.as_str().to_string());
    w
}

/// The file's [`Read`] from the assembly, with the warnings or the error
/// the gaps give.
pub(super) fn words(
    mut a: Assembled,
    plan: &Plan,
    facts: &ToolFacts,
    ocr: PdfOcr,
) -> Result<Read, Failure> {
    let mut warnings = Vec::new();
    // What text recognition and the page-reading model read replaced
    // their pages.
    a.recount();
    if !a.beyond.is_empty() {
        let message = format!(
            "pages {} are not in this PDF, which has {}",
            a.beyond.join(", "),
            a.total
        );
        if a.picked == 0 {
            let mut f = fail(Some(Format::Pdf), ErrorCode::PagesOutOfRange, message);
            f.tool = Some(ToolId::Poppler);
            return Err(f);
        }
        warnings.push(warning(WarningCode::PagesOutOfRange, message, Vec::new()));
    }
    if !a.without.is_empty() && a.with_text == 0 {
        // No page was read: the file fails, by recognition's own words
        // when it took charge of the pages.
        let (code, message) = if ocr.handled {
            let said: Vec<&str> = ocr.warnings.iter().map(|w| w.message.as_str()).collect();
            // A missing tool or language pack is what to install.
            let missing = ocr.warnings.iter().any(|w| {
                matches!(
                    w.code,
                    WarningCode::ToolMissing | WarningCode::OcrLanguageMissing
                )
            });
            let code = if missing {
                ErrorCode::ToolMissing
            } else {
                ErrorCode::Unreadable
            };
            (code, said.join("; "))
        } else {
            let mut w = textless_warning(&a.without, facts, plan);
            w.1.push_str(model_help::pdf::unread_hint(plan.model_help.is_some()));
            (w.0, w.1)
        };
        let mut f = fail(Some(Format::Pdf), code, message);
        f.tool = Some(if ocr.handled {
            ToolId::Tesseract
        } else {
            ToolId::Poppler
        });
        return Err(f);
    }
    if !a.without.is_empty() && !ocr.handled {
        let mut w = textless_warning(&a.without, facts, plan);
        w.1.push_str(model_help::pdf::unread_hint(plan.model_help.is_some()));
        warnings.push(warning(w.2, w.1, a.without.clone()));
    }
    warnings.extend(ocr.warnings);
    let (repeated, by_model) = a.repeated_now();
    if !repeated.is_empty() {
        warnings.push(repeated_warning(&repeated, by_model));
    }
    drop(repeated);
    Ok(Read {
        format: Format::Pdf,
        parts: a.picked,
        by_source: a.by_source(),
        body: Body::Doc(a.doc),
        warnings,
        partial: !a.without.is_empty(),
        tool: Some(ToolId::Poppler),
        also: ocr.ran.then_some(ToolId::Tesseract).into_iter().collect(),
        part_headings: 0,
        charged: 0,
        model: Default::default(),
    })
}

/// What pages without a text layer get, from the table's row for them: the
/// file's error code if none of its pages had text, the message, and the
/// warning's code otherwise. The pages are named as ranges.
fn textless_warning(
    pages: &[u32],
    facts: &ToolFacts,
    plan: &Plan,
) -> (ErrorCode, String, WarningCode) {
    let list = page_ranges(pages);
    let which = if pages.len() == 1 {
        format!("page {list} has no text layer, so it was not read")
    } else {
        format!("pages {list} have no text layer, so they were not read")
    };
    let r = caps::resolve(Format::PdfPageWithoutText, facts);
    match (r.no_reader, r.missing.is_empty()) {
        (Some(why), _) => (
            ErrorCode::NoReader,
            format!("{which}: {why}"),
            WarningCode::NoReader,
        ),
        (None, false) => (
            ErrorCode::ToolMissing,
            format!(
                "{which}: {}",
                report::needs_said(&r.missing, facts, plan.platform)
            ),
            WarningCode::ToolMissing,
        ),
        // Text recognition reads them, and the request turned it off.
        (None, true) => (
            ErrorCode::NoReader,
            format!("{which}: ocr is off; send ocr auto to recognise their text"),
            WarningCode::NoReader,
        ),
    }
}

#[cfg(test)]
mod tests;
