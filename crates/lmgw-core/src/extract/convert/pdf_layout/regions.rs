//! A page's blocks from a layout model's regions (document-conversion
//! design §10 WP8): where [`super::super::layout_model`] read a page, its
//! regions say what each piece of the page is, and poppler's lines give the
//! words.
//!
//! - [`assign`]: which lines each region takes, overlaps resolved, lines
//!   no region took kept as text regions; or `None`, and the page is laid
//!   out by poppler's geometry as without the model.
//! - The regions are put in reading order by the columns rule the
//!   geometry pass uses ([`super::order`]), over regions instead of
//!   poppler's blocks. docling's own rule (a graph of what lies above
//!   what) is not built yet.
//! - Each region becomes a block of its kind: a title or section header a
//!   heading (its level from its font size, ranked across the document as
//!   the geometry pass ranks them), text a paragraph, a list item an item
//!   (its bullet taken off; one without a bullet that starts with a number,
//!   `1.` or `2)`, a numbered item with that marker), a table region a
//!   table ([`table`]), a picture a `figure` block, a caption, a formula and
//!   code blocks of those kinds, a footnote a paragraph. Page headers and
//!   footers are marked and left out of the text.
//! - A paragraph that goes on in the next column, or on the next page, is
//!   one block (docling's `predict_merges`): the one before ends in a
//!   lower-case letter, a comma or a hyphen and the next starts in lower
//!   case. The block keeps the box of its first part.
//!
//! The lines are whatever the page has, with their boxes: poppler's text
//! layer today, recognised words later. Nothing here asks where a line
//! came from.
//!
//! **Pictures.** A picture region is a `figure` block with the region's
//! box, a vector drawing as much as an image (design D22), read by
//! [`Source::Onnx`]. Cropping it is picture export's. Text inside it (a
//! chart's labels) is the figure block's text: json keeps it, markdown and
//! text leave it out, and the file says how many lines.
//!
//! **Formulas and code** (design D23): the region is marked and its text
//! kept as the text layer has it; code keeps its lines and their
//! indentation. Reading a formula as LaTeX is the page-reading model's.

mod assign;
mod table;

use super::super::doc::{Block, BlockKind, Rect, Source};
use super::super::layout_model::{Label, PageUse};
use super::blocks::{self, Headings, Seg, Style, HEADING_SIZE_RATIO};
use super::order::{self, Unit, What};
use super::{Line, Page, Role};

pub(super) use assign::{take, Taken};
pub use assign::{MAX_REGIONS, ORPHAN_MAX_SHARE, PICTURE_TEXT_MAX_SHARE};

/// What a page became.
#[derive(Debug, Default)]
pub(super) struct Laid {
    pub segs: Vec<Seg>,
    /// The page header lines the model marked, as blocks.
    pub headers: Vec<Block>,
    pub footers: Vec<Block>,
    pub used: PageUse,
}

/// A paragraph that may go on: it ends in a lower-case letter, a comma, a
/// hyphen or a soft hyphen (docling's rule).
fn open_end(text: &str) -> bool {
    let t = text.trim_end();
    t.chars().count() >= 2
        && matches!(
            t.chars().next_back(),
            Some('a'..='z' | ',' | '-' | '\u{ad}')
        )
}

/// A list item's number and the text after it: up to nine digits, `.` or
/// `)`, a space and more (markdown's own ordered item).
fn ordinal(text: &str) -> Option<(&str, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let marker = text.get(..=digits)?;
    let rest = text.get(digits + 1..)?.strip_prefix(' ')?.trim_start();
    (marker.ends_with(['.', ')']) && !rest.is_empty()).then_some((marker, rest))
}

/// A paragraph that goes on from one before: it starts in lower case.
fn goes_on(text: &str) -> bool {
    let mut chars = text.trim_start().chars();
    chars.next().is_some_and(char::is_lowercase) && chars.next().is_some()
}

/// A segment a paragraph's flow passes over: a table, a figure, a caption
/// or a footnote.
fn passed_over(seg: &Seg) -> bool {
    match seg {
        Seg::Table(_) => true,
        Seg::Block(b) => matches!(
            b.kind,
            BlockKind::Figure | BlockKind::Caption | BlockKind::Paragraph
        ),
        _ => false,
    }
}

/// The region's lines that are still body (a repeated header line found
/// since is the geometry pass's), in poppler's order.
fn body<'a>(page: &'a Page, lines: &[usize]) -> Vec<&'a Line> {
    lines
        .iter()
        .filter_map(|&i| page.lines.get(i))
        .filter(|l| l.role == Role::Body)
        .collect()
}

fn joined(lines: &[&Line]) -> String {
    let mut text = String::new();
    for l in lines {
        blocks::join(&mut text, &l.text);
    }
    text
}

fn rect_of(lines: &[&Line]) -> Rect {
    Rect::union_all(lines.iter().map(|l| &l.rect)).unwrap_or_default()
}

/// Code as its lines, each indented by where it starts: the left edge's
/// distance from the block's, in the block's average character width. A
/// block whose indentation would be longer than its text is written
/// without any: such geometry is not code's, and the output stays within
/// twice the text.
fn code(lines: &[&Line]) -> String {
    let left = lines
        .iter()
        .map(|l| l.rect.x0)
        .fold(f32::INFINITY, f32::min);
    let (width, chars) = lines.iter().fold((0.0f32, 0usize), |(w, c), l| {
        (w + l.rect.width().max(0.0), c + l.text.chars().count())
    });
    let per_char = if chars > 0 { width / chars as f32 } else { 0.0 };
    let indent = |l: &Line| match per_char > 0.0 {
        true => ((l.rect.x0 - left) / per_char).round().max(0.0) as usize,
        false => 0,
    };
    let total = lines
        .iter()
        .fold(0usize, |n, l| n.saturating_add(indent(l)));
    let indented = total <= chars;
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if indented {
            out.extend(std::iter::repeat_n(' ', indent(l)));
        }
        out.push_str(&l.text);
    }
    out
}

fn block(kind: BlockKind, text: String, rect: Rect) -> Seg {
    Seg::Block(Block::new(kind, text, Some(rect), Source::TextLayer))
}

/// `page`'s segments from the regions `taken` gave lines to, in reading
/// order, the heading sizes recorded in `heads`.
pub(super) fn segments(
    page: &Page,
    taken: Taken,
    style: &Style,
    heads: &mut Headings,
    first_page: bool,
) -> Laid {
    let mut laid = Laid::default();
    let mut units: Vec<Unit> = Vec::with_capacity(taken.regs.len());
    for (k, r) in taken.regs.iter().enumerate() {
        let furniture = match r.label {
            Label::PageHeader => Some(true),
            Label::PageFooter => Some(false),
            _ => None,
        };
        let Some(top) = furniture else {
            units.push(Unit {
                rect: r.rect,
                what: What::Region(k),
            });
            continue;
        };
        for l in body(page, &r.lines) {
            let kind = if top {
                BlockKind::PageHeader
            } else {
                BlockKind::PageFooter
            };
            let b = Block::new(kind, l.text.clone(), Some(l.rect), Source::TextLayer);
            laid.used.furniture.push((top, l.text.clone()));
            if top {
                laid.headers.push(b);
            } else {
                laid.footers.push(b);
            }
        }
    }
    let mut segs: Vec<Seg> = Vec::with_capacity(units.len());
    for u in order::order(page, units) {
        let What::Region(k) = u.what else {
            continue;
        };
        let Some(r) = taken.regs.get(k) else {
            continue;
        };
        match r.label {
            Label::Table | Label::DocumentIndex => {
                // A table region without a word is a picture of a table.
                segs.push(match table::build(&page.lines, &r.lines, r.rect) {
                    Some(found) => Seg::Table(found),
                    None => Seg::Block(Block::new(
                        BlockKind::Figure,
                        String::new(),
                        Some(r.rect),
                        Source::Onnx,
                    )),
                });
                continue;
            }
            Label::Picture => {
                let lines = body(page, &r.lines);
                laid.used.picture_lines = laid
                    .used
                    .picture_lines
                    .saturating_add(u32::try_from(lines.len()).unwrap_or(u32::MAX));
                let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                segs.push(Seg::Block(Block::new(
                    BlockKind::Figure,
                    text.join("\n"),
                    Some(r.rect),
                    Source::Onnx,
                )));
                continue;
            }
            _ => {}
        }
        let lines = body(page, &r.lines);
        if lines.is_empty() {
            continue;
        }
        let rect = rect_of(&lines);
        segs.push(match r.label {
            Label::Title | Label::SectionHeader => {
                let size = lines.iter().map(|l| l.size).fold(0.0f32, f32::max);
                let big = style.body() > 0.0 && size >= style.body() * HEADING_SIZE_RATIO;
                let key = big.then(|| blocks::key(size));
                heads.record(key, first_page);
                Seg::Heading(key, joined(&lines), rect)
            }
            Label::ListItem => {
                let text = joined(&lines);
                match (blocks::bullet(&text), ordinal(&text)) {
                    (Some(rest), _) => Seg::Item(rest.to_string(), rect),
                    (None, Some((marker, rest))) => block(
                        BlockKind::NumberedItem(marker.to_string()),
                        rest.to_string(),
                        rect,
                    ),
                    (None, None) => Seg::Item(text, rect),
                }
            }
            Label::Caption => block(BlockKind::Caption, joined(&lines), rect),
            Label::Footnote => block(BlockKind::Paragraph, joined(&lines), rect),
            Label::Formula => block(BlockKind::Formula, joined(&lines), rect),
            Label::Code => block(BlockKind::Code, code(&lines), rect),
            _ => Seg::Para(joined(&lines), rect),
        });
    }
    laid.segs = in_columns(segs);
    laid
}

/// Paragraphs that go on in the next column joined (module doc): the
/// next paragraph, tables, figures, captions and footnotes passed over,
/// lies wholly to the right of the one that began.
fn in_columns(segs: Vec<Seg>) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::with_capacity(segs.len());
    // The paragraph that may go on: its place in `out` and its first box.
    let mut head: Option<(usize, Rect)> = None;
    for seg in segs {
        match (&seg, head) {
            (Seg::Para(text, rect), Some((at, first))) => {
                let on = first.x1 < rect.x0
                    && goes_on(text)
                    && matches!(&out[at], Seg::Para(t, _) if open_end(t));
                if on {
                    if let Seg::Para(t, _) = &mut out[at] {
                        blocks::join(t, text.trim_start());
                    }
                    continue;
                }
                head = Some((out.len(), *rect));
            }
            (Seg::Para(_, rect), None) => head = Some((out.len(), *rect)),
            (s, _) if passed_over(s) => {}
            _ => head = None,
        }
        out.push(seg);
    }
    out
}

/// Paragraphs that go on across a page break joined: for each pair of
/// neighbouring pages both laid out from regions (`model`), the first
/// page's last paragraph and the next page's first, what the flow passes
/// over aside.
pub(super) fn join_pages(laid: &mut [Vec<Seg>], model: &[bool]) {
    for p in 1..laid.len() {
        if !(model.get(p - 1) == Some(&true) && model.get(p) == Some(&true)) {
            continue;
        }
        let (before, after) = laid.split_at_mut(p);
        let (prev, next) = (&mut before[p - 1], &mut after[0]);
        let last = prev.iter().rposition(|s| !passed_over(s));
        let first = next.iter().position(|s| !passed_over(s));
        let (Some(a), Some(b)) = (last, first) else {
            continue;
        };
        let on = matches!((&prev[a], &next[b]), (Seg::Para(t, _), Seg::Para(n, _)) if open_end(t) && goes_on(n));
        if !on {
            continue;
        }
        if let (Seg::Para(t, _), Seg::Para(n, _)) = (&mut prev[a], next.remove(b)) {
            blocks::join(t, n.trim_start());
        }
    }
}
