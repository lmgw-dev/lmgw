//! A page's image for the model, bounded (design §9; review finding S13).
//!
//! A page is rendered at [`PAGE_DPI`] (`pdftoppm -r`), the resolution the
//! page prompts were measured at. A page's size is whatever its media box
//! says, so that alone bounds nothing: an A0 sheet, or a page declared
//! 10,000 pt wide, would be hundreds of MiB of pixels, base64'd and
//! serialised again for the count and the send, times the calls in flight,
//! for a request the model refuses anyway. So above [`PAGE_MAX_PIXELS`] a
//! page is rendered scaled down to it (`pdftoppm -scale-to`), and the
//! page's `page_scaled` note says so with both numbers. lmgw has no
//! per-model limit on an image's pixels yet (a projector's bound is in
//! tokens, and a cloud model's catalog has none), so the limit is this one
//! named constant for every model.

use std::path::Path;

use crate::extract::convert::ocr::spool::Spool;
use crate::extract::pdf::{self, PdfError, PAGE_DPI};

/// The most pixels a page image is rendered with: 4.4 million.
///
/// An A3 sheet at [`PAGE_DPI`] is 1754 × 2480 = 4.35 million pixels, so
/// every page up to A3 is rendered at the measured resolution (an A4 page:
/// 2.2 million). More pixels do not read better: a model scales an image
/// down to its own input size, and Gemma 4's image tokens stop growing
/// between an A4 page at 150 dpi (1053 tokens) and at 300 dpi (1094;
/// `modelinfo::GEMMA4_IMAGE_UBATCH`'s measurement, 2026-09-24). What more
/// pixels do cost is the render's memory and the request's size, once per
/// call in flight.
pub const PAGE_MAX_PIXELS: u64 = 4_400_000;

/// How a page is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scale {
    /// At [`PAGE_DPI`]: it is within [`PAGE_MAX_PIXELS`].
    Dpi,
    /// Scaled so its long side has `long_side` pixels. `pixels` is what it
    /// would have had at [`PAGE_DPI`]; `None` for a page whose size is not
    /// known, which is rendered to fit the limit.
    To { long_side: u32, pixels: Option<u64> },
}

/// How a page of `size` (width and height in points) is rendered.
pub(crate) fn scale(size: Option<(f32, f32)>) -> Scale {
    let limit = PAGE_MAX_PIXELS as f64;
    let known = size.filter(|(w, h)| w.is_finite() && h.is_finite() && *w > 0.0 && *h > 0.0);
    let Some((w, h)) = known else {
        // A square of the limit's area holds a page of any shape.
        return Scale::To {
            long_side: limit.sqrt() as u32,
            pixels: None,
        };
    };
    let px = |points: f32| f64::from(points) / 72.0 * f64::from(PAGE_DPI);
    let (w, h) = (px(w), px(h));
    let pixels = w * h;
    if pixels <= limit {
        return Scale::Dpi;
    }
    let mut long_side = (w.max(h) * (limit / pixels).sqrt()).floor().max(1.0);
    // A sliver's short side cannot go below one pixel, so there the long
    // side alone is the pixel count.
    if long_side * (w.min(h) / w.max(h)) < 1.0 {
        long_side = long_side.min(limit);
    }
    Scale::To {
        long_side: long_side.min(f64::from(u32::MAX)) as u32,
        pixels: Some(pixels.min(u64::MAX as f64) as u64),
    }
}

impl Scale {
    /// The page's note, when it was not rendered at [`PAGE_DPI`].
    pub(crate) fn said(self) -> Option<String> {
        let Self::To { long_side, pixels } = self else {
            return None;
        };
        Some(match pixels {
            Some(pixels) => format!(
                "would be {pixels} pixels at {PAGE_DPI} dpi, more than PAGE_MAX_PIXELS \
                 ({PAGE_MAX_PIXELS}), so the page-reading model was sent it scaled down to \
                 {long_side} pixels on its long side; small print may read worse than on a \
                 page of ordinary size"
            ),
            None => format!(
                "has no size lmgw could read, so the page-reading model was sent it scaled to \
                 {long_side} pixels on its long side, which keeps it within PAGE_MAX_PIXELS \
                 ({PAGE_MAX_PIXELS})"
            ),
        })
    }
}

/// Page `page` of the PDF `spool` holds as a PNG, rendered as `scale` says,
/// in a slot of the tool pool taken for the render alone (waiting for it
/// stops the file's clock; the slot is leased until `pdftoppm` is reaped).
/// The file is the spool text recognition used too: written once for the
/// file, read by position, never piped whole once a page.
pub(crate) async fn page(
    bin: &Path,
    spool: &Spool,
    page: u32,
    scale: Scale,
) -> Result<Vec<u8>, PdfError> {
    let (flag, size) = match scale {
        Scale::Dpi => ("-r", PAGE_DPI.to_string()),
        Scale::To { long_side, .. } => ("-scale-to", long_side.max(1).to_string()),
    };
    let n = page.to_string();
    let args = [flag, &size, "-png", "-singlefile", "-f", &n, "-l", &n].map(String::from);
    let mut call = pdf::wide::Call::new(
        pdf::PopplerTool::Render,
        bin,
        args.to_vec(),
        &[],
        pdf::wide::Src::Spool(spool),
    );
    call.span = Some((page, page));
    // A slot of its own charged the image limit and the PNG, once more
    // under the wide limit on a new one where it needs it (`pdf::wide`).
    let png = pdf::wide::own(&call, pdf::PDF_TIMEOUT).await?.out;
    if png.is_empty() {
        return Err(PdfError::EmptyPage(page));
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_up_to_a3_keep_the_measured_resolution_and_larger_ones_are_scaled() {
        // A4 and A3, in points.
        assert_eq!(scale(Some((595.0, 842.0))), Scale::Dpi);
        assert_eq!(scale(Some((842.0, 1191.0))), Scale::Dpi);
        // A0: 2384 × 3370 pt is 4967 × 7021 pixels at 150 dpi.
        let Scale::To { long_side, pixels } = scale(Some((2384.0, 3370.0))) else {
            panic!("an A0 page was rendered whole");
        };
        let pixels = pixels.unwrap();
        assert!((34_800_000..34_900_000).contains(&pixels), "{pixels}");
        // The scaled page is within the limit, and close to it.
        let short = f64::from(long_side) * 2384.0 / 3370.0;
        let scaled = f64::from(long_side) * short;
        assert!(scaled <= PAGE_MAX_PIXELS as f64, "{scaled}");
        assert!(scaled > PAGE_MAX_PIXELS as f64 * 0.99, "{scaled}");
        let said = scale(Some((2384.0, 3370.0))).said().unwrap();
        assert!(said.contains("PAGE_MAX_PIXELS (4400000)"), "{said}");
        assert!(said.contains(&format!("{long_side} pixels")), "{said}");
        assert_eq!(Scale::Dpi.said(), None);
    }

    #[test]
    fn a_hostile_or_unknown_size_is_bounded_without_overflow() {
        for size in [
            Some((10_000.0, 10_000.0)),
            Some((f32::MAX, f32::MAX)),
            Some((1.0e9, 1.0)),
        ] {
            let Scale::To { long_side, .. } = scale(size) else {
                panic!("{size:?} was rendered whole");
            };
            assert!(
                (1..=PAGE_MAX_PIXELS as u32).contains(&long_side),
                "{size:?}"
            );
        }
        // A sliver so long that scaling leaves it one pixel high: its long
        // side is the whole pixel count, and stays within the limit.
        assert_eq!(
            scale(Some((1.0e9, 1.0))),
            Scale::To {
                long_side: PAGE_MAX_PIXELS as u32,
                pixels: Some(4_340_277_777),
            }
        );
        // A sliver is few pixels whatever its length.
        assert_eq!(scale(Some((100_000.0, 1.0))), Scale::Dpi);
        for size in [
            None,
            Some((0.0, 842.0)),
            Some((f32::NAN, 1.0)),
            Some((-5.0, 5.0)),
        ] {
            let s = scale(size);
            assert_eq!(
                s,
                Scale::To {
                    long_side: 2097,
                    pixels: None
                },
                "{size:?}"
            );
            assert!(s.said().unwrap().contains("no size lmgw could read"));
        }
    }
}
