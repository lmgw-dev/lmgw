//! Where a PDF page's figures go among its blocks, which blocks are their
//! captions, and one figure where a poppler image and an existing `figure`
//! block overlap (pictures/pdf.rs's module doc).
//!
//! Every step is one pass over the page's blocks per placed figure (at most
//! [`PICTURES_PER_PAGE_MAX`](super::super::PICTURES_PER_PAGE_MAX) a page) or
//! one pass over the blocks for all of them, so a page that draws an image a
//! hundred thousand times costs what its count costs.

use super::super::super::doc::{Block, BlockKind, Rect};
use super::super::caption;
use super::CAPTION_MAX_GAP_PT;

/// How much of the smaller of two boxes their overlap covers for an image
/// and an existing figure block to be one figure.
pub const MERGE_OVERLAP: f64 = 0.5;

pub(super) fn overlap_x(a: &Rect, b: &Rect) -> bool {
    b.x0 < a.x1 && a.x0 < b.x1
}

fn ordered(b: &Block) -> bool {
    b.bbox.is_some() && !matches!(b.kind, BlockKind::PageHeader | BlockKind::PageFooter)
}

/// Where a figure at `r` goes among `blocks` (pictures/pdf.rs's order),
/// `end` the index of the page's footer (or the block count).
pub(super) fn slot(blocks: &[Block], end: usize, r: &Rect) -> usize {
    let cy = r.cy();
    let at = |test: &dyn Fn(&Rect) -> bool| {
        blocks[..end]
            .iter()
            .position(|b| ordered(b) && b.bbox.as_ref().is_some_and(test))
    };
    if let Some(i) = at(&|b| overlap_x(b, r) && b.cy() >= cy) {
        return i;
    }
    if let Some(i) = blocks[..end]
        .iter()
        .rposition(|b| ordered(b) && b.bbox.as_ref().is_some_and(|b| overlap_x(b, r)))
    {
        return i + 1;
    }
    at(&|b| b.cy() >= cy).unwrap_or(end)
}

fn area(r: &Rect) -> f64 {
    f64::from(r.width().max(0.0)) * f64::from(r.height().max(0.0))
}

/// Whether `a` and `b` overlap by [`MERGE_OVERLAP`] of the smaller.
fn same_figure(a: &Rect, b: &Rect) -> bool {
    let i = Rect {
        x0: a.x0.max(b.x0),
        y0: a.y0.max(b.y0),
        x1: a.x1.min(b.x1),
        y1: a.y1.min(b.y1),
    };
    if i.x1 <= i.x0 || i.y1 <= i.y0 {
        return false;
    }
    let small = area(a).min(area(b));
    small > 0.0 && area(&i) >= MERGE_OVERLAP * small
}

/// The page's existing `figure` blocks (another reader's regions: WP8's
/// layout model finds vector figures) that overlap images of `kept` take
/// those images: the block keeps its place in reading order and its box
/// becomes the images' (poppler's, which the crop is cut from). The images
/// taken leave `kept`. Per block, whether it took one.
pub(super) fn merge(blocks: &mut [Block], kept: &mut Vec<Rect>) -> Vec<bool> {
    let mut took = vec![false; blocks.len()];
    for (i, b) in blocks.iter_mut().enumerate() {
        if b.kind != BlockKind::Figure {
            continue;
        }
        let Some(region) = b.bbox else {
            continue;
        };
        let mut union: Option<Rect> = None;
        kept.retain(|r| {
            if same_figure(r, &region) {
                union = Some(union.map_or(*r, |u| u.union(r)));
                false
            } else {
                true
            }
        });
        if let Some(u) = union {
            b.bbox = Some(u);
            took[i] = true;
        }
    }
    took
}

/// Whether `a` and `b` are alike: their overlap covers [`MERGE_OVERLAP`] of
/// the larger, so one is not just inside the other.
fn alike(a: &Rect, b: &Rect) -> bool {
    let i = Rect {
        x0: a.x0.max(b.x0),
        y0: a.y0.max(b.y0),
        x1: a.x1.min(b.x1),
        y1: a.y1.min(b.y1),
    };
    let large = area(a).max(area(b));
    i.x1 > i.x0 && i.y1 > i.y0 && large > 0.0 && area(&i) >= MERGE_OVERLAP * large
}

/// The page's existing `figure` blocks that took no image ([`merge`]) and
/// are left out as a picture of poppler's would be: one alike a left-out
/// image (`gone`: a logo repeated on the pages, a watermark, already
/// counted), or one smaller than `min_area` of the page (`page_area`),
/// counted in `small`. Their text, if any, leaves with them.
pub(super) fn leave_out(
    blocks: &mut Vec<Block>,
    took: &[bool],
    gone: &[Rect],
    page_area: f64,
    min_area: f64,
    small: &mut u32,
) {
    let mut i = 0;
    blocks.retain(|b| {
        let k = i;
        i += 1;
        if b.kind != BlockKind::Figure || b.figure.is_some() || took.get(k) == Some(&true) {
            return true;
        }
        let Some(r) = b.bbox else {
            return true;
        };
        if gone.iter().any(|g| alike(g, &r)) {
            return false;
        }
        if page_area > 0.0 && area(&r) / page_area < min_area {
            *small += 1;
            return false;
        }
        true
    });
}

/// For each block, the nearest block before and after it that is not a
/// figure: one pass each way.
pub(super) fn neighbours(blocks: &[Block]) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let n = blocks.len();
    let (mut prev, mut next) = (vec![None; n], vec![None; n]);
    let mut last = None;
    for (k, b) in blocks.iter().enumerate() {
        prev[k] = last;
        if b.kind != BlockKind::Figure {
            last = Some(k);
        }
    }
    last = None;
    for k in (0..n).rev() {
        next[k] = last;
        if blocks[k].kind != BlockKind::Figure {
            last = Some(k);
        }
    }
    (prev, next)
}

/// The caption of the figure at `k`: a caption the layout model marked
/// right after it or else right before it (one no other figure took and
/// that names no table), else its next non-figure block or the one before
/// when that paragraph reads as a caption. Close by and overlapping it
/// sideways either way. Its index.
pub(super) fn caption_of(
    blocks: &[Block],
    k: usize,
    prev: Option<usize>,
    next: Option<usize>,
    taken: &[bool],
) -> Option<usize> {
    let fig = blocks[k].bbox?;
    let near = |i: usize, below: bool| {
        let Some(r) = blocks[i].bbox else {
            return false;
        };
        let gap = if below { r.y0 - fig.y1 } else { fig.y0 - r.y1 };
        gap <= CAPTION_MAX_GAP_PT && overlap_x(&r, &fig)
    };
    let marked = |i: &usize, below: bool| {
        let b = &blocks[*i];
        b.kind == BlockKind::Caption
            && !taken.get(*i).copied().unwrap_or(true)
            && !caption::names_table(&b.text)
            && near(*i, below)
    };
    let reads = |i: &usize, below: bool| {
        let b = &blocks[*i];
        b.kind == BlockKind::Paragraph && caption::is_caption(&b.text) && near(*i, below)
    };
    next.filter(|i| marked(i, true))
        .or_else(|| prev.filter(|i| marked(i, false)))
        .or_else(|| next.filter(|i| reads(i, true)))
        .or_else(|| prev.filter(|i| reads(i, false)))
}
