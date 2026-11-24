//! poppler geometry (document-conversion design §5.1, tier 0, WP3): a PDF's
//! text layer as a [`Doc`] of pages, each with its blocks in reading order:
//! headings, paragraphs, list items, tables, and the repeated page headers
//! and footers marked.
//!
//! Two poppler runs feed it (`extract/pdf/geometry.rs`):
//! - `pdftotext -bbox-layout`: pages, blocks, lines and words with their
//!   boxes ([`bbox`]);
//! - `pdftohtml -xml -i -zoom 1`: text runs with their font size and
//!   weight ([`fonts`]), matched to the lines by position. Without it (the
//!   run failed, or the binary is missing) a line's size is read from its
//!   box and no line is bold, and the file says so.
//!
//! The pass, each step linear in the page's lines (or `n log n` where it
//! sorts):
//! 1. [`table`]: rows of side-by-side lines whose cells line up become
//!    tables;
//! 2. [`repeat`]: lines in the top or bottom band of the pages (outside
//!    the tables) whose text repeats on enough pages, as it is or but for
//!    the page number, become page headers and footers;
//! 3. [`order`]: poppler's blocks (what is left of them) and the tables are
//!    put in reading order: column by column where the page has columns,
//!    side-by-side blocks that are no columns row by row;
//! 4. [`blocks`]: each block's lines become headings, paragraphs and list
//!    items, heading levels ranked by size across the document.
//!
//! Hostile input: the XML is parsed from an untrusted document's output.
//! Its size is bounded by the run's output budget (`MAX_UNCOMPRESSED_BYTES`);
//! nesting is fixed by poppler's format and anything out of place is
//! attached to an implicit parent, never recursed into; every number is
//! checked to be finite; nothing indexes without a bound check.

mod bbox;
mod bidi;
mod blocks;
mod fonts;
mod order;
mod regions;
mod repeat;
mod table;

use super::doc::{Block, BlockKind, Doc, Part, PartKind, Rect, Source};
use super::layout_model::{Regions, Used};

pub use regions::{MAX_REGIONS, ORPHAN_MAX_SHARE, PICTURE_TEXT_MAX_SHARE};
pub use repeat::{Band, Repeated, HEADER_BAND, HEADER_MIN_PAGES, HEADER_MIN_SHARE};

/// One line of a page as the pass sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Line {
    pub rect: Rect,
    /// Its words joined by spaces.
    pub text: String,
    /// Its font size in points (from `pdftohtml`, else its box's height).
    pub size: f32,
    pub bold: bool,
    /// The poppler block it is in (page-wide index).
    pub block: u32,
    pub role: Role,
}

/// What a line became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Body,
    /// A repeated page header: the index of its [`Repeated`].
    Header(u32),
    /// A repeated page footer, likewise.
    Footer(u32),
    /// A table's cell; the table is found by its index on the page.
    Cell(u32),
}

/// One page as parsed: its size and its lines in poppler's order.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Page {
    pub width: f32,
    pub height: f32,
    pub lines: Vec<Line>,
}

/// What the pass made of a PDF's pages.
#[derive(Debug)]
pub struct Layout {
    /// One part per page poppler wrote, numbered from the run's first page.
    pub doc: Doc,
    /// The repeated header and footer lines that were marked.
    pub repeated: Vec<Repeated>,
    /// For each part, the [`Layout::repeated`] lines left out of it, each
    /// index once.
    pub removed: Vec<Vec<u32>>,
    /// Font sizes were read from the boxes and bold was not seen
    /// (`pdftohtml` gave nothing).
    pub sizes_from_boxes: bool,
    /// Where `pdftohtml`'s output stopped being readable (the runs before
    /// it were used).
    pub fonts_error: Option<String>,
    /// The pages with text that `pdftohtml` gave no runs for, when it gave
    /// some: their sizes came from the boxes and no line on them is bold.
    pub pages_without_fonts: Vec<u32>,
    /// How a layout model's regions were used, page by page (nothing
    /// without one).
    pub model: Used,
}

/// A number from poppler's output: finite, or 0.
pub(crate) fn finite(s: &str) -> f32 {
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// Lay out the pages of `bbox_xml` (`pdftotext -bbox-layout`), the first
/// of which is page `first`, with the font runs of `fonts_xml`
/// (`pdftohtml -xml`) where there are any. CPU-bound: runs in a slot.
pub fn layout(bbox_xml: &[u8], fonts_xml: Option<&[u8]>, first: u32) -> Result<Layout, String> {
    layout_with(bbox_xml, fonts_xml, first, None)
}

/// [`layout`], the pages a layout model read laid out from its regions
/// ([`regions`]): a page it did not read, and one whose regions do not
/// hold up, goes through the pass above as without the model.
pub fn layout_with(
    bbox_xml: &[u8],
    fonts_xml: Option<&[u8]>,
    first: u32,
    model: Option<&Regions>,
) -> Result<Layout, String> {
    let mut pages = bbox::pages(bbox_xml)?;
    let read = fonts_xml.map(fonts::runs).unwrap_or_default();
    let (sizes_from_boxes, pages_without_fonts) = if read.runs.is_empty() {
        fonts::from_boxes(&mut pages);
        (true, Vec::new())
    } else {
        (false, fonts::apply(&mut pages, &read.runs, first))
    };
    let fonts_error = read.error;
    drop(read.runs);
    let displayed = read.sizes;
    // Where the model read the page, its regions say what is a table.
    let number = |i: usize| first.saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
    let mut used = Used::default();
    let taken: Vec<Option<regions::Taken>> = pages
        .iter_mut()
        .enumerate()
        .map(|(i, page)| {
            let raw = model?.page(number(i))?;
            let size = displayed.get(&number(i)).copied();
            match regions::take(page, raw, size) {
                Ok(taken) => Some(taken),
                Err(why) => {
                    used.implausible.push((number(i), why));
                    None
                }
            }
        })
        .collect();
    let from_regions: Vec<bool> = taken.iter().map(Option::is_some).collect();
    // Tables first, so a table's lines are never taken for page headers.
    let tables: Vec<_> = pages
        .iter_mut()
        .zip(&from_regions)
        .map(|(page, model)| match model {
            true => Vec::new(),
            false => table::find(page),
        })
        .collect();
    let repeated = repeat::mark(&mut pages);
    let style = blocks::Style::of(&pages);
    let mut doc = Doc::default();
    let mut heads = blocks::Headings::default();
    let mut laid = Vec::with_capacity(pages.len());
    // The page headers and footers the model marked, per page.
    let mut marked: Vec<(Vec<Block>, Vec<Block>)> = Vec::with_capacity(pages.len());
    for (i, ((page, tables), taken)) in pages.iter().zip(tables).zip(taken).enumerate() {
        let segs = match taken {
            Some(taken) => {
                let l = regions::segments(page, taken, &style, &mut heads, i == 0);
                used.pages.insert(number(i), l.used);
                marked.push((l.headers, l.footers));
                l.segs
            }
            None => {
                marked.push(Default::default());
                let units = order::units(page, tables);
                blocks::segments(page, units, &style, &mut heads, i == 0)
            }
        };
        laid.push(segs);
    }
    regions::join_pages(&mut laid, &from_regions);
    let levels = heads.levels();
    let mut removed = Vec::with_capacity(pages.len());
    for (i, ((page, segs), (headers, footers))) in pages.iter().zip(laid).zip(marked).enumerate() {
        let number = number(i);
        let mut out: Vec<Block> = Vec::new();
        let mut gone: Vec<u32> = Vec::new();
        for l in &page.lines {
            if let Role::Header(id) = l.role {
                gone.push(id);
                out.push(Block::new(
                    BlockKind::PageHeader,
                    l.text.clone(),
                    Some(l.rect),
                    Source::TextLayer,
                ));
            }
        }
        out.extend(headers);
        out.extend(blocks::blocks(segs, &levels));
        out.extend(footers);
        for l in &page.lines {
            if let Role::Footer(id) = l.role {
                gone.push(id);
                out.push(Block::new(
                    BlockKind::PageFooter,
                    l.text.clone(),
                    Some(l.rect),
                    Source::TextLayer,
                ));
            }
        }
        gone.sort_unstable();
        gone.dedup();
        removed.push(gone);
        let has_text = !page.lines.is_empty();
        doc.parts.push(Part {
            kind: PartKind::Page,
            number,
            name: None,
            size: Some(displayed_size(
                (page.width, page.height),
                displayed.get(&number).copied(),
            )),
            source: has_text.then_some(Source::TextLayer),
            blocks: out,
        });
    }
    Ok(Layout {
        doc,
        repeated,
        removed,
        sizes_from_boxes,
        fonts_error,
        pages_without_fonts,
        model: used,
    })
}

/// A page's size as displayed, the space its boxes and its pictures' are
/// in: `pdftotext`'s (`unturned`, exact) turned when pdftohtml's
/// (`shown`, whole points) is the other way round, so a `/Rotate 90` page
/// of a portrait sheet is landscape.
pub(super) fn displayed_size(unturned: (f32, f32), shown: Option<(f32, f32)>) -> (f32, f32) {
    let (w, h) = unturned;
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    match shown {
        Some((sw, sh)) if near(sw, h) && near(sh, w) && !near(sw, w) => (h, w),
        _ => (w, h),
    }
}

#[cfg(test)]
mod tests;
