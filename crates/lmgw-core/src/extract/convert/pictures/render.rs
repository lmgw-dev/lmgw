//! A PDF's pictures cropped out of their rendered pages (design §5.3): one
//! `pdftoppm` run per page with pictures (reading the file's spool),
//! rendering only the smallest box that holds them all (`-x -y -W -H`) at
//! `72 × images_scale` dpi as a binary PPM (no PNG to encode and decode:
//! §14 performance round), then each picture cut out of that raster and
//! written as a PNG (what docling writes)
//! by lmgw's reader (`lmgw-doc-reader crop`, [`super::super::reader`]) in
//! the same slot of the tool pool the render had, so the page's render is only
//! held while its slot is. poppler draws each picture as the page shows it
//! (masks, rotation, JBIG2, JPEG 2000, CMYK) in its own child under its
//! memory limit; the raster it wrote is read in the reader child, never in
//! the gateway (poppler drew it from an untrusted PDF), after the size it
//! declares is checked against the box asked for.
//!
//! **Why a render, not the embedded image:** the design asks for what the
//! reader sees, and an image's own pixels can be masked, rotated, clipped or
//! far larger than the page shows them; one crop size (the scale) for every
//! picture is also what docling does (`images_scale`).
//!
//! **Bounds**, each named in the page's warning:
//! - the crop box's raster (`width × height × 3`) at most
//!   [`PAGE_RASTER_MAX_BYTES`] before poppler is run;
//! - poppler's run: [`PDF_TIMEOUT`], `POPPLER_IMAGE_MEMORY_LIMIT_BYTES`
//!   (once more under `POPPLER_WIDE_MEMORY_LIMIT_BYTES` in the same slot
//!   where it needs it, `pdf::wide`), `PAGE_RASTER_MAX_BYTES` and its
//!   header of PPM;
//! - the reader's run: `READER_TIMEOUT`, `READER_MEMORY_LIMIT_BYTES`, and
//!   the PNGs it writes bounded by what the request's output has left
//!   (crops past it are left out) plus [`CROP_ANSWER_BYTES`];
//! - each picture's PNG is reserved from the request's output budget at the
//!   size the answer writes it in ([`super::reserve`]), or at its own size
//!   when it is made only for the picture-description model
//!   ([`super::held`]); a picture that does
//!   not fit is dropped before it is kept and stays a placeholder, so a
//!   request's pictures are bounded with its text and never fail its file.
//!
//! A page whose run fails keeps its figures as placeholders, and the
//! file's `pictures_partial` warning names the pages and poppler's reason.
//! A dropped request drops the runs (their children are killed).
//!
//! The pages are cropped side by side, each in a slot of its own, and taken
//! in page order, so which crops fit the request's output does not depend
//! on which page finished first ([`side`]).

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use lmgw_api_types::documents::convert::WarningCode;

use super::super::budget::OutputBudget;
use super::super::doc::Rect;
use super::super::ocr::spool::Spool;
use super::{pages_said, warning, Found, Image, Settings};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::pdf::wide::Within;
use crate::extract::pdf::PdfError;
use crate::extract::pdf::PDF_TIMEOUT;

/// The most one page's crop raster may take (RGB, 8 bits a channel), checked
/// before poppler renders it: `MAX_UNCOMPRESSED_BYTES`, the bound every
/// converted output has. The raster is decoded in lmgw's reader, under its
/// own memory limit; the gateway holds the page's PNG and the crops the
/// reader answers, each at most `MAX_UNCOMPRESSED_BYTES`, which take the
/// place of the page's geometry outputs (gone by the time pictures are
/// cropped) in the pool's per-run memory (`pool::charge::PAGE_CROPS`). An A4
/// page whole at the default scale is about 6 MiB; a raster past this is a
/// page declared enormous or a scale asked far past what a reader needs:
/// lower `images_scale`.
pub const PAGE_RASTER_MAX_BYTES: u64 = MAX_UNCOMPRESSED_BYTES;

/// A binary PPM's header (`P6`, the size and the largest value): far
/// below 64 bytes.
const PPM_HEADER_BYTES: u64 = 64;

/// What the reader's answer to a page's crops may take besides the PNGs:
/// its header, at most `PICTURES_PER_PAGE_MAX` (256) entries of three
/// numbers, and the frame's own lines; 1 MiB is far past that.
pub const CROP_ANSWER_BYTES: u64 = 1 << 20;

fn px(v: f32, scale: f64) -> u64 {
    let p = (f64::from(v) * scale).floor();
    if p.is_finite() && p > 0.0 {
        p as u64
    } else {
        0
    }
}

fn px_up(v: f32, scale: f64) -> u64 {
    let p = (f64::from(v) * scale).ceil();
    if p.is_finite() && p > 0.0 {
        p as u64
    } else {
        0
    }
}

/// `r` in pixels at `scale` (pixels per point), as `x, y, w, h`.
fn to_px(r: &Rect, scale: f64) -> (u64, u64, u64, u64) {
    let (x, y) = (px(r.x0, scale), px(r.y0, scale));
    let (x1, y1) = (px_up(r.x1, scale), px_up(r.y1, scale));
    (
        x,
        y,
        x1.saturating_sub(x).max(1),
        y1.saturating_sub(y).max(1),
    )
}

/// One crop: its PNG, width, height and what it reserved.
type Crop = (Bytes, u32, u32, u64);

mod side;

/// What one page's crops gave.
enum PageOut {
    /// Per picture: its PNG, width, height and what it reserved; `None`
    /// past the budget.
    Crops(Vec<Option<Crop>>),
    /// The page's raster, or its PNG, could not be used: why.
    Failed(String),
}

/// Fill the bytes of `found`'s pictures from `pdf` (its spool) with the `pdftoppm` at
/// `bin`, cut by lmgw's reader at `cutter` (`None`: not found, each page's
/// pictures are named without their bytes), at `s.scale`
/// (`images_scale`), each held of `budget` as `s` says ([`super::held`]).
pub async fn pdf(
    pdf: &Spool,
    found: &mut Found,
    s: &Settings,
    budget: &Arc<OutputBudget>,
    bin: &Path,
    cutter: Option<&Path>,
) {
    let scale = s.scale;
    let per_pt = scale; // 72 × scale dpi: `scale` pixels a point.
    let dpi = format!("{}", 72.0 * scale);
    // The pictures to crop, page by page (reading order keeps a page's
    // pictures together; pages ascend).
    let mut pages: Vec<(u32, Vec<usize>)> = Vec::new();
    for (i, p) in found.pictures.iter().enumerate() {
        if p.missing.is_some() || p.bbox.is_none() {
            continue;
        }
        match pages.iter_mut().find(|(n, _)| *n == p.part) {
            Some((_, v)) => v.push(i),
            None => pages.push((p.part, vec![i])),
        }
    }
    let (mut raster_pages, mut failed_pages, mut budget_pages) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut failed_why: Option<String> = None;
    let mut jobs = Vec::new();
    for (page, idx) in pages {
        let rects: Vec<Rect> = idx.iter().filter_map(|i| found.pictures[*i].bbox).collect();
        let Some(union) = Rect::union_all(&rects) else {
            continue;
        };
        let (ux, uy, uw, uh) = to_px(&union, per_pt);
        let raster = uw.saturating_mul(uh).saturating_mul(3);
        if raster > PAGE_RASTER_MAX_BYTES || uw > u64::from(u32::MAX) || uh > u64::from(u32::MAX) {
            raster_pages.push(page);
            for i in &idx {
                found.pictures[*i].missing = Some("PAGE_RASTER_MAX_BYTES");
            }
            continue;
        }
        let captions: Vec<usize> = idx
            .iter()
            .map(|i| found.pictures[*i].caption.as_ref().map_or(0, String::len))
            .collect();
        let boxes: Vec<[u32; 4]> = rects
            .iter()
            .map(|r| {
                let (x, y, w, h) = to_px(r, per_pt);
                [
                    (x.saturating_sub(ux)).min(uw - 1) as u32,
                    (y.saturating_sub(uy)).min(uh - 1) as u32,
                    w.min(uw) as u32,
                    h.min(uh) as u32,
                ]
            })
            .collect();
        let n = page.to_string();
        let args = [
            "-r",
            &dpi,
            "-x",
            &ux.to_string(),
            "-y",
            &uy.to_string(),
            "-W",
            &uw.to_string(),
            "-H",
            &uh.to_string(),
            "-singlefile",
            "-f",
            &n,
            "-l",
            &n,
        ]
        .map(str::to_string)
        .to_vec();
        jobs.push(side::Job {
            page,
            idx,
            args,
            size: (uw as u32, uh as u32),
            boxes,
            captions,
        });
    }
    // A binary PPM: the raster checked above and its header.
    let bounds = Within {
        timeout: PDF_TIMEOUT,
        output: PAGE_RASTER_MAX_BYTES + PPM_HEADER_BYTES,
    };
    let outs = match cutter {
        None => side::no_cutter(jobs),
        Some(cutter) => side::crop_all(jobs, pdf, bin, cutter, s, budget, bounds).await,
    };
    for (job, out) in outs {
        let (page, idx) = (job.page, job.idx);
        match out {
            PageOut::Crops(c) => {
                let mut over = false;
                for (i, got) in idx.iter().zip(c) {
                    let p = &mut found.pictures[*i];
                    match got {
                        Some((png, w, h, held)) => {
                            found.charged = found.charged.saturating_add(held);
                            p.image = Some(Image {
                                bytes: png,
                                mime: "image/png",
                                ext: "png",
                                width_px: Some(w),
                                height_px: Some(h),
                            });
                        }
                        None => {
                            p.missing = Some("MAX_UNCOMPRESSED_BYTES");
                            over = true;
                        }
                    }
                }
                if over {
                    budget_pages.push(page);
                }
            }
            PageOut::Failed(why) => {
                for i in &idx {
                    found.pictures[*i].missing = Some("render");
                }
                failed_pages.push(page);
                failed_why.get_or_insert(why);
            }
        }
    }
    let poppler = Some("poppler");
    if !raster_pages.is_empty() {
        found.warnings.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "the pictures of {} would render to more than PAGE_RASTER_MAX_BYTES \
                 ({}) at images_scale {scale}, so they are placeholders without their \
                 picture; a smaller images_scale crops them",
                pages_said(&raster_pages),
                crate::hf::fmt_bytes(PAGE_RASTER_MAX_BYTES)
            ),
            raster_pages,
            None,
        ));
    }
    if !failed_pages.is_empty() {
        found.warnings.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "pdftoppm could not render the pictures of {} ({}), so they are \
                 placeholders without their picture",
                pages_said(&failed_pages),
                failed_why.unwrap_or_default()
            ),
            failed_pages,
            poppler,
        ));
    }
    if !budget_pages.is_empty() {
        found.warnings.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "pictures of {} were not exported: the request's output passed \
                 MAX_UNCOMPRESSED_BYTES ({}), what all its files may produce together; {}",
                pages_said(&budget_pages),
                crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES),
                super::over_budget_advice(s)
            ),
            budget_pages,
            None,
        ));
    }
}

/// A run's end in a page warning's words.
pub(super) fn said(e: &PdfError) -> String {
    match e {
        PdfError::ToolMissing => "pdftoppm (part of poppler-utils) is not installed".to_string(),
        PdfError::Timeout { .. } => format!(
            "it did not finish within {} s (PDF_TIMEOUT)",
            PDF_TIMEOUT.as_secs()
        ),
        PdfError::OutputTooLarge { .. } => format!(
            "its image passed MAX_UNCOMPRESSED_BYTES ({})",
            crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
        ),
        PdfError::Failed { message, .. } => message.clone(),
        e => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_become_whole_pixels_at_the_scale() {
        let r = Rect {
            x0: 10.2,
            y0: 20.0,
            x1: 30.0,
            y1: 20.1,
        };
        assert_eq!(to_px(&r, 2.0), (20, 40, 40, 1));
        assert_eq!(to_px(&r, f64::INFINITY).0, 0);
    }
}
