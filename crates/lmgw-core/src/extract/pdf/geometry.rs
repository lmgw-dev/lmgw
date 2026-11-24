//! The two poppler runs a PDF's layout is read from (document-conversion
//! design §5.1), each through the shared runner with the bounds of its tool
//! ([`super::PDF_TIMEOUT`], [`MAX_UNCOMPRESSED_BYTES`] of output and
//! [`super::POPPLER_TEXT_MEMORY_LIMIT_BYTES`]; a picture run, which decodes
//! every image, [`super::POPPLER_IMAGE_MEMORY_LIMIT_BYTES`]; each once
//! more under the wide limit where its caller runs it again,
//! [`super::wide`]), on a slot of the tool pool the caller holds: a file
//! takes one slot for both runs and the layout read from their output, so
//! the output is only ever held while its slot is (`pool::charge::PDF_FILE`
//! counts it):
//!
//! - [`bbox_layout_with`]: `pdftotext -bbox-layout`, pages, blocks, lines
//!   and words with their boxes (about 17 times the size of the plain text);
//! - [`font_runs_with`]: `pdftohtml -xml -stdout -i -zoom 1 -noroundcoord`,
//!   text runs with font size and weight ([`image_runs_at`]: the same
//!   with every image drawn, for picture export, in runs of its own). Reading
//!   stdin it needs an output name, and with `-stdout -i` it writes no file (checked with poppler
//!   26.08 on 2026-11-18); it runs in a private empty directory anyway.
//!
//! Each reads the file from its spool ([`Spool`]: `/dev/stdin` on the
//! spool file, read by position, or a pipe where there is no spool), so no
//! run is sent the whole file.
//!
//! Both take the request's page span (`-f`/`-l`), so a request for pages
//! 3-7 of a long book reads those. poppler refuses a first page past the
//! last; [`last_page_of`] reads the page count from that refusal.
//!
//! [`MAX_UNCOMPRESSED_BYTES`]: super::MAX_UNCOMPRESSED_BYTES

use std::path::Path;
use std::time::Duration;

use super::super::convert::ocr::spool::Spool;
use super::super::convert::pool::Slot;
use super::wide::{self, Call, Ran, Src};
use super::{Limit, PdfError, PopplerTool};

/// The layout's page converter, looked up on `PATH` beside `pdftotext`.
pub const PDFTOHTML: &str = "pdftohtml";

fn span_args(span: Option<(u32, u32)>) -> Vec<String> {
    match span {
        Some((first, last)) => vec![
            "-f".into(),
            first.to_string(),
            "-l".into(),
            last.to_string(),
        ],
        None => Vec::new(),
    }
}

/// `pdftotext -bbox-layout` of `src`, pages `span` (all without one).
pub async fn bbox_layout_with(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    timeout: Duration,
) -> Result<Vec<u8>, PdfError> {
    Ok(bbox_layout_at(slot, bin, src, span, Limit::Text, timeout)
        .await?
        .out)
}

/// [`bbox_layout_with`] under `limit`.
pub async fn bbox_layout_at(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    limit: Limit,
    timeout: Duration,
) -> Result<Ran, PdfError> {
    let mut args = vec!["-bbox-layout".to_string(), "-enc".into(), "UTF-8".into()];
    args.extend(span_args(span));
    let call = Call::new(PopplerTool::Text, bin, args, &["-"], Src::Spool(src));
    wide::once(slot, &call, limit, timeout).await
}

/// `pdftohtml -xml` of `src`, pages `span`, in a private empty directory.
pub async fn font_runs_with(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    timeout: Duration,
) -> Result<Vec<u8>, PdfError> {
    Ok(font_runs_at(slot, bin, src, span, Limit::Text, timeout)
        .await?
        .out)
}

/// [`font_runs_with`] under `limit`.
pub async fn font_runs_at(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    limit: Limit,
    timeout: Duration,
) -> Result<Ran, PdfError> {
    let output = super::MAX_UNCOMPRESSED_BYTES;
    html_run(slot, bin, src, span, (limit, timeout, output), false).await
}

/// `pdftohtml -xml -dataurls` of `src`, pages `span`, under `limit`,
/// `within` its time and stdout's bound: every image drawn, each an
/// `<image top left width height src="data:…">` element (picture export,
/// design §5.3), inline on stdout and bounded like the rest of it, so no
/// file is written (without `-dataurls` pdftohtml writes each image into
/// its working directory; checked with poppler 26.08 on 2026-11-18). It
/// encodes every image as PNG, so it is far slower than [`font_runs_with`]
/// on pages with large images: picture export runs it apart from the text,
/// on the pages that need it.
pub async fn image_runs_at(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    limit: Limit,
    within: wide::Within,
) -> Result<Ran, PdfError> {
    let (timeout, output) = (within.timeout, within.output);
    html_run(slot, bin, src, span, (limit, timeout, output), true).await
}

async fn html_run(
    slot: &Slot,
    bin: &Path,
    src: &Spool,
    span: Option<(u32, u32)>,
    (limit, timeout, output): (Limit, Duration, u64),
    images: bool,
) -> Result<Ran, PdfError> {
    let tool = if images {
        PopplerTool::Pictures
    } else {
        PopplerTool::Fonts
    };
    let dir = src
        .private_dir(crate::extract::convert::libreoffice::scratch::POPPLER_PREFIX)
        .map_err(|e| PdfError::Failed {
            tool: tool.name(),
            message: format!("could not make its private directory: {e}"),
        })?;
    let images_arg = if images { "-dataurls" } else { "-i" };
    let mut args: Vec<String> = [
        "-xml",
        "-stdout",
        images_arg,
        "-zoom",
        "1",
        "-noroundcoord",
        "-enc",
        "UTF-8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(span_args(span));
    // The private directory is its working directory only: with `-stdout`
    // it writes no file, so its rules give it no write there.
    let mut call = Call::new(tool, bin, args, &["out"], Src::Spool(src));
    call.dir = Some(dir.path());
    call.output = output;
    call.span = span;
    wide::once(slot, &call, limit, timeout).await
}

/// The page count in poppler's refusal of a first page past the last one
/// ("… the first page (9) can not be after the last page (3)").
pub fn last_page_of(e: &PdfError) -> Option<u32> {
    let PdfError::Failed { message, .. } = e else {
        return None;
    };
    let at = message.find("the last page (")? + "the last page (".len();
    let rest = &message[at..];
    rest[..rest.find(')')?].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::extract::pdf::{available, PDFTOTEXT, PDF_TIMEOUT};
    #[cfg(unix)]
    use crate::extract::test_files;

    #[test]
    fn the_page_count_is_read_from_poppler_s_refusal() {
        let e = PdfError::Failed {
            tool: "pdftotext",
            message: "Command Line Error: Wrong page range given: the first page (9) can not \
                      be after the last page (3). (exit status: 99)"
                .into(),
        };
        assert_eq!(last_page_of(&e), Some(3));
        assert_eq!(last_page_of(&PdfError::ToolMissing), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn both_runs_read_the_span_and_a_first_page_past_the_end_names_the_count() {
        if !available().await {
            eprintln!("skipped: poppler is not on PATH");
            return;
        }
        let pdf = bytes::Bytes::from(test_files::pdf(&[Some("One"), Some("Two"), Some("Three")]));
        let pdf = Spool::of(None, pdf).await;
        let slot = crate::extract::convert::pool::global().acquire().await;
        let slot = &slot;
        let xml = bbox_layout_with(slot, Path::new(PDFTOTEXT), &pdf, Some((2, 9)), PDF_TIMEOUT)
            .await
            .unwrap();
        let xml = String::from_utf8_lossy(&xml);
        assert_eq!(xml.matches("<page ").count(), 2, "{xml}");
        assert!(xml.contains(">Two<") && !xml.contains(">One<"), "{xml}");
        let runs = font_runs_with(slot, Path::new(PDFTOHTML), &pdf, Some((3, 3)), PDF_TIMEOUT)
            .await
            .unwrap();
        let runs = String::from_utf8_lossy(&runs);
        assert!(runs.contains("<page number=\"3\""), "{runs}");
        let e = bbox_layout_with(slot, Path::new(PDFTOTEXT), &pdf, Some((9, 12)), PDF_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(last_page_of(&e), Some(3), "{e}");
    }
}
