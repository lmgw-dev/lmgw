//! Which pages of a PDF get a picture run (design §5.3): `pdfimages -list`
//! names every image drawn, page by page, with its pixel size and its
//! resolution, and `pdfinfo` each page's size. Both are as cheap as the font
//! run (they decode no image), where the picture run (`pdftohtml
//! -dataurls`) encodes every image it meets as PNG: about 1 s per page of a
//! 600 dpi scan, measured with poppler 26.08 on 2026-11-19. So only a page
//! with an image that may be exported gets a picture run.
//!
//! An image's drawn size in points is its pixels × 72 over its ppi. A page
//! is skipped when every image on it is clearly a page cover (drawn at the
//! page's size, within [`COVER_TOLERANCE`] on both sides) or clearly below
//! `picture_min_area` (drawn at less than the area over [`SMALL_MARGIN`]:
//! the box pdftohtml gives can only be as large as the drawn size, or up to
//! twice its area for an image drawn turned), and those images are counted
//! as left out from the list. Every unsure row runs its page:
//! - a resolution below [`PPI_MIN`] (pdfimages prints those with little
//!   precision: 4 px over 200 pt prints `1`);
//! - an inline image (pdfimages prints `[inline]` where the object number
//!   and generation are, one column for two);
//! - a type other than `image`/`stencil` (`mask`/`smask` rows are an
//!   image's masks, drawn with it, and not counted);
//! - a page whose size `pdfinfo` did not give, a line that does not parse.
//!
//! When either tool fails or a line names no page, the gate is unsure as a
//! whole and every page of the span gets its run ([`Gated::Unsure`]). The
//! gate judges by size only: pdfimages gives no position, so a page-sized
//! image drawn mostly off its page counts as a cover.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::super::super::ocr::spool::{file_arg, Spool};
use super::super::super::pages::PageSet;
use super::super::super::pool;
use super::super::super::run::{self, Bounds};
use super::super::LeftOut;
use crate::extract::pdf::{self, PopplerTool, PDF_TIMEOUT};

/// The poppler tool that lists a PDF's images.
pub const PDFIMAGES: &str = "pdfimages";
/// The poppler tool that gives a PDF's page sizes.
pub const PDFINFO: &str = "pdfinfo";

/// Below this resolution (pixels per inch) a row's drawn size is unsure.
pub const PPI_MIN: f64 = 10.0;
/// How far an image's drawn sides may be from its page's and still be the
/// page's cover (5%: a scan with a margin of bleed, rounding).
pub const COVER_TOLERANCE: f64 = 0.05;
/// An image is clearly small when its drawn area times this is below
/// `picture_min_area` of its page.
pub const SMALL_MARGIN: f64 = 2.0;

/// What the gate decided for the span.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gate {
    /// The picked pages that get a picture run, ascending.
    pub pages: Vec<u32>,
    /// The images of the skipped pages, counted by kind.
    pub left: LeftOut,
    /// The skipped pages that had images.
    pub left_pages: Vec<u32>,
}

/// The gate's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Gated {
    Pages(Gate),
    /// Nothing could be judged: the whole span gets its run.
    Unsure,
}

/// One row of `pdfimages -list`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Row {
    /// An image drawn on `page`, `w × h` pixels at `ppi` (x, y).
    Image {
        page: u32,
        w: f64,
        h: f64,
        ppi: (f64, f64),
    },
    /// A row of a page that says nothing for sure.
    Unsure(u32),
    /// A mask drawn with its image.
    Mask,
}

/// The rows of `pdfimages -list`; `None` when a line names no page.
fn rows(list: &str) -> Option<Vec<Row>> {
    let mut out = Vec::new();
    let mut body = false;
    for line in list.lines() {
        if !body {
            body = line.starts_with("---");
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.is_empty() {
            continue;
        }
        let page: u32 = t[0].parse().ok()?;
        // 16 columns, 15 for an inline image.
        if t.len() < 15 {
            out.push(Row::Unsure(page));
            continue;
        }
        match t[2] {
            "mask" | "smask" => {
                out.push(Row::Mask);
                continue;
            }
            "image" | "stencil" => {}
            _ => {
                out.push(Row::Unsure(page));
                continue;
            }
        }
        if t.contains(&"[inline]") {
            out.push(Row::Unsure(page));
            continue;
        }
        let num = |s: &str| s.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
        let n = t.len();
        match (num(t[3]), num(t[4]), num(t[n - 4]), num(t[n - 3])) {
            (Some(w), Some(h), Some(x), Some(y)) if x >= PPI_MIN && y >= PPI_MIN => {
                out.push(Row::Image {
                    page,
                    w,
                    h,
                    ppi: (x, y),
                })
            }
            _ => out.push(Row::Unsure(page)),
        }
    }
    Some(out)
}

/// The page count and each page's size in points from `pdfinfo -f -l`.
fn sizes(info: &str) -> (Option<u32>, HashMap<u32, (f64, f64)>) {
    let mut total = None;
    let mut out = HashMap::new();
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            total = rest.trim().parse().ok();
            continue;
        }
        let Some(rest) = line.strip_prefix("Page") else {
            continue;
        };
        let mut t = rest.split_whitespace();
        let (Some(n), Some("size:"), Some(w), Some("x"), Some(h)) =
            (t.next(), t.next(), t.next(), t.next(), t.next())
        else {
            continue;
        };
        if let (Ok(n), Ok(w), Ok(h)) = (n.parse(), w.parse::<f64>(), h.parse::<f64>()) {
            if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
                out.insert(n, (w, h));
            }
        }
    }
    (total, out)
}

fn minmax(a: f64, b: f64) -> (f64, f64) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// What one image on a page of `page` points is.
enum Kind {
    Cover,
    Small,
    Maybe,
}

fn kind(w_pt: f64, h_pt: f64, page: (f64, f64), min_area: f64) -> Kind {
    let (d0, d1) = minmax(w_pt, h_pt);
    let (p0, p1) = minmax(page.0, page.1);
    let near = |d: f64, p: f64| (d - p).abs() <= COVER_TOLERANCE * p;
    if near(d0, p0) && near(d1, p1) {
        return Kind::Cover;
    }
    if w_pt * h_pt * SMALL_MARGIN < min_area * page.0 * page.1 {
        return Kind::Small;
    }
    Kind::Maybe
}

/// The gate over `list` and `info` for the pages `picked` keeps.
fn judge(list: &str, info: &str, picked: impl Fn(u32) -> bool, min_area: f64) -> Gated {
    let Some(rows) = rows(list) else {
        return Gated::Unsure;
    };
    let (_, sizes) = sizes(info);
    // Per page: run it, or what its images were.
    #[derive(Default)]
    struct Seen {
        run: bool,
        cover: u32,
        small: u32,
    }
    let mut pages: BTreeMap<u32, Seen> = BTreeMap::new();
    for r in rows {
        let (page, what) = match r {
            Row::Mask => continue,
            Row::Unsure(p) => (p, None),
            Row::Image { page, w, h, ppi } => {
                let k = sizes
                    .get(&page)
                    .map(|s| kind(w * 72.0 / ppi.0, h * 72.0 / ppi.1, *s, min_area));
                (page, k)
            }
        };
        if !picked(page) {
            continue;
        }
        let e = pages.entry(page).or_default();
        match what {
            Some(Kind::Cover) => e.cover += 1,
            Some(Kind::Small) => e.small += 1,
            Some(Kind::Maybe) | None => e.run = true,
        }
    }
    let mut g = Gate::default();
    for (n, s) in pages {
        if s.run {
            g.pages.push(n);
        } else {
            g.left.cover += s.cover;
            g.left.small += s.small;
            g.left_pages.push(n);
        }
    }
    Gated::Pages(g)
}

/// `tool`'s bounds: `pdfinfo` reads no image, `pdfimages -list` decodes
/// some (a JPEG 2000 image whole) to describe them, and neither needs
/// their pixels (`PopplerTool::limit`). Neither is run again under the wide
/// limit: a run that fails leaves the gate unsure, which costs picture runs
/// on every page and loses nothing.
fn bounds(tool: PopplerTool) -> Bounds {
    pdf::poppler(tool.limit(), PDF_TIMEOUT)
}

/// What the gate's reading of its two outputs holds at most: their buffers
/// and their reading (`pool::charge::after_runs`), and a lossy UTF-8 copy
/// of each, up to 3 bytes for each byte that is not UTF-8.
fn judge_charge(info: &Vec<u8>, list: &Vec<u8>) -> u64 {
    let copies = (info.len() as u64 + list.len() as u64).saturating_mul(3);
    pool::charge::after_runs(&[info, list]).saturating_add(copies)
}

/// The gate for `src`'s pages in `span` that `pages` picks, with the
/// `pdfinfo` and `pdfimages` at `info` and `list`: both runs and the
/// reading in one slot of the tool pool.
pub async fn gate(
    info: &Path,
    list: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    pages: Option<PageSet>,
    min_area: f64,
) -> Gated {
    let (f, l) = span.unwrap_or((1, i32::MAX as u32));
    let (f, l) = (f.to_string(), l.to_string());
    let slot = pool::global()
        .acquire_charged(1, pool::charge::PDF_FILE + pool::charge::piped(src))
        .await;
    let input = src.input();
    let arg = file_arg(&input);
    let info_out = run::poppler_held(
        &slot,
        None,
        info,
        &["-f", &f, "-l", &l, arg],
        input,
        bounds(PopplerTool::Info),
    )
    .await;
    let input = src.input();
    let mut args = vec!["-list"];
    if span.is_some() {
        args.extend(["-f", &f, "-l", &l]);
    }
    args.push(file_arg(&input));
    let list_out = run::poppler_held(
        &slot,
        None,
        list,
        &args,
        input,
        bounds(PopplerTool::ImageList),
    )
    .await;
    let (Ok(info_out), Ok(list_out)) = (info_out, list_out) else {
        return Gated::Unsure;
    };
    // The runs have ended: the slot is fitted to the reading. Where that is
    // more than it holds and the pool has not got it free (outputs of
    // hundreds of MiB), the gate is unsure rather than read uncharged.
    let slot = match slot.fit(judge_charge(&info_out, &list_out)) {
        Ok(slot) => slot,
        Err(_) => {
            tracing::info!(
                "the picture gate's outputs ({} and {}) need more to be read than its slot \
                 holds: every page is looked at instead",
                crate::hf::fmt_bytes(info_out.len() as u64),
                crate::hf::fmt_bytes(list_out.len() as u64)
            );
            return Gated::Unsure;
        }
    };
    pool::blocking_on(slot, move || {
        let info = String::from_utf8_lossy(&info_out);
        let list = String::from_utf8_lossy(&list_out);
        judge(
            &list,
            &info,
            |n| pages.as_ref().is_none_or(|p| p.contains(n)),
            min_area,
        )
    })
    .await
    .unwrap_or(Gated::Unsure)
}

/// The picture runs for `pages` (ascending): runs of consecutive pages, each
/// at most `pages / slots` long (rounded up), so the pool's slots share
/// them; a run never spans a page the gate skipped.
pub fn ranges(pages: &[u32], slots: usize) -> Vec<(u32, u32)> {
    let per = pages.len().div_ceil(slots.max(1)).max(1);
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut len = 0usize;
    for &p in pages {
        match out.last_mut() {
            Some(r) if r.1.checked_add(1) == Some(p) && len < per => {
                r.1 = p;
                len += 1;
            }
            _ => {
                out.push((p, p));
                len = 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
