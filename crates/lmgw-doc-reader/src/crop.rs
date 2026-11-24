//! A PDF page's pictures cut out of poppler's render (`Op::Crop`, document
//! conversion design §5.3): the gateway renders the box that holds a page's
//! pictures with `pdftoppm` and hands that render here, with the box's size
//! and each picture's place in it; each picture comes back as a PNG of its
//! own. The render is read here, in the reader, never in the gateway: it is
//! poppler's output, but poppler drew it from an untrusted PDF.
//!
//! The render is a binary PPM (`P6`, 8 bits, what `pdftoppm` writes by
//! default) or a PNG (`pdftoppm -png`), told by its first bytes. Since the
//! performance round (2026-11-19) the gateway asks for PPM: measured with
//! poppler 26.08, a page at 144 dpi renders in 0.04 s as PPM and 0.21 s as
//! PNG (zlib), and here it needs no decoding. Both give the same RGB rows,
//! so the crops are the same bytes either way.
//!
//! **Bounds.** The render's declared size is checked against the box asked
//! of poppler before a pixel is read, and a PNG's decoder is limited to that
//! box's raster (four bytes a pixel at most); crops past the request's
//! budget of PNG bytes are left out. The process's own limits (memory,
//! time, output) are the gateway's.

use std::io::Cursor;

use lmgw_doc_reader::wire::{Crop, Failure};

fn unreadable(why: String) -> Failure {
    Failure::Unreadable(why)
}

/// The crops in the order of their boxes, and the PNG blobs of those made.
type Cut = (Vec<Option<Crop>>, Vec<Vec<u8>>);

/// Each of `boxes` (`[x, y, w, h]` in the render's pixels) cut out of
/// `png`, a render of at most `width × height` pixels, as a PNG, until the
/// PNGs pass `budget` bytes; the answer and its blobs in order.
pub fn crops(
    render: &[u8],
    width: u32,
    height: u32,
    budget: u64,
    boxes: &[[u32; 4]],
) -> Result<Cut, Failure> {
    if render.starts_with(b"P6") {
        let (w, h, rgb) = ppm(render, width, height)?;
        return cut(
            rgb,
            w,
            h,
            png::ColorType::Rgb,
            w as usize * 3,
            budget,
            boxes,
        );
    }
    let png = render;
    // RGBA at 8 bits, the most a render takes once 16-bit samples are
    // stripped; the decoder's own working memory on top is small.
    let limit = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(4)
        .saturating_add(1 << 20);
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut dec = png::Decoder::new_with_limits(Cursor::new(png), png::Limits { bytes: limit });
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec
        .read_info()
        .map_err(|e| unreadable(format!("its image could not be read: {e}")))?;
    let (w, h) = reader.info().size();
    if w > width || h > height || w == 0 || h == 0 {
        return Err(unreadable(format!(
            "its image is {w}×{h} pixels where at most {width}×{height} were asked for"
        )));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| unreadable("its image's size does not fit in memory here".to_string()))?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| unreadable(format!("its image could not be read: {e}")))?;
    cut(&buf, w, h, info.color_type, info.line_size, budget, boxes)
}

/// A binary PPM's size and its RGB rows; its declared size checked against
/// `width × height` before anything else.
fn ppm(data: &[u8], width: u32, height: u32) -> Result<(u32, u32, &[u8]), Failure> {
    let bad = || unreadable("its image is not a PPM poppler writes".to_string());
    // `P6`, then width, height and the largest value, each after white
    // space (no comments: poppler writes none), then one white-space byte.
    let mut at = 2usize;
    let mut field = || -> Option<u64> {
        while data.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let start = at;
        while data.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        let digits = data
            .get(start..at)
            .filter(|d| !d.is_empty() && d.len() <= 10)?;
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    let (w, h, max) = (field().ok_or_else(bad)?, field().ok_or_else(bad)?, field());
    if max != Some(255) || !data.get(at).is_some_and(u8::is_ascii_whitespace) {
        return Err(bad());
    }
    if w > u64::from(width) || h > u64::from(height) || w == 0 || h == 0 {
        return Err(unreadable(format!(
            "its image is {w}×{h} pixels where at most {width}×{height} were asked for"
        )));
    }
    let len = (w * h * 3) as usize;
    let rgb = data.get(at + 1..at + 1 + len).ok_or_else(bad)?;
    Ok((w as u32, h as u32, rgb))
}

/// Each of `boxes` cut out of `buf` (`h` rows of `line` bytes, `w` pixels
/// of `color`), as a PNG, until the PNGs pass `budget` bytes.
fn cut(
    buf: &[u8],
    w: u32,
    h: u32,
    color: png::ColorType,
    line: usize,
    budget: u64,
    boxes: &[[u32; 4]],
) -> Result<Cut, Failure> {
    let ch = color.samples();
    let (mut answer, mut blobs) = (Vec::with_capacity(boxes.len()), Vec::new());
    let mut used = 0u64;
    for &[x, y, cw, chh] in boxes {
        // Clipped to what poppler rendered.
        let x0 = x.min(w - 1) as usize;
        let y0 = y.min(h - 1) as usize;
        let x1 = x.saturating_add(cw).min(w) as usize;
        let y1 = y.saturating_add(chh).min(h) as usize;
        let (cw, chh) = ((x1 - x0).max(1), (y1 - y0).max(1));
        let mut data = Vec::with_capacity(cw * chh * ch);
        for row in y0..y0 + chh {
            let at = row * line + x0 * ch;
            data.extend_from_slice(&buf[at..at + cw * ch]);
        }
        let mut out = Vec::new();
        let written = (|| {
            let mut enc = png::Encoder::new(&mut out, cw as u32, chh as u32);
            enc.set_color(color);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut wr = enc.write_header()?;
            wr.write_image_data(&data)?;
            wr.finish()
        })();
        drop(data);
        written.map_err(|e| unreadable(format!("its picture could not be written as PNG: {e}")))?;
        let len = out.len() as u64;
        if used.saturating_add(len) > budget {
            answer.push(None);
            continue;
        }
        used += len;
        answer.push(Some(Crop {
            width: cw as u32,
            height: chh as u32,
            len,
        }));
        blobs.push(out);
    }
    Ok((answer, blobs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        let data: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
        wr.write_image_data(&data).unwrap();
        wr.finish().unwrap();
        out
    }

    #[test]
    fn crops_are_cut_from_the_raster_and_clipped_to_it() {
        let png = png_of(20, 10);
        // The second passes the edge: clipped.
        let (c, blobs) = crops(&png, 20, 10, u64::MAX, &[[0, 0, 5, 5], [15, 8, 50, 50]]).unwrap();
        let c0 = c[0].unwrap();
        assert_eq!((c0.width, c0.height), (5, 5));
        assert!(blobs[0].starts_with(b"\x89PNG"));
        assert_eq!(c0.len, blobs[0].len() as u64);
        let c1 = c[1].unwrap();
        assert_eq!((c1.width, c1.height), (5, 2));
        assert_eq!(c1.len, blobs[1].len() as u64);
    }

    #[test]
    fn crops_past_the_budget_are_left_out_and_a_lying_image_is_refused() {
        let png = png_of(20, 10);
        let (one, _) = crops(&png, 20, 10, u64::MAX, &[[0, 0, 20, 10]]).unwrap();
        let len = one[0].unwrap().len;
        // Room for one whole crop, not two: the second is left out.
        let (c, blobs) = crops(&png, 20, 10, len, &[[0, 0, 20, 10], [0, 0, 20, 10]]).unwrap();
        assert!(c[0].is_some() && c[1].is_none());
        assert_eq!(blobs.len(), 1);
        // Larger than the box asked of poppler: refused before decoding.
        let Err(Failure::Unreadable(why)) = crops(&png, 10, 10, u64::MAX, &[[0, 0, 1, 1]]) else {
            panic!("decoded");
        };
        assert!(why.contains("20×10"), "{why}");
        assert!(crops(b"garbage", 1, 1, u64::MAX, &[[0, 0, 1, 1]]).is_err());
    }

    /// A PPM of the same pixels gives the same crops, byte for byte; one
    /// larger than asked, cut short or not 8 bits is refused.
    #[test]
    fn a_ppm_render_gives_the_png_render_s_crops() {
        let (w, h) = (20u32, 10u32);
        let data: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.extend_from_slice(&data);
        let boxes = [[0, 0, 5, 5], [15, 8, 50, 50]];
        let from_png = crops(&png_of(w, h), w, h, u64::MAX, &boxes).unwrap();
        let from_ppm = crops(&ppm, w, h, u64::MAX, &boxes).unwrap();
        assert_eq!(from_png.1, from_ppm.1);
        assert_eq!(from_png.0, from_ppm.0);
        assert!(crops(&ppm, 10, 10, u64::MAX, &boxes).is_err());
        assert!(crops(&ppm[..ppm.len() - 1], w, h, u64::MAX, &boxes).is_err());
        let deep = format!("P6\n{w} {h}\n65535\n").into_bytes();
        assert!(crops(&deep, w, h, u64::MAX, &boxes).is_err());
    }
}
