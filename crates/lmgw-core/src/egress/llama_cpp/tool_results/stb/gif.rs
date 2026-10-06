//! A GIF as stb_image v2.30 reads it (`stbi__gif_header`,
//! `stbi__gif_load_next`, `stbi__process_gif_raster`), walked block by block
//! to its trailer.
//!
//! stb_image decodes the first image only, so that one is checked as it
//! checks it: inside the logical screen, a colour table to index, an LZW
//! code size it takes, and a raster whose first code is a clear (or end)
//! code. Every block after it is only walked. A Graphic Control Extension
//! whose block is not 4 bytes fails: stb_image would read past it out of
//! step with the blocks. The trailer is required, though stb_image stops
//! reading after the first image (module doc of [`super`]); bytes after it
//! are never read.

use super::{product_fits, Reader};

pub(super) fn check(bytes: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(bytes);
    let magic = r.take(6, "header")?;
    if magic != b"GIF87a" && magic != b"GIF89a" {
        return Err("it has no GIF87a or GIF89a header".into());
    }
    let width = r.le16("logical screen descriptor")?;
    let height = r.le16("logical screen descriptor")?;
    let flags = r.u8("logical screen descriptor")?;
    r.take(2, "logical screen descriptor")?;
    if width == 0 || height == 0 {
        return Err("its logical screen has no pixels".into());
    }
    if !product_fits(4, u64::from(width), u64::from(height)) {
        return Err(format!(
            "it is {width}x{height}, too large for stb_image to decode"
        ));
    }
    let global = flags & 0x80 != 0;
    if global {
        r.take(colour_table(flags), "global colour table")?;
    }
    let mut images = 0u32;
    loop {
        match r.u8("block list (no trailer)")? {
            // Image descriptor
            0x2C => {
                let x = r.le16("image descriptor")?;
                let y = r.le16("image descriptor")?;
                let w = r.le16("image descriptor")?;
                let h = r.le16("image descriptor")?;
                let local = r.u8("image descriptor")?;
                let first = images == 0;
                if first
                    && (u32::from(x) + u32::from(w) > u32::from(width)
                        || u32::from(y) + u32::from(h) > u32::from(height))
                {
                    return Err("its first image lies outside its logical screen".into());
                }
                if local & 0x80 != 0 {
                    r.take(colour_table(local), "local colour table")?;
                } else if first && !global {
                    return Err("its first image has no colour table".into());
                }
                let code_size = r.u8("image data")?;
                if first && code_size > 12 {
                    return Err(format!("its LZW code size is {code_size}, above 12"));
                }
                let head = sub_blocks(&mut r, "image data")?;
                if first {
                    first_code(code_size, &head)?;
                }
                images += 1;
            }
            // Extension
            0x21 => {
                let label = r.u8("extension")?;
                if label == 0xF9 {
                    let len = r.u8("graphic control extension")?;
                    if len != 4 {
                        return Err(format!(
                            "its graphic control extension is {len} bytes, not 4"
                        ));
                    }
                    r.take(4, "graphic control extension")?;
                }
                sub_blocks(&mut r, "extension")?;
            }
            // Trailer
            0x3B if images == 0 => return Err("it has no image before its trailer".into()),
            0x3B => return Ok(()),
            other => return Err(format!("it has an unknown block {other:#04X}")),
        }
    }
}

/// The size of a colour table its flags announce.
fn colour_table(flags: u8) -> usize {
    3 * (2 << (flags & 7))
}

/// Walk data sub-blocks to their terminator; the first two data bytes, for
/// [`first_code`].
fn sub_blocks(r: &mut Reader<'_>, what: &str) -> Result<Vec<u8>, String> {
    let mut head = Vec::with_capacity(2);
    loop {
        let len = r.u8(what)?;
        if len == 0 {
            return Ok(head);
        }
        let data = r.take(usize::from(len), what)?;
        let want = 2usize.saturating_sub(head.len()).min(data.len());
        head.extend_from_slice(&data[..want]);
    }
}

/// stb_image's first LZW code: it fails on any but the clear code or the end
/// code. A raster too short to hold one code decodes to nothing, as there.
fn first_code(code_size: u8, head: &[u8]) -> Result<(), String> {
    let bits = u32::from(code_size) + 1;
    let have = 8 * head.len() as u32;
    if have < bits {
        return Ok(());
    }
    let word = head
        .iter()
        .rev()
        .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    let code = word & ((1 << bits) - 1);
    let clear = 1u32 << code_size;
    if code == clear || code == clear + 1 {
        Ok(())
    } else {
        Err("its first image's LZW data does not start with a clear code".into())
    }
}
