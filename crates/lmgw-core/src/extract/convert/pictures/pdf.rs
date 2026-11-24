//! A PDF's pictures (design §5.3, D22): which pages may have one
//! ([`gate`]), where `pdftohtml` drew each raster image on them ([`runs`],
//! [`images`]), which of them are pictures, and their `figure` blocks
//! placed in the reading order the layout pass gave the page's blocks
//! ([`place`]).
//!
//! **The runs.** The text and its fonts come from the file's own runs
//! (`pdftotext -bbox-layout`, `pdftohtml -xml -i`), whatever images the
//! file holds. The pictures come from their own `pdftohtml -xml -dataurls`
//! runs over the pages the gate picked, beside the text runs: that run
//! encodes every image it meets as PNG (the cost of picture export), so a
//! page whose images are all page covers or clearly too small has none, and
//! a run that fails costs its pages' pictures only, named.
//!
//! `pdftohtml -xml -dataurls` (poppler 26.08, checked 2026-11-18) writes,
//! inside each `<page number top left height width>`, one
//! `<image top left width height src="data:image/…;base64,…"/>` per image
//! drawn (inline images and stencil masks too), in points at `-zoom 1`, the
//! coordinates `pdftotext -bbox-layout` gives its words. The page's width
//! and height are as displayed (a `/Rotate 90` page of 612 × 792 is 792 ×
//! 612), and an image on a rotated page or drawn mirrored has a negative
//! width or height: its box is the span of `left` and `left + width` (and
//! `top`, `top + height`), checked for `/Rotate` 0/90/180/270 and a
//! mirroring matrix on 2026-11-19. The data itself is not kept: only a hash
//! of it, which tells the same image drawn again (a logo on every page)
//! apart from a different one in the same place.
//!
//! **Order** ([`order`]). The layout pass ordered the page's text blocks
//! (columns, sections, tables); a figure goes before the first block in
//! that order that it overlaps sideways and whose middle is below its own
//! (its column's next block, a caption under it), else after the last such
//! block above it, else among the blocks by height. A `figure` block the
//! page has already (another reader's region) that overlaps an image takes
//! it, so they are one figure.
//!
//! **Captions.** The block right after the figure (or else right before
//! it) is its caption when it reads as one (`caption.rs`) and sits within
//! [`CAPTION_MAX_GAP_PT`] of it, overlapping it sideways; that block becomes
//! a `caption` block. A numbered figure caption ("Figure 3") that no picture
//! took is a figure lmgw did not find, a vector drawing or chart: the
//! page is named in a `pictures_partial` note. Where the layout model (WP8)
//! read the page, its picture regions are `figure` blocks already, vector
//! drawings included (D22), and a caption it marked beside one is taken as
//! the figure's before any paragraph is (`order::caption_of`). A drawing
//! without such a caption on a page the model did not read is not noted
//! yet: no poppler tool lists vector paths without rendering the page.

use std::collections::{HashMap, HashSet};

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};
use quick_xml::events::Event;
use quick_xml::Reader;
use sha2::{Digest, Sha256};

use super::super::doc::{Block, BlockKind, Doc, Figure, PartKind, Rect, Source};
use super::super::pdf_layout::finite;
use super::{
    caption, pages_said, warning, Found, LeftOut, Picture, Settings, HEADER_MIN_PAGES,
    HEADER_MIN_SHARE, PAGE_COVER_SHARE, PICTURES_PER_PAGE_MAX,
};

pub mod gate;
mod order;
pub mod runs;

/// The most a caption may sit from its picture, in points (half an inch:
/// LibreOffice and LaTeX leave a line or two between them).
pub const CAPTION_MAX_GAP_PT: f32 = 36.0;

/// One image drawn on a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImgBox {
    pub rect: Rect,
    /// The image data's hash (its first 8 bytes of SHA-256).
    pub hash: u64,
}

/// One page of a run, with its images in drawing order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageImages {
    pub number: u32,
    /// As displayed (rotation applied), in points.
    pub width: f32,
    pub height: f32,
    pub boxes: Vec<ImgBox>,
}

/// A picture run that gave its pages' pictures in part or not at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Gone {
    /// Its pages (`None`: the file's).
    pub range: Option<(u32, u32)>,
    /// poppler's reason, or where its output stopped being readable.
    pub why: String,
    /// The pages before that point were read.
    pub partial: bool,
}

/// The images of a file's picked pages and what was said of them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Images {
    pub pages: Vec<PageImages>,
    /// Where one run's output stopped being readable ([`images`]).
    pub error: Option<String>,
    /// The runs that failed or stopped.
    pub gone: Vec<Gone>,
    /// What the gate left out without a run.
    pub left: LeftOut,
    pub left_pages: Vec<u32>,
    /// `pdftohtml` is not there: no picture was looked for.
    pub missing: bool,
}

fn hash(data: &[u8]) -> u64 {
    let d = Sha256::digest(data);
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    u64::from_le_bytes(b)
}

/// The span of `at` and `at + len`, whichever way the length points.
fn span(at: f32, len: f32) -> (f32, f32) {
    let end = at + len;
    if end.is_finite() {
        (at.min(end), at.max(end))
    } else {
        (at, at)
    }
}

/// The images of `xml` (`pdftohtml -xml -dataurls`), page by page. Linear in
/// the output; a value that is no number is 0, as the layout reads them.
pub fn images(xml: &[u8]) -> Images {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = false;
    let mut out = Images::default();
    loop {
        let ev = match reader.read_event() {
            Ok(ev) => ev,
            Err(e) => {
                let at = reader.buffer_position();
                out.error = Some(format!("stops being readable XML at byte {at} ({e})"));
                return out;
            }
        };
        match ev {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"page" => {
                    let mut p = PageImages {
                        number: out.pages.last().map_or(1, |l| l.number.saturating_add(1)),
                        ..Default::default()
                    };
                    for a in e.attributes().flatten() {
                        let v = String::from_utf8_lossy(&a.value);
                        match a.key.as_ref() {
                            b"number" => {
                                if let Ok(n) = v.trim().parse() {
                                    p.number = n;
                                }
                            }
                            b"width" => p.width = finite(&v).max(0.0),
                            b"height" => p.height = finite(&v).max(0.0),
                            _ => {}
                        }
                    }
                    out.pages.push(p);
                }
                b"image" => {
                    let (mut top, mut left, mut w, mut h, mut hsh) = (0.0, 0.0, 0.0, 0.0, 0);
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"src" => hsh = hash(&a.value),
                            k => {
                                let v = finite(&String::from_utf8_lossy(&a.value));
                                match k {
                                    b"top" => top = v,
                                    b"left" => left = v,
                                    b"width" => w = v,
                                    b"height" => h = v,
                                    _ => {}
                                }
                            }
                        }
                    }
                    if let Some(p) = out.pages.last_mut() {
                        let ((x0, x1), (y0, y1)) = (span(left, w), span(top, h));
                        p.boxes.push(ImgBox {
                            rect: Rect { x0, y0, x1, y1 },
                            hash: hsh,
                        });
                    }
                }
                _ => {}
            },
            Event::Eof => return out,
            _ => {}
        }
    }
}

/// An image's identity across pages: its data and its place, to the point.
type Key = (u64, i64, i64, i64, i64);

fn key(b: &ImgBox) -> Key {
    let r = |v: f32| v.round() as i64;
    (
        b.hash,
        r(b.rect.x0),
        r(b.rect.y0),
        r(b.rect.x1),
        r(b.rect.y1),
    )
}

/// The images repeated over `pages` pages: the same data in the same place
/// on at least [`HEADER_MIN_SHARE`] of them (and [`HEADER_MIN_PAGES`]),
/// anywhere on the page: a logo in the running header, a watermark, a
/// background drawn on every page.
fn repeated(images: &Images, pages: usize) -> HashSet<Key> {
    let need = ((HEADER_MIN_SHARE * pages as f32).ceil() as usize).max(HEADER_MIN_PAGES as usize);
    if images.pages.len() < need {
        return HashSet::new();
    }
    let mut seen: HashMap<Key, (usize, usize)> = HashMap::new();
    for (i, p) in images.pages.iter().enumerate() {
        for b in &p.boxes {
            let e = seen.entry(key(b)).or_insert((usize::MAX, 0));
            if e.0 != i {
                *e = (i, e.1 + 1);
            }
        }
    }
    seen.into_iter()
        .filter(|(_, (_, n))| *n >= need)
        .map(|(k, _)| k)
        .collect()
}

fn figure_block(r: Rect) -> Block {
    let mut b = Block::new(BlockKind::Figure, String::new(), Some(r), Source::TextLayer);
    b.figure = Some(Box::new(Figure {
        index: 0,
        caption: None,
        description: None,
    }));
    b
}

/// The figure blocks of `doc`'s pages from `images`, each picture of
/// [`Found`] (in reading order, without bytes yet), and the notes.
pub fn place(doc: &mut Doc, images: &Images, s: &Settings) -> Found {
    let mut found = Found::default();
    let pages = doc
        .parts
        .iter()
        .filter(|p| p.kind == PartKind::Page)
        .count();
    let repeats = repeated(images, pages);
    let by_number: HashMap<u32, &PageImages> = images.pages.iter().map(|p| (p.number, p)).collect();
    let mut left = images.left;
    let mut left_pages = images.left_pages.clone();
    let (mut over_pages, mut vector_pages) = (Vec::new(), Vec::new());
    for part in doc.parts.iter_mut().filter(|p| p.kind == PartKind::Page) {
        let pi = by_number.get(&part.number);
        // The page as displayed, the space the boxes are in: the layout's
        // record of it (pdftohtml's page from the font run, which the
        // layout model's regions are placed by too); the picture run's own
        // only for a part without one.
        let (w, h) = part
            .size
            .or_else(|| {
                pi.filter(|p| p.width > 0.0 && p.height > 0.0)
                    .map(|p| (p.width, p.height))
            })
            .unwrap_or((0.0, 0.0));
        let area = f64::from(w) * f64::from(h);
        let mut kept: Vec<Rect> = Vec::new();
        // The images left out, for another reader's figures over them.
        let mut gone: Vec<Rect> = Vec::new();
        let mut here: HashSet<Key> = HashSet::new();
        let mut left_here = false;
        for b in pi.map_or(&[][..], |p| &p.boxes[..]) {
            let r = Rect {
                x0: b.rect.x0.clamp(0.0, w),
                y0: b.rect.y0.clamp(0.0, h),
                x1: b.rect.x1.clamp(0.0, w),
                y1: b.rect.y1.clamp(0.0, h),
            };
            // Off the page: nothing a reader sees.
            if r.width() <= 0.0 || r.height() <= 0.0 || area <= 0.0 {
                continue;
            }
            // Drawn twice in one place: one picture.
            if !here.insert(key(b)) {
                continue;
            }
            let share = f64::from(r.width()) * f64::from(r.height()) / area;
            if repeats.contains(&key(b)) {
                left.repeated += 1;
            } else if share >= PAGE_COVER_SHARE {
                left.cover += 1;
            } else if share < s.min_area {
                left.small += 1;
            } else {
                kept.push(r);
                continue;
            }
            gone.push(r);
            left_here = true;
        }
        kept.sort_by(|a, b| a.y0.total_cmp(&b.y0).then(a.x0.total_cmp(&b.x0)));
        let extra = kept.split_off(kept.len().min(PICTURES_PER_PAGE_MAX));
        if !extra.is_empty() {
            over_pages.push(part.number);
        }
        // One place for merging: a figure block the page has already takes
        // the images it overlaps; one that took none is left out as an
        // image of its size or over a left-out image would be.
        let took = order::merge(&mut part.blocks, &mut kept);
        let small = left.small;
        order::leave_out(
            &mut part.blocks,
            &took,
            &gone,
            area,
            s.min_area,
            &mut left.small,
        );
        if left_here || left.small != small {
            left_pages.push(part.number);
        }
        let end = part
            .blocks
            .iter()
            .position(|b| b.kind == BlockKind::PageFooter)
            .unwrap_or(part.blocks.len());
        // Each figure's place among the page's blocks as the layout left
        // them; figures with one place keep their own order. Past the bound,
        // at the page's end.
        let mut at: Vec<(usize, Rect, bool)> = kept
            .into_iter()
            .map(|r| (order::slot(&part.blocks, end, &r), r, false))
            .collect();
        at.extend(extra.into_iter().map(|r| (end, r, true)));
        at.sort_by_key(|(i, _, _)| *i);
        let old = std::mem::take(&mut part.blocks);
        let mut figs = at.into_iter().peekable();
        let mut blocks = Vec::with_capacity(old.len() + figs.len());
        // Per block: a figure past the bound.
        let mut over: Vec<bool> = Vec::with_capacity(blocks.capacity());
        for (i, b) in old.into_iter().enumerate() {
            while let Some((_, r, x)) = figs.next_if(|(j, _, _)| *j == i) {
                blocks.push(figure_block(r));
                over.push(x);
            }
            blocks.push(b);
            over.push(false);
        }
        for (_, r, x) in figs {
            blocks.push(figure_block(r));
            over.push(x);
        }
        part.blocks = blocks;
        // Captions, the paragraph before, and the pictures in order: one
        // pass, the neighbours found beforehand.
        let (prev, next) = order::neighbours(&part.blocks);
        let mut taken = vec![false; part.blocks.len()];
        let mut last_para: Option<usize> = None;
        for k in 0..part.blocks.len() {
            match part.blocks[k].kind {
                BlockKind::Paragraph => {
                    last_para = Some(k);
                    continue;
                }
                BlockKind::Figure => {}
                _ => continue,
            }
            let index = u32::try_from(found.pictures.len()).unwrap_or(u32::MAX);
            // The figure's reader: poppler's, or the layout model's region.
            let mut p = Picture::new(part.number, part.blocks[k].bbox, part.blocks[k].source);
            p.share = p
                .bbox
                .filter(|_| area > 0.0)
                .map(|r| f64::from(r.width()) * f64::from(r.height()) / area);
            let mut caption = None;
            if over[k] {
                p.missing = Some("PICTURES_PER_PAGE_MAX");
            } else {
                let cap = order::caption_of(&part.blocks, k, prev[k], next[k], &taken);
                caption = cap.map(|i| part.blocks[i].text.clone());
                if let Some(i) = cap {
                    part.blocks[i].kind = BlockKind::Caption;
                    taken[i] = true;
                }
                p.before = last_para
                    .filter(|i| Some(*i) != cap)
                    .map(|i| part.blocks[i].text.clone());
            }
            p.caption = caption.clone();
            found.pictures.push(p);
            part.blocks[k].figure = Some(Box::new(Figure {
                index,
                caption,
                description: None,
            }));
        }
        // A numbered figure caption no figure took, read as a paragraph. One
        // the layout model marked is on a page it read, where drawings are
        // figures already; it may just sit too far from its figure.
        if part
            .blocks
            .iter()
            .any(|b| b.kind == BlockKind::Paragraph && caption::is_numbered_figure(&b.text))
        {
            vector_pages.push(part.number);
        }
    }
    left_pages.sort_unstable();
    left_pages.dedup();
    found.warnings = notes(s, left, left_pages, over_pages, vector_pages, images);
    found
}

/// The pages a run's note names.
fn range_said(range: Option<(u32, u32)>) -> (String, Vec<u32>) {
    match range {
        None => ("this file".to_string(), Vec::new()),
        Some((f, l)) => {
            let pages: Vec<u32> = (f..=l).collect();
            (pages_said(&pages), pages)
        }
    }
}

/// The file's picture notes.
fn notes(
    s: &Settings,
    left: LeftOut,
    left_pages: Vec<u32>,
    over_pages: Vec<u32>,
    vector_pages: Vec<u32>,
    images: &Images,
) -> Vec<ConvertWarning> {
    let mut out = Vec::new();
    let poppler = Some("poppler");
    if images.missing {
        out.push(warning(
            WarningCode::ToolMissing,
            "pdftohtml (part of poppler-utils) was not found, so no pictures were looked for"
                .to_string(),
            Vec::new(),
            poppler,
        ));
    }
    for g in &images.gone {
        let (said, parts) = range_said(g.range);
        let message = if g.partial {
            format!(
                "pdftohtml's picture list for {said} {}; pictures after that point were not \
                 found (the text was read in full)",
                g.why
            )
        } else {
            format!(
                "pdftohtml could not list the pictures of {said} ({}), so they have none; the \
                 text was read in full",
                g.why
            )
        };
        out.push(warning(
            WarningCode::PicturesPartial,
            message,
            parts,
            poppler,
        ));
    }
    if let Some(w) = left.warning(s, left_pages) {
        out.push(w);
    }
    if !over_pages.is_empty() {
        out.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "more than PICTURES_PER_PAGE_MAX ({PICTURES_PER_PAGE_MAX}) pictures on {}; \
                 the ones past it are placeholders at the end of their page, without their \
                 picture or caption",
                pages_said(&over_pages)
            ),
            over_pages,
            None,
        ));
    }
    if !vector_pages.is_empty() {
        out.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "a figure caption with no picture found beside it on {}: probably a drawing \
                 or chart made of lines and shapes, which poppler does not list; the layout \
                 model (Settings › Documents › Layout model) finds those on the pages it reads; \
                 the figure's text, if any, is in the text",
                pages_said(&vector_pages)
            ),
            vector_pages,
            None,
        ));
    }
    out
}

#[cfg(test)]
mod tests;
