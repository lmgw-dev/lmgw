//! An office picture's format and size, read from its first bytes: nothing
//! is decoded. The formats a browser shows in a `data:` URI or a link are
//! exported; EMF/WMF and anything unknown are not yet (a `pictures_partial`
//! note says how many).

/// A media entry's format: its media type and the artifact's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub mime: &'static str,
    pub ext: &'static str,
}

/// The format of `b`, when it is one lmgw exports.
pub fn kind(b: &[u8]) -> Option<Kind> {
    let k = |mime, ext| Some(Kind { mime, ext });
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        k("image/png", "png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        k("image/jpeg", "jpg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        k("image/gif", "gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        k("image/webp", "webp")
    } else if b.starts_with(b"BM") {
        k("image/bmp", "bmp")
    } else if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        k("image/tiff", "tif")
    } else if svg(b) {
        k("image/svg+xml", "svg")
    } else {
        None
    }
}

/// An SVG: `<svg` within the first KiB, after an XML prolog or comments.
fn svg(b: &[u8]) -> bool {
    let head = &b[..b.len().min(1024)];
    let t = String::from_utf8_lossy(head);
    let t = t.trim_start_matches('\u{feff}').trim_start();
    (t.starts_with("<?xml") || t.starts_with("<!--") || t.starts_with("<svg")) && t.contains("<svg")
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

/// The JPEG markers looked at for the frame header at most: a real file has
/// it within a few dozen (after APPn and quantisation tables).
const JPEG_MARKERS_MAX: usize = 256;

/// Width and height in pixels, as the header declares them (PNG, GIF,
/// JPEG); `None` for the rest.
pub fn size(b: &[u8]) -> Option<(u32, u32)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
        return Some((w, h));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(b, 6)?, le16(b, 8)?));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut at = 2usize;
        for _ in 0..JPEG_MARKERS_MAX {
            if *b.get(at)? != 0xFF {
                return None;
            }
            let marker = *b.get(at + 1)?;
            // SOF0..SOF15 but DHT (C4), JPG (C8) and DAC (CC).
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return Some((be16(b, at + 7)?, be16(b, at + 5)?));
            }
            let len = usize::try_from(be16(b, at + 2)?).ok()?;
            at = at.checked_add(2)?.checked_add(len)?;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_sizes_come_from_the_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&400u32.to_be_bytes());
        png.extend_from_slice(&300u32.to_be_bytes());
        assert_eq!(kind(&png).unwrap().ext, "png");
        assert_eq!(size(&png), Some((400, 300)));
        let gif = b"GIF89a\x10\x00\x20\x00";
        assert_eq!(kind(gif).unwrap().mime, "image/gif");
        assert_eq!(size(gif), Some((16, 32)));
        // SOI, an APP0 of 4 bytes, SOF0 with height 2, width 3.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0, 0xFF, 0xC0, 0x00, 0x11, 8, 0x00, 0x02, 0x00,
            0x03,
        ];
        assert_eq!(kind(&jpeg).unwrap().ext, "jpg");
        assert_eq!(size(&jpeg), Some((3, 2)));
        assert!(kind(b"<?xml version=\"1.0\"?><svg/>").is_some());
        // EMF and garbage are not exported.
        assert_eq!(kind(&[1, 0, 0, 0, 0x6c, 0, 0, 0]), None);
        assert_eq!(size(b"BM"), None);
        // A JPEG whose segments loop or run off the end has no size.
        assert_eq!(size(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]), None);
    }
}
