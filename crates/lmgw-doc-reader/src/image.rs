//! An image file's pages for text recognition (document-conversion design
//! §5.1): their sizes from the headers ([`info`]), and one page as grey
//! pixels, a PGM ([`page`]), through the `image` and `tiff` decoders.
//!
//! - A PNG, JPEG, GIF or WebP is one page. tesseract reads such a file
//!   itself; it is decoded here only when the gateway has to turn it.
//! - A TIFF is a chain of directories, one page each. [`info`] walks the
//!   chain once with one decoder and gives every page's directory offset;
//!   [`page`] is then asked for one page by that offset and goes straight
//!   to it ([`FirstIfd`]), so reading a file's pages one by one costs the
//!   chain once, not once per page.
//!
//! A page's size is checked against the request's pixel bound from its
//! header before a pixel is decoded, and the decoders get the same bound as
//! their allocation limit. What a header claims beyond that is this
//! process's problem (its memory limit), not the gateway's.

use std::io::{BufRead, Read, Seek, SeekFrom};

use lmgw_doc_reader::wire::{Failure, ImagePage, Pixels};

/// `pixels` (one byte each, row by row) as a PGM.
fn pgm(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let header = format!("P5\n{width} {height}\n255\n");
    let mut data = Vec::with_capacity(header.len() + pixels.len());
    data.extend_from_slice(header.as_bytes());
    data.extend_from_slice(pixels);
    data
}

fn unreadable(why: String) -> Failure {
    Failure::Unreadable(why)
}

fn within(width: u32, height: u32, max_pixels: u64) -> Result<(), Failure> {
    let (w, h) = (u64::from(width), u64::from(height));
    if w == 0 || h == 0 {
        return Err(unreadable(format!(
            "the page is {w} × {h} pixels: nothing to read"
        )));
    }
    if w.saturating_mul(h) > max_pixels {
        return Err(Failure::TooManyPixels {
            width: w,
            height: h,
        });
    }
    Ok(())
}

/// Whether the file starts as a TIFF (classic or BigTIFF, either byte order).
fn is_tiff<R: Read + Seek>(r: &mut R) -> std::io::Result<bool> {
    let mut magic = [0u8; 4];
    let got = r.read(&mut magic)?;
    r.seek(SeekFrom::Start(0))?;
    Ok(got == 4 && matches!(&magic, b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"))
}

/// A TIFF whose header names `ifd` as its first directory: the file as it
/// is, with those header bytes overlaid, so a decoder opens that page first.
struct FirstIfd<R> {
    inner: R,
    /// Where the overlay starts in the file, and its bytes.
    at: u64,
    patch: Vec<u8>,
}

impl<R: Read + Seek> FirstIfd<R> {
    fn new(mut inner: R, ifd: u64) -> Result<Self, Failure> {
        let mut head = [0u8; 4];
        inner
            .read_exact(&mut head)
            .and_then(|()| inner.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|e| unreadable(format!("the TIFF could not be read: {e}")))?;
        let little = &head[..2] == b"II";
        let (at, patch) = match (&head, little) {
            (b"II*\0" | b"MM\0*", _) => {
                let o = u32::try_from(ifd)
                    .map_err(|_| unreadable("the TIFF page's place is not in the file".into()))?;
                (
                    4,
                    if little {
                        o.to_le_bytes()
                    } else {
                        o.to_be_bytes()
                    }
                    .to_vec(),
                )
            }
            (b"II+\0" | b"MM\0+", _) => (
                8,
                if little {
                    ifd.to_le_bytes()
                } else {
                    ifd.to_be_bytes()
                }
                .to_vec(),
            ),
            _ => return Err(unreadable("the file is not a TIFF".into())),
        };
        Ok(Self { inner, at, patch })
    }
}

impl<R: Read + Seek> Read for FirstIfd<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = self.inner.stream_position()?;
        let n = self.inner.read(buf)?;
        let (from, to) = (
            start.max(self.at),
            (start + n as u64).min(self.at + self.patch.len() as u64),
        );
        if from < to {
            buf[(from - start) as usize..(to - start) as usize]
                .copy_from_slice(&self.patch[(from - self.at) as usize..(to - self.at) as usize]);
        }
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for FirstIfd<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// The file's pages by their headers: one for a picture, one per directory
/// for a TIFF (with where each directory is). No pixel is decoded.
pub fn info<R: BufRead + Seek>(mut r: R) -> Result<Vec<ImagePage>, Failure> {
    let tiff =
        is_tiff(&mut r).map_err(|e| unreadable(format!("the image could not be read: {e}")))?;
    if !tiff {
        let (width, height) = image::ImageReader::new(r)
            .with_guessed_format()
            .map_err(|e| unreadable(format!("the image could not be read: {e}")))?
            .into_dimensions()
            .map_err(|e| unreadable(format!("the image could not be read: {e}")))?;
        return Ok(vec![ImagePage {
            width,
            height,
            ifd: None,
        }]);
    }
    let mut d = tiff::decoder::Decoder::new(r)
        .map_err(|e| unreadable(format!("the TIFF could not be read: {e}")))?;
    let mut pages = Vec::new();
    loop {
        let n = pages.len() + 1;
        let bad =
            |e: tiff::TiffError| unreadable(format!("the TIFF's page {n} could not be read: {e}"));
        let (width, height) = d.dimensions().map_err(bad)?;
        let ifd = d
            .ifd_pointer()
            .ok_or_else(|| unreadable(format!("the TIFF's page {n} has no directory")))?;
        pages.push(ImagePage {
            width,
            height,
            ifd: Some(ifd.0),
        });
        if !d.more_images() {
            return Ok(pages);
        }
        let n = n + 1;
        d.next_image()
            .map_err(|e| unreadable(format!("the TIFF's page {n} could not be read: {e}")))?;
    }
}

/// One page as a grey PGM: the picture itself, or the TIFF page whose
/// directory is at `ifd`; refused from its header past `max_pixels`.
pub fn page<R: BufRead + Seek>(
    mut r: R,
    max_pixels: u64,
    ifd: Option<u64>,
) -> Result<(Pixels, Vec<u8>), Failure> {
    let tiff =
        is_tiff(&mut r).map_err(|e| unreadable(format!("the image could not be read: {e}")))?;
    match (tiff, ifd) {
        (true, ifd) => tiff_page(r, max_pixels, ifd),
        (false, Some(_)) => Err(unreadable("the file is not a TIFF".into())),
        (false, None) => picture(r, max_pixels),
    }
}

/// A PNG, JPEG, GIF or WebP decoded to grey.
fn picture<R: BufRead + Seek>(mut r: R, max_pixels: u64) -> Result<(Pixels, Vec<u8>), Failure> {
    let bad = |e: String| unreadable(format!("the image could not be read: {e}"));
    let (width, height) = image::ImageReader::new(&mut r)
        .with_guessed_format()
        .map_err(|e| bad(e.to_string()))?
        .into_dimensions()
        .map_err(|e| bad(e.to_string()))?;
    within(width, height, max_pixels)?;
    r.seek(SeekFrom::Start(0)).map_err(|e| bad(e.to_string()))?;
    let mut reader = image::ImageReader::new(r)
        .with_guessed_format()
        .map_err(|e| bad(e.to_string()))?;
    let mut limits = image::Limits::default();
    // RGBA at four bytes a pixel, and the grey copy.
    limits.max_alloc = Some(max_pixels.saturating_mul(5));
    reader.limits(limits);
    let grey = reader
        .decode()
        .map_err(|e| unreadable(format!("the image could not be decoded: {e}")))?
        .to_luma8();
    let (width, height) = grey.dimensions();
    let pixels = Pixels {
        width,
        height,
        dpi: None,
    };
    Ok((pixels, pgm(width, height, grey.as_raw())))
}

/// The TIFF page at `ifd` (the file's first without one) decoded to grey,
/// with its resolution in DPI when the file says.
fn tiff_page<R: BufRead + Seek>(
    r: R,
    max_pixels: u64,
    ifd: Option<u64>,
) -> Result<(Pixels, Vec<u8>), Failure> {
    use tiff::tags::{ResolutionUnit, Tag};
    let bad = |e: tiff::TiffError| unreadable(format!("the TIFF page could not be read: {e}"));
    let mut limits = tiff::decoder::Limits::default();
    // A page of the pixel bound at up to four 16-bit samples a pixel.
    limits.decoding_buffer_size =
        usize::try_from(max_pixels.saturating_mul(8)).unwrap_or(usize::MAX);
    let source: Box<dyn ReadSeek> = match ifd {
        Some(at) => Box::new(FirstIfd::new(r, at)?),
        None => Box::new(r),
    };
    let mut d = tiff::decoder::Decoder::new(source)
        .map(|d| d.with_limits(limits))
        .map_err(bad)?;
    let (width, height) = d.dimensions().map_err(bad)?;
    within(width, height, max_pixels)?;
    let color = d.colortype().map_err(bad)?;
    let dpi = d.get_tag_f64(Tag::XResolution).ok().and_then(|x| {
        let unit = d
            .find_tag_unsigned::<u16>(Tag::ResolutionUnit)
            .ok()
            .flatten();
        let per_inch = match unit.and_then(ResolutionUnit::from_u16) {
            Some(ResolutionUnit::Centimeter) => x * 2.54,
            Some(ResolutionUnit::None) => return None,
            _ => x,
        };
        (per_inch.is_finite() && (1.0..=10_000.0).contains(&per_inch)).then_some(per_inch as u32)
    });
    let img = d
        .read_image()
        .map_err(|e| unreadable(format!("the TIFF page could not be decoded: {e}")))?;
    let grey = to_grey(img, color, width as usize, height as usize).map_err(unreadable)?;
    let pixels = Pixels { width, height, dpi };
    Ok((pixels, pgm(width, height, &grey)))
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A decoded TIFF page as one grey byte a pixel.
fn to_grey(
    img: tiff::decoder::DecodingResult,
    color: tiff::ColorType,
    w: usize,
    h: usize,
) -> Result<Vec<u8>, String> {
    use tiff::decoder::DecodingResult as D;
    use tiff::ColorType as C;
    let short = || "the TIFF page has fewer pixels than its size says".to_string();
    let n = w * h;
    let (samples, bits): (usize, u8) = match color {
        C::Gray(b) => (1, b),
        C::GrayA(b) => (2, b),
        C::RGB(b) => (3, b),
        C::RGBA(b) => (4, b),
        other => {
            return Err(format!(
                "the TIFF page's colour type ({other:?}) is not read yet"
            ))
        }
    };
    // A pixel's grey from its first sample (grey) or its first three (RGB).
    let luma = |p: &[u32]| -> u8 {
        if samples < 3 {
            p[0] as u8
        } else {
            ((p[0] * 299 + p[1] * 587 + p[2] * 114) / 1000) as u8
        }
    };
    match (img, bits) {
        // Bilevel and 2/4-bit grey: rows packed, each padded to a byte.
        (D::U8(v), 1 | 2 | 4) if samples == 1 => {
            let per = 8 / bits as usize;
            let row = w.div_ceil(per);
            if v.len() < row * h {
                return Err(short());
            }
            let max = (1u32 << bits) - 1;
            let mut out = Vec::with_capacity(n);
            for y in 0..h {
                for x in 0..w {
                    let byte = v[y * row + x / per];
                    let shift = 8 - bits as usize * (x % per + 1);
                    let s = u32::from(byte >> shift) & max;
                    out.push((s * 255 / max) as u8);
                }
            }
            Ok(out)
        }
        (D::U8(v), 8) => {
            if v.len() < n * samples {
                return Err(short());
            }
            let mut px = [0u32; 4];
            Ok(v.chunks_exact(samples)
                .take(n)
                .map(|p| {
                    for (d, s) in px.iter_mut().zip(p) {
                        *d = u32::from(*s);
                    }
                    luma(&px)
                })
                .collect())
        }
        (D::U16(v), 16) => {
            if v.len() < n * samples {
                return Err(short());
            }
            let mut px = [0u32; 4];
            Ok(v.chunks_exact(samples)
                .take(n)
                .map(|p| {
                    for (d, s) in px.iter_mut().zip(p) {
                        *d = u32::from(*s >> 8);
                    }
                    luma(&px)
                })
                .collect())
        }
        (_, b) => Err(format!("the TIFF page's {b}-bit samples are not read yet")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// A classic little-endian TIFF of `pages` uncompressed 8-bit grey
    /// pages, `width` × 1 pixels, page `i` filled with the byte `i`.
    pub(crate) fn grey_tiff(pages: u32, width: u32) -> Vec<u8> {
        let mut f = b"II*\0".to_vec();
        f.extend_from_slice(&8u32.to_le_bytes());
        for i in 0..pages {
            let ifd_at = f.len() as u32;
            let entries: [(u16, u16, u32, u32); 8] = [
                (256, 4, 1, width),                   // ImageWidth
                (257, 4, 1, 1),                       // ImageLength
                (258, 3, 1, 8),                       // BitsPerSample
                (259, 3, 1, 1),                       // Compression: none
                (262, 3, 1, 1),                       // Photometric: black is zero
                (273, 4, 1, ifd_at + 2 + 8 * 12 + 4), // StripOffsets
                (278, 4, 1, 1),                       // RowsPerStrip
                (279, 4, 1, width),                   // StripByteCounts
            ];
            f.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for (tag, kind, count, value) in entries {
                f.extend_from_slice(&tag.to_le_bytes());
                f.extend_from_slice(&kind.to_le_bytes());
                f.extend_from_slice(&count.to_le_bytes());
                f.extend_from_slice(&value.to_le_bytes());
            }
            let next = if i + 1 == pages {
                0
            } else {
                f.len() as u32 + 4 + width
            };
            f.extend_from_slice(&next.to_le_bytes());
            f.extend(std::iter::repeat_n(i as u8, width as usize));
        }
        f
    }

    #[test]
    fn a_tiffs_pages_are_listed_once_and_read_by_their_place() {
        let file = grey_tiff(5, 3);
        let pages = info(Cursor::new(&file)).unwrap();
        assert_eq!(pages.len(), 5);
        assert!(pages.iter().all(|p| (p.width, p.height) == (3, 1)));
        for (i, p) in pages.iter().enumerate() {
            let (px, data) = page(Cursor::new(&file), 100, p.ifd).unwrap();
            assert_eq!((px.width, px.height), (3, 1));
            let mut want = b"P5\n3 1\n255\n".to_vec();
            want.extend([i as u8; 3]);
            assert_eq!(data, want, "page {i}");
        }
        // Without a place it is the first page; a place outside the file is
        // that page's failure.
        assert_eq!(
            page(Cursor::new(&file), 100, None).unwrap().1[11..],
            [0, 0, 0]
        );
        assert!(matches!(
            page(Cursor::new(&file), 100, Some(1 << 20)),
            Err(Failure::Unreadable(_))
        ));
        assert_eq!(
            page(Cursor::new(&file), 2, pages[1].ifd).unwrap_err(),
            Failure::TooManyPixels {
                width: 3,
                height: 1
            }
        );
    }

    /// 20,000 directories: the list is one walk (the per-page decoder of the
    /// first pass walked the chain from the start for every page).
    #[test]
    fn a_long_chain_of_directories_is_walked_once() {
        let file = grey_tiff(20_000, 1);
        let started = std::time::Instant::now();
        let pages = info(Cursor::new(&file)).unwrap();
        assert_eq!(pages.len(), 20_000);
        let last = pages.last().unwrap();
        let (_, data) = page(Cursor::new(&file), 100, last.ifd).unwrap();
        assert_eq!(data.last(), Some(&((20_000u32 - 1) as u8)));
        assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
    }

    /// A PNG whose header claims 30,000 × 30,000 pixels: its size is read,
    /// nothing is decoded, and a page of it is refused from the header.
    #[test]
    fn a_dimension_bomb_is_refused_from_its_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&30_000u32.to_be_bytes());
        png.extend_from_slice(&30_000u32.to_be_bytes());
        png.extend_from_slice(&[8, 0, 0, 0, 0]);
        let crc = crc32fast::hash(&png[12..29]);
        png.extend_from_slice(&crc.to_be_bytes());
        for chunk in [&b"IDAT"[..], b"IEND"] {
            png.extend_from_slice(&0u32.to_be_bytes());
            png.extend_from_slice(chunk);
            png.extend_from_slice(&crc32fast::hash(chunk).to_be_bytes());
        }
        let pages = info(Cursor::new(&png)).unwrap();
        assert_eq!((pages[0].width, pages[0].height), (30_000, 30_000));
        assert_eq!(
            page(Cursor::new(&png), 100_000_000, None).unwrap_err(),
            Failure::TooManyPixels {
                width: 30_000,
                height: 30_000
            }
        );
    }

    #[test]
    fn a_bilevel_tiff_row_unpacks_to_grey() {
        // 10 pixels wide: two bytes a row, the last six bits padding.
        let grey = to_grey(
            tiff::decoder::DecodingResult::U8(vec![0b1010_0000, 0b0100_0000]),
            tiff::ColorType::Gray(1),
            10,
            1,
        )
        .unwrap();
        assert_eq!(grey, [255, 0, 255, 0, 0, 0, 0, 0, 0, 255]);
        let e = to_grey(
            tiff::decoder::DecodingResult::U8(vec![0]),
            tiff::ColorType::Gray(1),
            10,
            2,
        )
        .unwrap_err();
        assert!(e.contains("fewer pixels"), "{e}");
    }
}
