//! Whether stb_image decodes an image's bytes (llama egress design §8.2):
//! png, jpeg, gif and bmp, each walked the way stb_image v2.30 reads it.
//!
//! llama-server hands a tool image's bytes to `stbi_load_from_memory`
//! (`tools/mtmd/mtmd-helper.cpp:390`, read at b062ba735; its
//! `vendor/stb/stb_image.h` is v2.30, unchanged since 53f925074 of
//! 2025-05-30, and built with no `STBI_ONLY_*`/`STBI_NO_*`). An image it
//! cannot decode fails the whole request, so a tool loop that works with the
//! placeholder would fail on every iteration once the image went (decision
//! 18). Each format's check here walks the whole file once and fails on
//! every structural condition stb_image fails on, or that leaves it nothing
//! to decode, and names it.
//!
//! **Conservative, never lenient.** Where stb_image is more forgiving than
//! its own format (a JPEG cut off before its EOI decodes as far as it goes,
//! a BMP short of its pixels reads zeros, a GIF needs no trailer), the
//! check asks for the whole file: a placeholder costs a tool loop nothing,
//! a refused decode costs it the request. What it cannot see without
//! decoding (a deflate stream's or an entropy-coded segment's contents, an
//! LZW code past the first) is not checked; a file whose structure is whole
//! and whose compressed body is corrupt still fails at the server.
//!
//! The reasons are short phrases (`"it ends before its IEND chunk"`); the
//! caller says which format and that llama.cpp cannot decode it.

mod bmp;
mod gif;
mod jpeg;
mod png;

#[cfg(test)]
mod tests;

use super::ImageFormat;

/// Whether stb_image decodes `bytes` as `format`, or why not. `Ok` for webp,
/// which stb_image does not read (its check is the caller's).
pub(super) fn check(format: ImageFormat, bytes: &[u8]) -> Result<(), String> {
    match format {
        ImageFormat::Png => png::check(bytes),
        ImageFormat::Jpeg => jpeg::check(bytes),
        ImageFormat::Gif => gif::check(bytes),
        ImageFormat::Bmp => bmp::check(bytes),
        ImageFormat::Webp => Ok(()),
    }
}

/// stb_image's `STBI_MAX_DIMENSIONS`: no side may be larger.
const MAX_DIMENSION: u64 = 1 << 24;

/// stb_image's `stbi__mad3sizes_valid(a, b, c, 0)`: `a * b * c` with no
/// intermediate product over `INT_MAX`.
fn product_fits(a: u64, b: u64, c: u64) -> bool {
    let max = i32::MAX as u64;
    a.checked_mul(b).is_some_and(|ab| ab <= max)
        && (a * b).checked_mul(c).is_some_and(|abc| abc <= max)
}

/// A cursor over the image's bytes. Every read is strict: running past the
/// end is an error naming what was being read, never stb_image's zeros.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn at_end(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// The next `n` bytes, or the error that the data ends inside `what`.
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let Some(end) = end else {
            return Err(format!("it ends inside its {what}"));
        };
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self, what: &str) -> Result<u8, String> {
        Ok(self.take(1, what)?[0])
    }

    fn be16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn le16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn be32(&mut self, what: &str) -> Result<u32, String> {
        let b = self.take(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn le32(&mut self, what: &str) -> Result<u32, String> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}
