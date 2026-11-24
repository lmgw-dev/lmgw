//! A rendered page as the model's input: pre-processing as docling does it.
//!
//! The pinned export takes `images`, unsigned bytes `[1, 3, SIDE, SIDE]`
//! (red, green and blue planes, 0 to 255: its `preprocessor_config.json`
//! has `do_rescale` and `do_normalize` off, the scaling is in the graph),
//! and the page stretched to the square without keeping its shape
//! (`do_resize` to 640 × 640, `do_pad` off). `pdftoppm -scale-to-x SIDE
//! -scale-to-y SIDE` renders the page straight to that square, so the
//! gateway resizes nothing: the render's resolution is the model's input
//! and no more. On the WP0a corpus this gave the regions docling.rs's
//! 72 DPI render with a bilinear resize gives (measured in WP8).
//!
//! What comes back from `pdftoppm` is a binary PPM (`P6`): a short header
//! and the pixels row by row. It is read as input all the same: the size
//! must be the square asked for and the pixels all there.

/// The model's input side in pixels (`preprocessor_config.json` `size`).
pub const SIDE: u32 = 640;

/// One header token of a PPM from `at`: skips white space and `#` comment
/// lines, at most 16 bytes long.
fn token<'a>(ppm: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    loop {
        while ppm.get(*at)?.is_ascii_whitespace() {
            *at += 1;
        }
        if ppm[*at] != b'#' {
            break;
        }
        while *ppm.get(*at)? != b'\n' {
            *at += 1;
        }
    }
    let start = *at;
    while ppm.get(*at).is_some_and(|b| !b.is_ascii_whitespace()) {
        *at += 1;
        if *at - start > 16 {
            return None;
        }
    }
    Some(&ppm[start..*at])
}

fn number(t: &[u8]) -> Option<u32> {
    std::str::from_utf8(t).ok()?.parse().ok()
}

/// The pixels of a [`SIDE`]-square `P6` image as the model's input: the
/// three colour planes one after the other.
pub fn input(ppm: &[u8]) -> Result<Vec<u8>, String> {
    let bad = |why: &str| format!("the page's render is not the image asked of pdftoppm: {why}");
    let mut at = 0;
    let magic = token(ppm, &mut at).ok_or_else(|| bad("no header"))?;
    if magic != b"P6" {
        return Err(bad("not a binary PPM"));
    }
    let mut next = || token(ppm, &mut at).and_then(number);
    let (w, h, max) = (next(), next(), next());
    let (Some(w), Some(h), Some(max)) = (w, h, max) else {
        return Err(bad("its header is cut short"));
    };
    if w != SIDE || h != SIDE || max != 255 {
        return Err(bad(&format!(
            "{w} x {h} pixels of up to {max}, not {SIDE} x {SIDE} of up to 255"
        )));
    }
    // One white-space byte ends the header.
    let n = (SIDE as usize) * (SIDE as usize);
    let pixels = ppm
        .get(at + 1..)
        .filter(|p| p.len() == n * 3)
        .ok_or_else(|| bad("its pixels are cut short or run on"))?;
    let mut planes = vec![0u8; n * 3];
    for (i, px) in pixels.as_chunks::<3>().0.iter().enumerate() {
        planes[i] = px[0];
        planes[n + i] = px[1];
        planes[2 * n + i] = px[2];
    }
    Ok(planes)
}
