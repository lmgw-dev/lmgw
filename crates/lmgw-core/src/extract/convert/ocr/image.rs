//! A page's pixels for tesseract (document-conversion design §5.1, WP4).
//!
//! - A rendered PDF page is `pdftoppm -gray`'s PGM, handed on as it is.
//! - An image file (PNG, JPEG, GIF, WebP) is handed to tesseract as its own
//!   bytes, which tesseract reads with the file's own resolution
//!   ([`PageImage::file`]); it is only decoded (to grey, by lmgw's reader
//!   executable) when it has to be turned upright.
//! - A TIFF page is decoded to grey by lmgw's reader executable, one page a
//!   run, so a multi-page TIFF is read page by page like a PDF.
//!
//! No image decoder runs in the gateway. What is here is lmgw's own work on
//! bounded pixels: a PGM's header ([`parse_pgm`], which checks its size
//! against [`MAX_OCR_PIXELS`] and against the bytes that are there) and the
//! quarter turns ([`PageImage::rotated`]).

use bytes::Bytes;

use super::MAX_OCR_PIXELS;

/// One page's image, as tesseract reads it.
#[derive(Debug, Clone)]
pub enum PageImage {
    /// A grey PGM (`P5`, one byte a pixel): its bytes, its size and where
    /// its pixels start.
    Pgm {
        data: Bytes,
        width: u32,
        height: u32,
        offset: usize,
    },
    /// An image file's own bytes, which tesseract decodes itself
    /// ([`PageImage::file`]).
    File(Bytes),
}

/// What tesseract (leptonica) recognises on stdin, by its first bytes.
fn leptonica_reads(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xFF\xD8\xFF")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

impl PageImage {
    /// An image file handed to tesseract as it is: only a PNG, JPEG, GIF or
    /// WebP by its first bytes.
    ///
    /// `tesseract stdin` takes input leptonica does not recognise as a
    /// **list of file names to open and read**, one a line. A picture
    /// format the sniffer learns later, or bytes that merely passed it,
    /// must therefore never reach tesseract as a file: this is the one
    /// place a [`PageImage::File`] is made, and it refuses anything else.
    pub fn file(bytes: Bytes) -> Result<Self, String> {
        if leptonica_reads(&bytes) {
            Ok(Self::File(bytes))
        } else {
            Err(
                "the picture is not a PNG, JPEG, GIF or WebP by its first bytes, so it was not \
                 given to tesseract"
                    .to_string(),
            )
        }
    }

    /// What tesseract is given on stdin (shared, not copied).
    pub fn bytes(&self) -> Bytes {
        match self {
            Self::Pgm { data, .. } | Self::File(data) => data.clone(),
        }
    }

    /// The page turned `degrees` clockwise (90, 180 or 270), as a PGM.
    /// CPU-bound: run on a blocking thread that holds the page's slot. A
    /// picture's own bytes are decoded by lmgw's reader first
    /// (`ocr::turn`), never here.
    pub fn rotated(self, degrees: u32) -> Result<Self, String> {
        let (w, h, pixels) = match self {
            Self::Pgm {
                data,
                width,
                height,
                offset,
            } => (width, height, data.slice(offset..)),
            Self::File(_) => {
                return Err("a picture is decoded before it is turned".to_string());
            }
        };
        let (w, h) = (w as usize, h as usize);
        let mut out = vec![0u8; w * h];
        let (nw, nh) = match degrees {
            90 => {
                for y in 0..h {
                    for x in 0..w {
                        out[x * h + (h - 1 - y)] = pixels[y * w + x];
                    }
                }
                (h, w)
            }
            180 => {
                for (i, p) in pixels[..w * h].iter().rev().enumerate() {
                    out[i] = *p;
                }
                (w, h)
            }
            270 => {
                for y in 0..h {
                    for x in 0..w {
                        out[(w - 1 - x) * h + y] = pixels[y * w + x];
                    }
                }
                (h, w)
            }
            _ => return Err(format!("a page cannot be turned by {degrees}°")),
        };
        Ok(pgm(nw as u32, nh as u32, &out))
    }
}

/// `pixels` (one byte each, row by row) as a PGM.
pub fn pgm(width: u32, height: u32, pixels: &[u8]) -> PageImage {
    let header = format!("P5\n{width} {height}\n255\n");
    let mut data = Vec::with_capacity(header.len() + pixels.len());
    data.extend_from_slice(header.as_bytes());
    data.extend_from_slice(pixels);
    PageImage::Pgm {
        data: Bytes::from(data),
        width,
        height,
        offset: header.len(),
    }
}

/// Why a page of `width` × `height` pixels is not read.
pub fn too_large(width: u64, height: u64) -> String {
    format!(
        "the page is {width} × {height} pixels ({} million), past MAX_OCR_PIXELS ({} million), \
         so it was not read; scale it down and send it again",
        width.saturating_mul(height) / 1_000_000,
        MAX_OCR_PIXELS / 1_000_000
    )
}

/// A page of `width` × `height` pixels has pixels, and no more than
/// [`MAX_OCR_PIXELS`].
pub fn within(width: u64, height: u64) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "the page is {width} × {height} pixels: nothing to read"
        ));
    }
    if width.saturating_mul(height) > MAX_OCR_PIXELS {
        return Err(too_large(width, height));
    }
    Ok(())
}

/// A PGM as `pdftoppm -gray` writes it: `P5`, width, height and a maxval
/// of at most 255, separated by white space (comments allowed), one white
/// space, then `width × height` bytes.
pub fn parse_pgm(data: impl Into<Bytes>) -> Result<PageImage, String> {
    let data: Bytes = data.into();
    let bad = |why: &str| format!("the page image could not be read ({why})");
    if !data.starts_with(b"P5") {
        return Err(bad("not a grey PGM"));
    }
    let mut at = 2;
    let mut fields = [0u64; 3];
    for f in &mut fields {
        loop {
            match data.get(at) {
                Some(b) if b.is_ascii_whitespace() => at += 1,
                Some(b'#') => {
                    while data.get(at).is_some_and(|b| *b != b'\n') {
                        at += 1;
                    }
                }
                _ => break,
            }
        }
        let start = at;
        while data.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
            if at - start > 12 {
                return Err(bad("its header has no number where one belongs"));
            }
        }
        *f = std::str::from_utf8(&data[start..at])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("its header has no number where one belongs"))?;
    }
    let [width, height, maxval] = fields;
    if !data.get(at).is_some_and(u8::is_ascii_whitespace) {
        return Err(bad("its header does not end"));
    }
    at += 1;
    if maxval == 0 || maxval > 255 {
        return Err(bad("not one byte a pixel"));
    }
    within(width, height)?;
    let need = (width * height) as usize;
    if data.len() - at < need {
        return Err(bad("it has fewer pixels than its header says"));
    }
    Ok(PageImage::Pgm {
        data,
        width: width as u32,
        height: height as u32,
        offset: at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(img: &PageImage) -> (u32, u32, Vec<u8>) {
        match img {
            PageImage::Pgm {
                data,
                width,
                height,
                offset,
            } => (*width, *height, data[*offset..].to_vec()),
            PageImage::File(_) => panic!("not a PGM"),
        }
    }

    #[test]
    fn a_pgm_reads_and_turns_clockwise() {
        // 3 × 2: a b c / d e f
        let img = parse_pgm(b"P5\n# made by a test\n3 2\n255\nabcdef".to_vec()).unwrap();
        assert_eq!(pixels(&img), (3, 2, b"abcdef".to_vec()));
        assert_eq!(
            pixels(&img.clone().rotated(90).unwrap()),
            (2, 3, b"daebfc".to_vec())
        );
        assert_eq!(
            pixels(&img.clone().rotated(180).unwrap()),
            (3, 2, b"fedcba".to_vec())
        );
        assert_eq!(
            pixels(&img.clone().rotated(270).unwrap()),
            (2, 3, b"cfbead".to_vec())
        );
        assert!(img.rotated(45).is_err());
    }

    #[test]
    fn hostile_pgm_headers_are_refused_by_name() {
        for (bytes, says) in [
            (&b"P6\n1 1\n255\nx"[..], "not a grey PGM"),
            (b"P5\n1 1\n65535\nxx", "not one byte a pixel"),
            (b"P5\n4 4\n255\nshort", "fewer pixels"),
            (b"P5\nx 1\n255\n", "no number"),
            (b"P5\n0 10\n255\n", "nothing to read"),
            (b"P5\n30000 30000\n255\n", "MAX_OCR_PIXELS"),
            (b"P5\n99999999999999 1\n255\n", "no number"),
        ] {
            let e = parse_pgm(bytes.to_vec()).unwrap_err();
            assert!(e.contains(says), "{e}");
        }
    }

    /// Only what leptonica recognises is handed to tesseract as a file:
    /// anything else it would read as a list of file names to open.
    #[test]
    fn only_a_picture_leptonica_knows_reaches_tesseract_as_a_file() {
        for ok in [
            &b"\x89PNG\r\n\x1a\n...."[..],
            b"\xFF\xD8\xFF\xE0..",
            b"GIF89a..",
            b"RIFF\x10\0\0\0WEBPVP8 ",
        ] {
            assert!(PageImage::file(Bytes::from_static(ok)).is_ok(), "{ok:?}");
        }
        for bad in [
            &b"/etc/passwd\n/home/example/.ssh/id_ed25519\n"[..],
            b"II*\0\x08\0\0\0",
            b"RIFF\x10\0\0\0WAVEfmt ",
            b"BM6\0\0\0",
            b"",
        ] {
            let e = PageImage::file(Bytes::from_static(bad)).unwrap_err();
            assert!(e.contains("not given to tesseract"), "{e}");
        }
        let file = PageImage::file(Bytes::from_static(b"GIF89a..")).unwrap();
        assert!(file.rotated(90).unwrap_err().contains("decoded before"));
    }
}
