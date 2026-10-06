//! A JPEG as stb_image v2.30 reads it (`stbi__decode_jpeg_image` and what it
//! calls), walked from its SOI to its EOI.
//!
//! - **Before the frame:** only the segments stb_image processes (DQT, DHT,
//!   DRI, APPn, COM), each whole and as stb_image checks it; any other
//!   marker fails there. Fill bytes between segments are skipped, as
//!   stb_image skips them.
//! - **The frame:** baseline (SOF0), extended sequential (SOF1) or
//!   progressive (SOF2), with 8-bit samples: stb_image's `stbi__SOF`. A
//!   12-bit SOF1, lossless SOF3, the differential SOF5-7 and the
//!   arithmetic-coded SOF9-11 and SOF13-15 all fail, as does JPEG-LS.
//! - **The scans:** each SOS header as stb_image checks it, the Huffman
//!   tables it uses already defined, then its entropy-coded data to the next
//!   marker (stuffed `FF 00` and restart markers belong to the data).
//! - **The end:** an EOI after at least one scan. Bytes after the EOI are
//!   never read, by stb_image or here. A file cut off before its EOI fails,
//!   though stb_image would decode as far as it got (module doc of
//!   [`super`]).

use super::{product_fits, Reader};

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DNL: u8 = 0xDC;

/// The frame, as the scans after it are checked against.
struct Frame {
    height: u16,
    progressive: bool,
    /// The component ids, in frame order.
    ids: Vec<u8>,
}

/// Which Huffman tables are defined: `[class][slot]`, class 0 DC, 1 AC.
type Tables = [[bool; 4]; 2];

pub(super) fn check(bytes: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(bytes);
    if marker(&mut r) != Some(SOI) {
        return Err("it does not start with an SOI marker".into());
    }
    let mut tables: Tables = [[false; 4]; 2];
    let mut m = marker(&mut r);
    let frame = loop {
        let Some(found) = m else {
            return Err("its SOI is not followed by a marker".into());
        };
        if matches!(found, 0xC0..=0xC2) {
            break frame_header(&mut r, found == 0xC2)?;
        }
        segment(&mut r, found, &mut tables)?;
        m = marker(&mut r);
        while m.is_none() {
            if r.at_end() {
                return Err("it has no frame (SOF) marker".into());
            }
            m = marker(&mut r);
        }
    };
    let mut scans = 0u32;
    loop {
        match marker(&mut r) {
            None if r.at_end() => return Err("it ends before its EOI marker".into()),
            None => return Err("its frame is followed by bytes that are not a marker".into()),
            Some(EOI) if scans == 0 => return Err("it has no scan before its EOI".into()),
            Some(EOI) => return Ok(()),
            Some(SOS) => {
                scan_header(&mut r, &frame, &tables)?;
                entropy_coded(&mut r)?;
                scans += 1;
            }
            Some(DNL) => {
                if r.be16("DNL segment")? != 4 {
                    return Err("its DNL segment is not 4 bytes".into());
                }
                if r.be16("DNL segment")? != frame.height {
                    return Err("its DNL height differs from its frame's".into());
                }
            }
            Some(found) => segment(&mut r, found, &mut tables)?,
        }
    }
}

/// The next marker (`stbi__get_marker`): `FF`, any fill `FF`s, then the
/// marker byte. `None` when the next byte is not `FF`, which is consumed,
/// as stb_image consumes it. A marker byte past the end reads as `0`, an
/// unknown marker, as stb_image's reader returns `0` there.
fn marker(r: &mut Reader<'_>) -> Option<u8> {
    let next = |r: &mut Reader<'_>| r.u8("marker").unwrap_or(0);
    if next(r) != 0xFF {
        return None;
    }
    let mut x = 0xFF;
    while x == 0xFF {
        x = next(r);
    }
    Some(x)
}

/// One segment stb_image processes outside a scan (`stbi__process_marker`),
/// or why it fails on the marker.
fn segment(r: &mut Reader<'_>, m: u8, tables: &mut Tables) -> Result<(), String> {
    match m {
        // DRI
        0xDD => {
            if r.be16("DRI segment")? != 4 {
                return Err("its DRI segment is not 4 bytes".into());
            }
            r.be16("DRI segment")?;
            Ok(())
        }
        // DQT
        0xDB => {
            let mut left = i32::from(r.be16("DQT segment")?) - 2;
            while left > 0 {
                let q = r.u8("DQT segment")?;
                if q >> 4 > 1 {
                    return Err("a quantization table's precision is not 8 or 16 bits".into());
                }
                if q & 15 > 3 {
                    return Err("a quantization table's slot is above 3".into());
                }
                let wide = q >> 4 == 1;
                r.take(if wide { 128 } else { 64 }, "DQT segment")?;
                left -= if wide { 129 } else { 65 };
            }
            if left != 0 {
                return Err("its DQT segment's length does not match its tables".into());
            }
            Ok(())
        }
        // DHT
        0xC4 => {
            let mut left = i32::from(r.be16("DHT segment")?) - 2;
            while left > 0 {
                let q = r.u8("DHT segment")?;
                let (class, slot) = (usize::from(q >> 4), usize::from(q & 15));
                if class > 1 || slot > 3 {
                    return Err("a Huffman table's class or slot is out of range".into());
                }
                let counts = r.take(16, "DHT segment")?;
                let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
                if n > 256 {
                    return Err("a Huffman table has more than 256 codes".into());
                }
                if !canonical(counts) {
                    return Err("a Huffman table's code lengths overflow".into());
                }
                r.take(n, "DHT segment")?;
                left -= 17 + n as i32;
                tables[class][slot] = true;
            }
            if left != 0 {
                return Err("its DHT segment's length does not match its tables".into());
            }
            Ok(())
        }
        // APPn, COM
        0xE0..=0xEF | 0xFE => {
            let len = r.be16("APP or COM segment")?;
            if len < 2 {
                return Err("an APP or COM segment is shorter than its length field".into());
            }
            r.take(usize::from(len) - 2, "APP or COM segment")?;
            Ok(())
        }
        other => Err(unknown(other)),
    }
}

/// Why stb_image fails on marker `m` where it expects a segment.
fn unknown(m: u8) -> String {
    match m {
        0xC3 => "it is a lossless JPEG (SOF3)".into(),
        0xC5..=0xC7 => format!("it is a differential JPEG (SOF{})", m - 0xC0),
        0xC9..=0xCB | 0xCD..=0xCF => {
            format!("it is an arithmetic-coded JPEG (SOF{})", m - 0xC0)
        }
        0xCC => "it uses arithmetic coding (DAC)".into(),
        0xDE => "it is a hierarchical JPEG (DHP)".into(),
        0xF7 => "it is a JPEG-LS image (SOF55)".into(),
        _ => format!("it has a marker FF{m:02X} stb_image does not take there"),
    }
}

/// `stbi__build_huffman`'s check: the canonical codes of these per-length
/// counts fit their lengths.
fn canonical(counts: &[u8]) -> bool {
    let mut code: u32 = 0;
    for (i, &count) in counts.iter().enumerate() {
        let bits = i as u32 + 1;
        if count > 0 {
            code += u32::from(count);
            if code > 1 << bits {
                return false;
            }
        }
        code <<= 1;
    }
    true
}

/// The frame header (`stbi__process_frame_header`).
fn frame_header(r: &mut Reader<'_>, progressive: bool) -> Result<Frame, String> {
    let len = r.be16("frame header")?;
    if len < 11 {
        return Err("its frame header is shorter than 11 bytes".into());
    }
    let precision = r.u8("frame header")?;
    if precision != 8 {
        return Err(format!(
            "its samples are {precision}-bit, and stb_image decodes 8-bit only"
        ));
    }
    let height = r.be16("frame header")?;
    if height == 0 {
        return Err("its height is left to a DNL marker, which stb_image does not take".into());
    }
    let width = r.be16("frame header")?;
    if width == 0 {
        return Err("its width is 0".into());
    }
    let n = r.u8("frame header")?;
    if ![1, 3, 4].contains(&n) {
        return Err(format!("it has {n} components, not 1, 3 or 4"));
    }
    if usize::from(len) != 8 + 3 * usize::from(n) {
        return Err("its frame header's length does not match its components".into());
    }
    let mut ids = Vec::with_capacity(usize::from(n));
    let mut sampling = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        ids.push(r.u8("frame header")?);
        let hv = r.u8("frame header")?;
        let (h, v) = (hv >> 4, hv & 15);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
            return Err("a component's sampling factor is not 1 to 4".into());
        }
        if r.u8("frame header")? > 3 {
            return Err("a component names a quantization table above 3".into());
        }
        sampling.push((h, v));
    }
    if !product_fits(u64::from(width), u64::from(height), u64::from(n)) {
        return Err(format!(
            "it is {width}x{height}, too large for stb_image to decode"
        ));
    }
    let h_max = sampling.iter().map(|s| s.0).max().unwrap_or(1);
    let v_max = sampling.iter().map(|s| s.1).max().unwrap_or(1);
    if sampling
        .iter()
        .any(|&(h, v)| h_max % h != 0 || v_max % v != 0)
    {
        return Err("its components' sampling factors are not integer ratios".into());
    }
    Ok(Frame {
        height,
        progressive,
        ids,
    })
}

/// A scan header (`stbi__process_scan_header`), and the Huffman tables its
/// entropy-coded data will decode with.
fn scan_header(r: &mut Reader<'_>, frame: &Frame, tables: &Tables) -> Result<(), String> {
    let len = r.be16("scan header")?;
    let n = r.u8("scan header")?;
    if !(1..=4).contains(&n) || usize::from(n) > frame.ids.len() {
        return Err(format!("a scan has {n} components"));
    }
    if usize::from(len) != 6 + 2 * usize::from(n) {
        return Err("a scan header's length does not match its components".into());
    }
    let mut uses = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        let id = r.u8("scan header")?;
        if !frame.ids.contains(&id) {
            return Err("a scan names a component its frame does not have".into());
        }
        let q = r.u8("scan header")?;
        let (dc, ac) = (usize::from(q >> 4), usize::from(q & 15));
        if dc > 3 || ac > 3 {
            return Err("a scan names a Huffman table above 3".into());
        }
        uses.push((dc, ac));
    }
    let start = r.u8("scan header")?;
    let end = r.u8("scan header")?;
    let approx = r.u8("scan header")?;
    let (high, low) = (approx >> 4, approx & 15);
    let (dc_needed, ac_needed) = if frame.progressive {
        if start > 63 || end > 63 || start > end || high > 13 || low > 13 {
            return Err("a progressive scan's spectral selection is out of range".into());
        }
        // stb_image decodes a DC scan, or one component's AC: never both,
        // and never AC across components.
        if (start == 0 && end != 0) || (start > 0 && n > 1) {
            return Err("a progressive scan mixes DC and AC coefficients".into());
        }
        (start == 0 && high == 0, start > 0)
    } else {
        if start != 0 || high != 0 || low != 0 {
            return Err("a sequential scan has a progressive spectral selection".into());
        }
        (true, true)
    };
    for (dc, ac) in uses {
        if (dc_needed && !tables[0][dc]) || (ac_needed && !tables[1][ac]) {
            return Err("a scan uses a Huffman table it never defines".into());
        }
    }
    Ok(())
}

/// A scan's entropy-coded data, up to the marker after it, which is left
/// for [`marker`]: a stuffed `FF 00` and a restart marker `FF D0`-`FF D7`
/// are part of the data, fill `FF`s before a marker are skipped.
fn entropy_coded(r: &mut Reader<'_>) -> Result<(), String> {
    let bytes = r.bytes;
    let mut at = r.at;
    loop {
        let Some(off) = bytes
            .get(at..)
            .and_then(|b| b.iter().position(|&x| x == 0xFF))
        else {
            return Err("it ends inside a scan, before its EOI marker".into());
        };
        let ff = at + off;
        let mut next = ff + 1;
        while bytes.get(next) == Some(&0xFF) {
            next += 1;
        }
        match bytes.get(next) {
            None => return Err("it ends inside a scan, before its EOI marker".into()),
            Some(0x00) | Some(0xD0..=0xD7) => at = next + 1,
            Some(_) => {
                r.at = ff;
                return Ok(());
            }
        }
    }
}
