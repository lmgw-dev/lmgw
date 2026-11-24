//! What a model that looks at pages needs of poppler (the layout model,
//! `convert/layout_model`), each run with the bounds of its tool
//! (`super::limit`), each run once more under the wide limit where it
//! needs it (`super::wide`):
//!
//! - [`pages_with_text_with`]: which pages of a span have words, by one
//!   plain `pdftotext` run over the span, on a slot of its own;
//! - [`pages_square_into`]: a run of pages rendered to squares of pixels,
//!   `pdftoppm -scale-to-x N -scale-to-y N -scale-dimension-before-rotation
//!   -f A -l B - p`, each page stretched to `N` × `N` as it is displayed
//!   (a page with `/Rotate` 90 or 270 too: the flag makes poppler scale the
//!   page before it turns it, so the square holds the whole turned page),
//!   read from the file's spool (`/dev/stdin`), written as binary PPMs into a
//!   private folder the caller reads them
//!   back from ([`rendered`]), one file per page, on a slot the caller
//!   holds. Each file is exactly `N` × `N` × 3 bytes and a header, so the
//!   caller bounds the folder by the pages it asks for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::convert::ocr::spool::Spool;
use super::super::convert::pool::Slot;
use super::wide::{self, Call, Ran, Src};
use super::{Limit, PdfError, PdfText, PopplerTool};

/// For each page of `src` poppler reads in `span` (all pages without
/// one), from the span's first, whether it has words. A span that starts
/// past the last page is poppler's refusal, as in the geometry runs.
pub async fn pages_with_text_with(
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    timeout: Duration,
) -> Result<Vec<bool>, PdfError> {
    let mut args = vec!["-enc".to_string(), "UTF-8".into()];
    if let Some((first, last)) = span {
        args.extend([
            "-f".into(),
            first.to_string(),
            "-l".into(),
            last.to_string(),
        ]);
    }
    let mut call = Call::new(PopplerTool::Text, bin, args, &["-"], Src::Spool(src));
    call.span = span;
    let got = wide::own(&call, timeout).await?;
    let text = PdfText::from_raw(&String::from_utf8_lossy(&got.out));
    Ok(text.pages.iter().map(|p| !p.trim().is_empty()).collect())
}

/// The name the rendered pages start with in their folder.
const ROOT: &str = "p";

/// The run that renders pages `first..=last` (1-based) of `src`, each as
/// a `side` × `side` PPM in `dir` (module doc).
pub fn square_call<'a>(
    bin: &'a Path,
    src: &'a Spool,
    (first, last): (u32, u32),
    side: u32,
    dir: &'a Path,
) -> Call<'a> {
    let side = side.to_string();
    let args = [
        "-scale-to-x",
        &side,
        "-scale-to-y",
        &side,
        "-scale-dimension-before-rotation",
        "-f",
        &first.to_string(),
        "-l",
        &last.to_string(),
    ]
    .map(String::from);
    let mut call = Call::new(
        PopplerTool::Render,
        bin,
        args.to_vec(),
        &[ROOT],
        Src::Spool(src),
    );
    call.dir = Some(dir);
    call.writes = true;
    call.span = Some((first, last));
    call
}

/// Pages `first..=last` (1-based) of `src`, each as a `side` × `side`
/// PPM in `dir` (module doc), on `slot` under the image limit. A failed run
/// may have written some of them: [`rendered`] says which.
#[allow(clippy::too_many_arguments)]
pub async fn pages_square_into(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    first: u32,
    last: u32,
    side: u32,
    dir: &Path,
    timeout: Duration,
) -> Result<Ran, PdfError> {
    let call = square_call(bin, src, (first, last), side, dir);
    wide::once(slot, &call, Limit::Image, timeout).await
}

/// The pages [`pages_square_into`] wrote into `dir`, by their number
/// (`p-7.ppm`, or `p-007.ppm` in a file of hundreds of pages): regular
/// files only, never a link (its type is the entry's own, not followed;
/// the reader opens each with [`open_rendered`]).
pub fn rendered(dir: &Path) -> std::io::Result<BTreeMap<u32, PathBuf>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let number = name
            .to_str()
            .and_then(|n| n.strip_prefix(ROOT)?.strip_prefix('-'))
            .and_then(|n| n.strip_suffix(".ppm"))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(n) = number {
            out.insert(n, entry.path());
        }
    }
    Ok(out)
}

/// The most bytes a `side` × `side` binary PPM takes: its header
/// (`P6\n<side> <side>\n255\n`) and three bytes a pixel.
pub fn ppm_max_bytes(side: u32) -> u64 {
    let header = format!("P6\n{side} {side}\n255\n").len() as u64;
    header + u64::from(side) * u64::from(side) * 3
}

/// A page [`rendered`] found, a `side` × `side` square, read without
/// following a link (`crate::child::beneath`; security round B, L3) and no
/// further than such a square can be ([`ppm_max_bytes`]; the review of the
/// round, S3): a process of the run that outlived it could still be
/// writing the file, so its size is not trusted.
pub fn open_rendered(page: &Path, side: u32) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let (Some(dir), Some(name)) = (page.parent(), page.file_name()) else {
        return Err(std::io::Error::other("no page file named"));
    };
    let max = ppm_max_bytes(side);
    let mut out = Vec::new();
    crate::child::beneath::open(dir, Path::new(name))?
        .take(max + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(std::io::Error::other(format!(
            "it is larger than a {side} × {side} page picture can be ({max} bytes)"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use crate::extract::pdf::{available, PDFTOPPM, PDFTOTEXT, PDF_TIMEOUT};
    #[cfg(unix)]
    use crate::extract::test_files;
    #[cfg(unix)]
    use bytes::Bytes;

    /// The PPM's pixels (after its `P6\n<side> <side>\n255\n` header).
    #[cfg(unix)]
    fn pixels(ppm: &[u8], side: usize) -> &[u8] {
        let header = format!("P6\n{side} {side}\n255\n");
        assert!(ppm.starts_with(header.as_bytes()), "{:?}", &ppm[..16]);
        let px = &ppm[header.len()..];
        assert_eq!(px.len(), side * side * 3);
        px
    }

    /// The leftmost and the rightmost column with a dark pixel.
    #[cfg(unix)]
    fn ink_columns(px: &[u8], side: usize) -> (usize, usize) {
        let (mut lo, mut hi) = (side, 0);
        for (i, p) in px.as_chunks::<3>().0.iter().enumerate() {
            if p.iter().map(|&c| u32::from(c)).sum::<u32>() < 3 * 200 {
                lo = lo.min(i % side);
                hi = hi.max(i % side);
            }
        }
        (lo, hi)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_of_pages_comes_back_as_squares_in_the_folder() {
        if !available().await {
            eprintln!("skipped: poppler is not on PATH");
            return;
        }
        let pdf = Bytes::from(test_files::pdf(&[Some("One"), Some("Two"), Some("Three")]));
        let dir = tempfile::tempdir().unwrap();
        // Read from a spool file (`/dev/stdin`), as a conversion does.
        let spools = tempfile::tempdir().unwrap();
        let pdf = Spool::of(Some(spools.path()), pdf).await;
        assert!(matches!(
            pdf.input(),
            crate::extract::convert::run::Input::File(_)
        ));
        let slot = crate::extract::convert::pool::global().acquire().await;
        let bin = Path::new(PDFTOPPM);
        pages_square_into(&slot, bin, &pdf, 2, 3, 64, dir.path(), PDF_TIMEOUT)
            .await
            .unwrap();
        let got = rendered(dir.path()).unwrap();
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
        for p in got.values() {
            let ppm = std::fs::read(p).unwrap();
            pixels(&ppm, 64);
        }
        let e = pages_square_into(&slot, bin, &pdf, 9, 9, 64, dir.path(), PDF_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(e, PdfError::Failed { .. }), "{e}");
    }

    /// A Letter page turned by `/Rotate` 90 and 270: the whole displayed
    /// page is in the square, its last word at the right edge included.
    /// Without `-scale-dimension-before-rotation` poppler scales by the
    /// unturned size and the right of the page is cut off.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turned_page_is_rendered_whole() {
        if !available().await {
            eprintln!("skipped: poppler is not on PATH");
            return;
        }
        for rotate in [90, 270] {
            // A word near the displayed page's right edge: the displayed
            // page is 792 wide, the word ends about 40 pt before it.
            let pdf = test_files::pdf_turned(612.0, 792.0, rotate, &[(700.0, 300.0, 14.0, "Edge")]);
            let dir = tempfile::tempdir().unwrap();
            let slot = crate::extract::convert::pool::global().acquire().await;
            pages_square_into(
                &slot,
                Path::new(PDFTOPPM),
                &Spool::of(None, Bytes::from(pdf)).await,
                1,
                1,
                320,
                dir.path(),
                PDF_TIMEOUT,
            )
            .await
            .unwrap();
            let file = rendered(dir.path()).unwrap().remove(&1).unwrap();
            let ppm = std::fs::read(file).unwrap();
            let (lo, hi) = ink_columns(pixels(&ppm, 320), 320);
            // 700 / 792 of 320 is column 283.
            assert!(
                (278..=287).contains(&lo),
                "rotate {rotate}: ink from column {lo}"
            );
            assert!(hi > lo && hi < 319, "rotate {rotate}: ink to column {hi}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_pages_of_a_span_that_have_words_are_told_apart() {
        if !available().await {
            eprintln!("skipped: poppler is not on PATH");
            return;
        }
        let pdf = Bytes::from(test_files::pdf(&[Some("One"), None, Some("Three"), None]));
        let pdf = &Spool::of(None, pdf).await;
        let bin = Path::new(PDFTOTEXT);
        let all = pages_with_text_with(bin, pdf, None, PDF_TIMEOUT).await;
        assert_eq!(all.unwrap(), vec![true, false, true, false]);
        let span = pages_with_text_with(bin, pdf, Some((2, 3)), PDF_TIMEOUT).await;
        assert_eq!(span.unwrap(), vec![false, true]);
        // A span that runs past the end is cut at the last page.
        let past = pages_with_text_with(bin, pdf, Some((3, 9)), PDF_TIMEOUT).await;
        assert_eq!(past.unwrap(), vec![true, false]);
        let beyond = pages_with_text_with(bin, pdf, Some((7, 9)), PDF_TIMEOUT).await;
        assert!(matches!(beyond, Err(PdfError::Failed { .. })), "{beyond:?}");
    }

    /// Security round B, L3: a link among the pages is not a page.
    #[cfg(unix)]
    #[test]
    fn a_link_among_the_rendered_pages_is_skipped_and_never_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p-1.ppm"), b"P6").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("p-2.ppm")).unwrap();
        let got = rendered(dir.path()).unwrap();
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(open_rendered(&got[&1], 4).unwrap(), b"P6");
        assert!(open_rendered(&dir.path().join("p-2.ppm"), 4).is_err());
        // The review of round B, S3: no more than a square can be is read.
        assert_eq!(ppm_max_bytes(4), 11 + 48);
        std::fs::write(dir.path().join("p-3.ppm"), vec![0u8; 60]).unwrap();
        let e = open_rendered(&dir.path().join("p-3.ppm"), 4).unwrap_err();
        assert!(e.to_string().contains("(59 bytes)"), "{e}");
        std::fs::write(dir.path().join("p-3.ppm"), vec![0u8; 59]).unwrap();
        assert_eq!(
            open_rendered(&dir.path().join("p-3.ppm"), 4).unwrap().len(),
            59
        );
    }
}
