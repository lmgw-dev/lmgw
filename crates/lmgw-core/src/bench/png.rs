//! The vision probe's image (benchmark design §5): a 64×64 solid red PNG,
//! generated in code so the suite carries no binary asset and needs no image
//! crate.
//!
//! It is the smallest valid encoding there is — 8-bit RGB, no interlace,
//! filter 0 on every row, and the zlib stream made of one *stored* (i.e.
//! uncompressed) deflate block — so all it needs is CRC-32 for the chunks and
//! Adler-32 for the zlib trailer, both a dozen lines.

/// Side length in pixels.
pub const SIDE: u32 = 64;

/// The PNG's bytes.
pub fn solid_red() -> Vec<u8> {
    solid(SIDE, SIDE, [0xFF, 0x00, 0x00])
}

/// The same, base64-encoded as a `data:` URL for an OpenAI `image_url` part.
pub fn solid_red_data_url() -> String {
    use base64::Engine as _;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(solid_red())
    )
}

/// A `width`×`height` image of one colour. The raw scanlines must fit one
/// stored deflate block (65 535 bytes), which 64×64 RGB (12 352) does.
fn solid(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    // Filter type 0 (none), then the pixels; every row is the same.
    let row: Vec<u8> = std::iter::once(0)
        .chain(rgb.iter().copied().cycle().take(width as usize * 3))
        .collect();
    let raw = row.repeat(height as usize);
    assert!(
        raw.len() <= 0xFFFF,
        "one stored block holds at most 65535 bytes"
    );

    let mut zlib = vec![0x78, 0x01]; // deflate, 32K window, no dictionary
    zlib.push(0x01); // BFINAL = 1, BTYPE = 00 (stored)
    let len = raw.len() as u16;
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth 8, RGB, deflate, filter 0, no interlace

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (ISO-HDLC, the polynomial PNG uses), bitwise — the image is 12 KB,
/// a table buys nothing.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_their_reference_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    /// Walks the file the way a decoder does: signature, chunk lengths and
    /// CRCs, the IHDR fields, and the stored block's header and trailer.
    #[test]
    fn the_png_is_well_formed() {
        let png = solid_red();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut pos = 8;
        let mut kinds = Vec::new();
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&png[pos + 4..pos + 8 + len]));
            kinds.push(String::from_utf8(kind.to_vec()).unwrap());
            match kind {
                b"IHDR" => {
                    assert_eq!(&data[..8], &[0, 0, 0, 64, 0, 0, 0, 64]);
                    assert_eq!(&data[8..], &[8, 2, 0, 0, 0]);
                }
                b"IDAT" => {
                    assert_eq!(&data[..3], &[0x78, 0x01, 0x01]);
                    let n = u16::from_le_bytes([data[3], data[4]]) as usize;
                    assert_eq!(n, 64 * (1 + 64 * 3));
                    assert_eq!(u16::from_le_bytes([data[5], data[6]]), !(n as u16));
                    let raw = &data[7..7 + n];
                    assert_eq!(&raw[..4], &[0, 0xFF, 0, 0]);
                    let adler = u32::from_be_bytes(data[7 + n..].try_into().unwrap());
                    assert_eq!(adler, adler32(raw));
                    // zlib's header check: (CMF·256 + FLG) is a multiple of 31.
                    assert_eq!((0x78u32 * 256 + 0x01) % 31, 0);
                }
                _ => {}
            }
            pos += 12 + len;
        }
        assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"]);
        assert!(solid_red_data_url().starts_with("data:image/png;base64,iVBORw0KGgo"));
    }
}
