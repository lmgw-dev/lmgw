//! pdftoppm's stub (review 3 of the poppler limit): a page bitmap past the
//! size poppler allocates at all ("Bogus memory allocation size", measured
//! 2026-11-20 on poppler 26.08.0: a 300 × 200 pt page at 20 000 dpi, under
//! 512 MiB and under 4 GiB alike) is written as a 1 × 1 image and the run
//! exits 0. That line alone is no failure (a damaged object's size is
//! skipped and the rest drawn), so the runner reads it as the page not
//! rendered only with the stub on stdout, and words it as out of memory,
//! which it was before: run again wide, then refused naming both limits.

/// Whether `out` is a 1 × 1 PNG or PNM (pdftoppm's `-png` or its default
/// `-gray` / colour output).
pub(super) fn is_stub(out: &[u8]) -> bool {
    if out.starts_with(b"\x89PNG\r\n\x1a\n") {
        return out.get(12..16) == Some(b"IHDR")
            && out.get(16..20) == Some(&1u32.to_be_bytes()[..])
            && out.get(20..24) == Some(&1u32.to_be_bytes()[..]);
    }
    if matches!(out.get(..2), Some(b"P4" | b"P5" | b"P6")) {
        let mut words = out[2..]
            .split(|b| b.is_ascii_whitespace())
            .filter(|w| !w.is_empty());
        return words.next() == Some(b"1") && words.next() == Some(b"1");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_stub;

    #[test]
    fn a_one_pixel_png_or_pnm_is_the_stub() {
        // pdftoppm 26.08.0's stubs, header bytes as written.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
        assert!(is_stub(&png));
        assert!(is_stub(b"P5\n1 1\n255\n\xff"));
        let mut wide = png.clone();
        wide[19] = 2;
        assert!(!is_stub(&wide));
        assert!(!is_stub(b"P5\n1 12\n255\n"));
        assert!(!is_stub(b"P6\n11 1\n255\n"));
        assert!(!is_stub(b"<doc>"));
    }
}
