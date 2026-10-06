//! A PNG as stb_image v2.30 reads it (`stbi__parse_png_file`,
//! `stbi__parse_zlib_header`): the signature, an IHDR it accepts first, the
//! chunks it knows in an order it accepts, at least one IDAT starting a zlib
//! stream it accepts, and an IEND. Chunk CRCs are not read (stb_image skips
//! them); bytes after the IEND are never read either.

use super::{Reader, MAX_DIMENSION};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// What the IHDR says, as far as later chunks depend on it.
struct Header {
    /// Colour type 3: the IDAT indexes a PLTE.
    paletted: bool,
    /// Channels before a tRNS: 1 (grey), 2 (grey+alpha), 3 (rgb), 4 (rgba).
    channels: u32,
}

pub(super) fn check(bytes: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(bytes);
    if r.take(8, "signature")? != SIGNATURE {
        return Err("it has no PNG signature".into());
    }
    let mut header: Option<Header> = None;
    // Apple's CgBI: a raw deflate stream, without the zlib header.
    let mut cgbi = false;
    let mut palette = 0u32;
    let mut idat = 0u64;
    // The first bytes of the IDAT data, across chunks: its zlib header.
    let mut zlib = Vec::with_capacity(2);
    loop {
        if r.at_end() {
            let missing = if header.is_none() { "IHDR" } else { "IEND" };
            return Err(format!("it ends before its {missing} chunk"));
        }
        let len = r.be32("chunk header")?;
        let kind: [u8; 4] = r.take(4, "chunk header")?.try_into().expect("four bytes");
        let name = kind.escape_ascii().to_string();
        if &kind == b"IEND" {
            if header.is_none() {
                return Err("its IEND comes before its IHDR".into());
            }
            if idat == 0 {
                return Err("it has no IDAT chunk".into());
            }
            return zlib_header(&zlib, idat, cgbi);
        }
        let data = r.take(len as usize, &format!("{name} chunk"))?;
        r.take(4, &format!("{name} chunk's CRC"))?;
        if &kind == b"CgBI" {
            cgbi = true;
            continue;
        }
        if &kind == b"IHDR" {
            if header.is_some() {
                return Err("it has two IHDR chunks".into());
            }
            header = Some(ihdr(data)?);
            continue;
        }
        let Some(h) = &header else {
            return Err(format!("its first chunk is {name}, not IHDR"));
        };
        match &kind {
            b"PLTE" => {
                if len > 256 * 3 || len % 3 != 0 {
                    return Err(format!("its PLTE chunk is {len} bytes, not a palette"));
                }
                palette = len / 3;
            }
            b"tRNS" => {
                if idat > 0 {
                    return Err("its tRNS chunk comes after IDAT".into());
                }
                if h.paletted {
                    if palette == 0 {
                        return Err("its tRNS chunk comes before PLTE".into());
                    }
                    if len > palette {
                        return Err("its tRNS chunk is longer than its palette".into());
                    }
                } else {
                    if h.channels % 2 == 0 {
                        return Err("it has a tRNS chunk and an alpha channel".into());
                    }
                    if len != h.channels * 2 {
                        return Err(format!("its tRNS chunk is {len} bytes"));
                    }
                }
            }
            b"IDAT" => {
                if h.paletted && palette == 0 {
                    return Err("its IDAT comes before its PLTE".into());
                }
                if len > 1 << 30 {
                    return Err("an IDAT chunk is larger than 2^30 bytes".into());
                }
                idat += u64::from(len);
                let want = 2usize.saturating_sub(zlib.len()).min(data.len());
                zlib.extend_from_slice(&data[..want]);
            }
            // The fifth bit of the first letter marks an ancillary chunk,
            // which stb_image skips; it fails on a critical one it does not
            // know.
            _ if kind[0] & 0x20 == 0 => {
                return Err(format!(
                    "it has a critical chunk {name} stb_image does not know"
                ));
            }
            _ => {}
        }
    }
}

/// The IHDR's checks, in stb_image's order.
fn ihdr(data: &[u8]) -> Result<Header, String> {
    if data.len() != 13 {
        return Err(format!("its IHDR chunk is {} bytes, not 13", data.len()));
    }
    let mut r = Reader::new(data);
    let width = u64::from(r.be32("IHDR")?);
    let height = u64::from(r.be32("IHDR")?);
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!(
            "it is {width}x{height}, larger than stb_image takes"
        ));
    }
    let depth = r.u8("IHDR")?;
    if ![1, 2, 4, 8, 16].contains(&depth) {
        return Err(format!("its bit depth is {depth}"));
    }
    let colour = r.u8("IHDR")?;
    let paletted = colour == 3;
    if colour > 6 || (paletted && depth == 16) || (!paletted && colour & 1 == 1) {
        return Err(format!(
            "its colour type {colour} at depth {depth} is not one PNG has"
        ));
    }
    if r.u8("IHDR")? != 0 {
        return Err("its compression method is not deflate".into());
    }
    if r.u8("IHDR")? != 0 {
        return Err("its filter method is not 0".into());
    }
    if r.u8("IHDR")? > 1 {
        return Err("its interlace method is not one PNG has".into());
    }
    if width == 0 || height == 0 {
        return Err("it has no pixels".into());
    }
    let channels = (if colour & 2 != 0 { 3 } else { 1 }) + u32::from(colour & 4 != 0);
    // stb_image's own bound on the decoded size, per row of channels.
    let per_pixel = if paletted { 4 } else { u64::from(channels) };
    if (1u64 << 30) / width / per_pixel < height {
        return Err(format!(
            "it is {width}x{height}, too large for stb_image to decode"
        ));
    }
    Ok(Header { paletted, channels })
}

/// The zlib header the IDAT stream starts with (`stbi__parse_zlib_header`),
/// unless a CgBI chunk said there is none.
fn zlib_header(head: &[u8], idat: u64, cgbi: bool) -> Result<(), String> {
    if cgbi {
        return Ok(());
    }
    // stb_image wants a byte after the two of the header.
    let [cmf, flg] = head else {
        return Err("its IDAT data is too short for a zlib stream".into());
    };
    if idat < 3 {
        return Err("its IDAT data is too short for a zlib stream".into());
    }
    if (u16::from(*cmf) * 256 + u16::from(*flg)) % 31 != 0 {
        return Err("its IDAT data does not start with a zlib header".into());
    }
    if flg & 0x20 != 0 {
        return Err("its zlib stream needs a preset dictionary".into());
    }
    if cmf & 0x0F != 8 {
        return Err("its zlib stream is not deflate".into());
    }
    Ok(())
}
