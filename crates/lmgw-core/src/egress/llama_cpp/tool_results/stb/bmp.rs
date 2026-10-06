//! A BMP as stb_image v2.30 reads it (`stbi__bmp_test`,
//! `stbi__bmp_parse_header`, `stbi__bmp_load`).
//!
//! - **The DIB header** is one of the five sizes stb_image takes (12, 40,
//!   56, 108, 124), with one plane.
//! - **Compression** is none (0) or bitfields (3, at 16 or 32 bits): RLE
//!   (1, 2) and embedded JPEG/PNG (4, 5) fail there.
//! - **Pixels:** 1, 4 or 8 bits through a palette of 1 to 256 entries; 24
//!   bits; or 16 and 32 bits through channel masks of at most 8 bits each.
//!   A header size and bit depth that leave the masks empty (a 16-bit BMP
//!   with a 12-byte header) fail as stb_image fails them.
//! - **The pixel offset** lies where stb_image accepts it, and the pixel rows
//!   are all there, though stb_image would read zeros past the end (module
//!   doc of [`super`]). A zero width or height fails too: stb_image would
//!   hand llama.cpp an empty image.

use super::{product_fits, Reader, MAX_DIMENSION};

/// The header sizes stb_image takes.
const HEADER_SIZES: [u32; 5] = [12, 40, 56, 108, 124];

pub(super) fn check(bytes: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(bytes);
    if r.take(2, "file header")? != b"BM" {
        return Err("it does not start with BM".into());
    }
    r.take(8, "file header")?;
    let offset = r.le32("file header")? as i32;
    let size = r.le32("DIB header")?;
    if !HEADER_SIZES.contains(&size) {
        return Err(format!(
            "its DIB header is {size} bytes, a size stb_image does not take"
        ));
    }
    if offset < 0 {
        return Err("its pixel offset is negative".into());
    }
    let (width, height) = if size == 12 {
        (
            u64::from(r.le16("DIB header")?),
            u64::from(r.le16("DIB header")?),
        )
    } else {
        let w = u64::from(r.le32("DIB header")?);
        let h = r.le32("DIB header")? as i32;
        (w, u64::from(h.unsigned_abs()))
    };
    if r.le16("DIB header")? != 1 {
        return Err("it does not have one plane".into());
    }
    let bpp = r.le16("DIB header")?;
    let mut masks = [0u32; 4];
    // Bytes stb_image has read when it judges the offset: the file header,
    // the DIB header, and bitfield masks after a 40- or 56-byte one.
    let mut read = 14 + size;
    if size != 12 {
        let compression = r.le32("DIB header")?;
        match compression {
            0 => {}
            1 | 2 => return Err("it is run-length encoded, which stb_image does not decode".into()),
            3 if bpp != 16 && bpp != 32 => {
                return Err(format!("it has bitfields at {bpp} bits per pixel"))
            }
            3 => {}
            _ => {
                return Err(format!(
                    "its compression {compression} is not one stb_image takes"
                ))
            }
        }
        r.take(20, "DIB header")?;
        if size == 40 || size == 56 {
            if size == 56 {
                r.take(16, "DIB header")?;
            }
            if bpp == 16 || bpp == 32 {
                if compression == 3 {
                    for m in &mut masks[..3] {
                        *m = r.le32("bitfield masks")?;
                    }
                    read += 12;
                    if masks[0] == masks[1] && masks[1] == masks[2] {
                        return Err("its three colour masks are the same".into());
                    }
                } else {
                    masks = default_masks(bpp);
                }
            }
        } else {
            for m in &mut masks {
                *m = r.le32("DIB header")?;
            }
            if compression != 3 {
                masks = default_masks(bpp);
            }
            r.take(size as usize - 56, "DIB header")?;
        }
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!(
            "it is {width}x{height}, larger than stb_image takes"
        ));
    }
    if width == 0 || height == 0 {
        return Err("it has no pixels".into());
    }
    if !product_fits(3, width, height) {
        return Err(format!(
            "it is {width}x{height}, too large for stb_image to decode"
        ));
    }

    let offset = offset.unsigned_abs();
    // stb_image's palette size, from the gap between the headers and the
    // pixels, in its own arithmetic: a 12-byte header's entries are 3 bytes
    // (and it subtracts 24, not 12), every other one's 4. Below 16 bits no
    // masks were read, so the headers end at `14 + size`.
    let palette = match (size, bpp) {
        (12, ..=23) => (i64::from(offset) - 14 - 24) / 3,
        (12, _) => 0,
        (_, ..=15) => (i64::from(offset) - 14 - i64::from(size)) >> 2,
        _ => 0,
    };
    if palette == 0 && (offset < read || offset - read > 1024) {
        return Err("its pixel offset is not where stb_image looks for pixels".into());
    }
    let row = match bpp {
        1 | 4 | 8 => {
            if !(1..=256).contains(&palette) {
                return Err(format!("it has {bpp} bits per pixel and no usable palette"));
            }
            (width * u64::from(bpp)).div_ceil(8)
        }
        _ if bpp < 16 => return Err(format!("it has {bpp} bits per pixel")),
        24 => 3 * width,
        16 | 32 => {
            let easy = bpp == 32 && masks == [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000];
            if !easy && (masks[..3].contains(&0) || masks.iter().any(|m| m.count_ones() > 8)) {
                return Err("its channel masks are not ones stb_image reads".into());
            }
            u64::from(bpp / 8) * width
        }
        _ => {
            return Err(format!(
                "it has {bpp} bits per pixel, which stb_image does not read"
            ))
        }
    };
    // Rows are padded to four bytes; the last row's padding is not needed.
    let stride = row.div_ceil(4) * 4;
    let pixels = stride * (height - 1) + row;
    if (bytes.len() as u64) < u64::from(offset) + pixels {
        return Err("it ends before its last row of pixels".into());
    }
    Ok(())
}

/// The masks stb_image assumes without bitfields (`stbi__bmp_set_mask_defaults`):
/// 5-5-5 at 16 bits, 8-8-8-8 at 32, none otherwise.
fn default_masks(bpp: u16) -> [u32; 4] {
    match bpp {
        16 => [31 << 10, 31 << 5, 31, 0],
        32 => [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
        _ => [0; 4],
    }
}
