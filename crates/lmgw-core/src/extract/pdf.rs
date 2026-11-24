//! PDF text and page images through poppler's `pdftotext` and `pdftoppm`,
//! external binaries (poppler-utils; the RPM recommends it, so it can be
//! missing: a run then fails with [`PdfError::ToolMissing`], which names the
//! package, and nothing checks for it at start). Ported from
//! folder-chat's `pdf.rs`, which explains why an external tool: it is the
//! engine every Linux desktop viewer uses, a malformed PDF crashes a child
//! process rather than the gateway, and nothing is written to disk — the PDF
//! goes to the tool's stdin and the result comes back on stdout, so **no path
//! is ever handed to poppler**.
//!
//! Every run goes through the conversion's shared runner
//! ([`super::convert::run`]): a slot of the tool pool, a process group of its
//! own, bounded by [`PDF_TIMEOUT`] and killed when it passes (or when the
//! calling future is dropped). Its stdout is read as it arrives, against
//! the same named budget as the office formats
//! ([`super::office::MAX_UNCOMPRESSED_BYTES`]): a hostile PDF's text can be far
//! larger than the PDF, and a run that passes it is killed and refused by name.
//!
//! - [`text`] → [`PdfText`]: `pdftotext -layout -enc UTF-8 - -`, pages split
//!   on form feeds (1-based; a trailing form feed closes the last page rather
//!   than opening another), lossy UTF-8.
//! - [`PdfText::classify`]: every page has text ([`PdfClass::Text`]), none has
//!   ([`PdfClass::Scanned`]), or some do ([`PdfClass::Hybrid`]).
//! - [`render_page`]: `pdftoppm -r 150 -png -singlefile -f N -l N -`, one PNG.
//! - [`geometry`]: the two runs document conversion reads a PDF's layout
//!   from (`pdftotext -bbox-layout`, `pdftohtml -xml`). Chat attachments and
//!   knowledge bases read [`text`] until they resolve through the conversion's
//!   capability table too (document-conversion design WP7).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;

use super::convert::run::RunError;
use super::office::MAX_UNCOMPRESSED_BYTES;

pub mod geometry;
pub mod limit;
pub mod square;
pub mod wide;
pub(crate) use limit::reading;
pub use limit::{
    piped_read, poppler, At, Limit, PopplerTool, PDFTOHTML_IMAGES,
    POPPLER_IMAGE_MEMORY_LIMIT_BYTES, POPPLER_PIPE_READ_PER_BYTE, POPPLER_TEXT_MEMORY_LIMIT_BYTES,
    POPPLER_WIDE_MEMORY_LIMIT_BYTES,
};
#[cfg(all(test, unix))]
mod tests_limit;

/// The text extractor, looked up on `PATH`.
pub const PDFTOTEXT: &str = "pdftotext";

/// The page renderer, looked up on `PATH`.
pub const PDFTOPPM: &str = "pdftoppm";

/// How long one poppler run may take before it is killed and the PDF is
/// refused with [`PdfError::Timeout`], which names this constant.
///
/// Five minutes. Poppler extracts ordinary documents at hundreds of pages a
/// second, so even a several-thousand-page book finishes in well under a
/// minute; a run still going after five is a pathological or hostile file (a
/// decompression bomb, a content stream that loops). Generous on purpose: a
/// false timeout costs a document, an over-long wait costs minutes.
pub const PDF_TIMEOUT: Duration = Duration::from_secs(300);

/// The resolution a page is rendered at for a vision model (`pdftoppm -r`).
///
/// 150 dpi: measured in folder-chat on 2026-09-24, an A4 page came to about
/// 1000 image tokens for Gemma 4 and read a scan near-perfectly and a table
/// cell for cell; more pixels cost tokens (and a llama-server `ubatch_size`
/// at least the image's tokens) without reading better.
pub const PAGE_DPI: u32 = 150;

const FORM_FEED: char = '\u{c}';

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PdfError {
    #[error(
        "pdftotext/pdftoppm is not installed — install poppler-utils (e.g. `sudo dnf install \
         poppler-utils`) to read PDFs"
    )]
    ToolMissing,
    #[error("{tool} failed: {message}")]
    Failed { tool: &'static str, message: String },
    #[error(
        "{tool} did not finish within {} s (PDF_TIMEOUT) and was stopped; a PDF that takes this \
         long is usually malformed",
        PDF_TIMEOUT.as_secs()
    )]
    Timeout { tool: &'static str },
    #[error(
        "{tool} produced more than MAX_UNCOMPRESSED_BYTES ({}) of output and was stopped",
        crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
    )]
    OutputTooLarge { tool: &'static str },
    #[error("{}", oom_said(tool, *limit, *piped, *retry))]
    OutOfMemory {
        tool: &'static str,
        limit: Limit,
        piped: bool,
        retry: Retry,
    },
    #[error("pdftoppm finished but wrote no image (is page {0} in the document?)")]
    EmptyPage(u32),
    /// lmgw's reader, which starts poppler, is not there.
    #[error("{}", super::convert::run::start::no_reader())]
    NoReader,
    /// lmgw's reader could not confine poppler or set its limits
    /// (`RunError::NotStarted`): lmgw's own failure, not the file's.
    #[error("{0}")]
    NotStarted(String),
}

/// Whether a run that ran out of memory was run once more under
/// [`POPPLER_WIDE_MEMORY_LIMIT_BYTES`] ([`wide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Not run again: the wide run itself, or a step that is not run
    /// again (the picture gate's runs, whose failure only costs it its
    /// choice of pages).
    No,
    /// Its own limit (this) was not enough and it was run again under the
    /// wide one, which it passed too.
    Widened(Limit),
    /// Not run again: the file's deadline left no time for it.
    Deadline,
}

/// [`PdfError::OutOfMemory`] in words: the limit it passed, a pipe's share,
/// and whether it was run again.
fn oom_said(tool: &str, limit: Limit, piped: bool, retry: Retry) -> String {
    let piped = if piped {
        format!(
            " (and POPPLER_PIPE_READ_PER_BYTE ({POPPLER_PIPE_READ_PER_BYTE}) per byte of the \
             file, which came through a pipe)"
        )
    } else {
        String::new()
    };
    let retry = match retry {
        Retry::No => String::new(),
        Retry::Widened(from) => format!(" (run again under it after {})", from.said()),
        Retry::Deadline => format!(
            "; it was not run again under {}, as the file's deadline left no time",
            Limit::Wide.said()
        ),
    };
    format!(
        "{tool} needed more memory than {}{piped} and was stopped{retry}; the PDF is probably \
         malformed, declares an enormous page or holds an enormous image",
        limit.said()
    )
}

/// A poppler run's end, in poppler's words: `at` says the tool, its limit
/// and whether its file was piped.
pub(crate) fn into_pdf(e: RunError, at: At) -> PdfError {
    let tool = at.tool.name();
    match e {
        RunError::Missing => PdfError::ToolMissing,
        RunError::Failed(message) => PdfError::Failed { tool, message },
        RunError::Timeout => PdfError::Timeout { tool },
        RunError::TooLarge | RunError::FileTooLarge(_) => PdfError::OutputTooLarge { tool },
        RunError::OutOfMemory => PdfError::OutOfMemory {
            tool,
            limit: at.limit,
            piped: at.piped,
            retry: Retry::No,
        },
        RunError::NoReader => PdfError::NoReader,
        RunError::NotStarted(m) => PdfError::NotStarted(m),
    }
}

/// The text of a PDF, page by page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfText {
    /// `pages[0]` is page 1. A page without text is an empty (or blank) string.
    pub pages: Vec<String>,
    /// The 1-based numbers of the pages that have no text.
    pub textless: Vec<u32>,
    /// What its run said of its memory: run again under the wide limit
    /// ([`wide::Note`]).
    pub notes: Vec<wide::Note>,
}

/// What kind of PDF it is, by its pages' text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfClass {
    /// Every page has text.
    Text,
    /// No page has text (a scan), or there are no pages.
    Scanned,
    /// Some pages have text and some do not.
    Hybrid,
}

impl PdfText {
    /// Split `pdftotext`'s output into pages.
    pub fn from_raw(raw: &str) -> Self {
        let pages: Vec<String> = if raw.is_empty() {
            Vec::new()
        } else {
            raw.strip_suffix(FORM_FEED)
                .unwrap_or(raw)
                .split(FORM_FEED)
                .map(str::to_string)
                .collect()
        };
        let textless = pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.trim().is_empty())
            .map(|(i, _)| i as u32 + 1)
            .collect();
        Self {
            pages,
            textless,
            notes: Vec::new(),
        }
    }

    pub fn classify(&self) -> PdfClass {
        if self.textless.is_empty() && !self.pages.is_empty() {
            PdfClass::Text
        } else if self.textless.len() == self.pages.len() {
            PdfClass::Scanned
        } else {
            PdfClass::Hybrid
        }
    }

    /// The text pages, each under a `--- page N ---` line (the chat's
    /// `<file kind="pdf">` block); text-less pages are left out, for the
    /// caller to send as images or to note.
    pub fn marked(&self) -> String {
        let mut out = String::new();
        for (i, p) in self.pages.iter().enumerate() {
            if p.trim().is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("--- page {} ---\n{}\n", i + 1, p.trim_end()));
        }
        out.truncate(out.trim_end().len());
        out
    }
}

/// Extract the text of the PDF in `bytes`.
pub async fn text(bytes: Bytes) -> Result<PdfText, PdfError> {
    text_with(Path::new(PDFTOTEXT), bytes, PDF_TIMEOUT).await
}

/// [`text`] with the binary and the timeout named, so a test can prove what
/// a missing tool or a slow one turns into.
pub async fn text_with(bin: &Path, bytes: Bytes, timeout: Duration) -> Result<PdfText, PdfError> {
    let args = ["-layout", "-enc", "UTF-8"].map(String::from).to_vec();
    let call = wide::Call::new(
        PopplerTool::Text,
        bin,
        args,
        &["-"],
        wide::Src::Bytes(bytes),
    );
    let got = wide::own(&call, timeout).await?;
    let mut text = PdfText::from_raw(&String::from_utf8_lossy(&got.out));
    text.notes = got.notes;
    Ok(text)
}

/// A page rendered for Chat or a knowledge base: its PNG, and what its run
/// said of its memory ([`wide::Note`]).
#[derive(Debug)]
pub struct Rendered {
    pub png: Vec<u8>,
    pub notes: Vec<wide::Note>,
}

/// Render page `page` (1-based) of the PDF in `bytes` as one PNG at
/// [`PAGE_DPI`].
pub async fn render_page(bytes: Bytes, page: u32) -> Result<Rendered, PdfError> {
    render_page_with(Path::new(PDFTOPPM), bytes, page, PAGE_DPI, PDF_TIMEOUT).await
}

/// [`render_page`] with the binary, resolution and timeout named.
pub async fn render_page_with(
    bin: &Path,
    bytes: Bytes,
    page: u32,
    dpi: u32,
    timeout: Duration,
) -> Result<Rendered, PdfError> {
    let (dpi, n) = (dpi.to_string(), page.to_string());
    let args = ["-r", &dpi, "-png", "-singlefile", "-f", &n, "-l", &n].map(String::from);
    let mut call = wide::Call::new(
        PopplerTool::Render,
        bin,
        args.to_vec(),
        &[],
        wide::Src::Bytes(bytes),
    );
    call.span = Some((page, page));
    let got = wide::own(&call, timeout).await?;
    if got.out.is_empty() {
        return Err(PdfError::EmptyPage(page));
    }
    Ok(Rendered {
        png: got.out,
        notes: got.notes,
    })
}

/// Whether both poppler tools can be run here — what tests that need them
/// gate on.
pub async fn available() -> bool {
    for tool in [PDFTOTEXT, PDFTOPPM] {
        let ran = tokio::process::Command::new(tool)
            .arg("-v")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if ran.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::test_files;

    #[test]
    fn pages_split_on_form_feeds_and_a_trailing_one_closes_the_last() {
        let t = PdfText::from_raw("one\u{c}two\u{c}");
        assert_eq!(t.pages, ["one", "two"]);
        assert!(t.textless.is_empty());
        assert_eq!(t.classify(), PdfClass::Text);
        // Without the trailing feed the last page is still a page.
        assert_eq!(PdfText::from_raw("one\u{c}two").pages, ["one", "two"]);
        // A blank page in the middle.
        let t = PdfText::from_raw("one\u{c}  \n\u{c}three\u{c}");
        assert_eq!(t.pages.len(), 3);
        assert_eq!(t.textless, [2]);
        assert_eq!(t.classify(), PdfClass::Hybrid);
    }

    #[test]
    fn classification_covers_scanned_and_empty() {
        let t = PdfText::from_raw("\u{c}\u{c}");
        assert_eq!(t.pages.len(), 2);
        assert_eq!(t.textless, [1, 2]);
        assert_eq!(t.classify(), PdfClass::Scanned);
        let t = PdfText::from_raw("");
        assert!(t.pages.is_empty());
        assert_eq!(t.classify(), PdfClass::Scanned);
        // pdftotext's output for one blank page.
        assert_eq!(PdfText::from_raw("\u{c}").textless, [1]);
    }

    #[test]
    fn marked_text_numbers_pages_and_skips_the_textless_ones() {
        let t = PdfText::from_raw("first\n\u{c}\u{c}third  \n\n\u{c}");
        assert_eq!(t.marked(), "--- page 1 ---\nfirst\n\n--- page 3 ---\nthird");
        assert_eq!(PdfText::from_raw("").marked(), "");
    }

    // Unix's tools (sh, yes, cat, sleep, false) under poppler's address-space
    // limit; Windows refuses every run (a_run_is_refused_on_windows).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_tool_is_a_visible_error_naming_poppler_utils() {
        let missing = Path::new("pdftotext-that-is-not-installed");
        let e = text_with(missing, Bytes::from_static(b"%PDF"), PDF_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(e, PdfError::ToolMissing);
        assert!(e.to_string().contains("poppler-utils"), "{e}");
        let e = render_page_with(
            Path::new("pdftoppm-that-is-not-installed"),
            Bytes::from_static(b"%PDF"),
            1,
            PAGE_DPI,
            PDF_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert_eq!(e, PdfError::ToolMissing);
    }

    /// The runner's ends in poppler's words: each names the constant that
    /// bounded it (the runner's own tests stop real runs, `convert/run`).
    #[test]
    fn a_stopped_run_names_the_constant_that_stopped_it() {
        let text = At {
            tool: PopplerTool::Text,
            limit: Limit::Text,
            piped: true,
        };
        let e = into_pdf(RunError::Timeout, text);
        assert_eq!(e, PdfError::Timeout { tool: "pdftotext" });
        let s = e.to_string();
        assert!(s.contains("PDF_TIMEOUT") && s.contains("300 s"), "{s}");
        let e = into_pdf(RunError::TooLarge, text);
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
        let render = At {
            tool: PopplerTool::Render,
            limit: Limit::Image,
            piped: false,
        };
        let s = into_pdf(RunError::OutOfMemory, render).to_string();
        assert!(
            s.contains("POPPLER_IMAGE_MEMORY_LIMIT_BYTES (2.0 GiB)"),
            "{s}"
        );
        // A spooled file has no pipe's share to say.
        assert!(!s.contains("POPPLER_PIPE_READ_PER_BYTE"), "{s}");
        let s = into_pdf(RunError::OutOfMemory, text).to_string();
        assert!(
            s.contains("POPPLER_TEXT_MEMORY_LIMIT_BYTES (512 MiB)"),
            "{s}"
        );
        assert!(s.contains("POPPLER_PIPE_READ_PER_BYTE (3)"), "{s}");
        // Run again under the wide limit and stopped there too, or not run
        // again for the deadline: both limits named.
        let wide = PdfError::OutOfMemory {
            tool: "pdftotext",
            limit: Limit::Wide,
            piped: false,
            retry: Retry::Widened(Limit::Text),
        };
        let s = wide.to_string();
        assert!(
            s.contains("more memory than POPPLER_WIDE_MEMORY_LIMIT_BYTES (4.0 GiB)")
                && s.contains("after POPPLER_TEXT_MEMORY_LIMIT_BYTES (512 MiB)"),
            "{s}"
        );
        let late = PdfError::OutOfMemory {
            tool: "pdftoppm",
            limit: Limit::Image,
            piped: false,
            retry: Retry::Deadline,
        };
        let s = late.to_string();
        assert!(
            s.contains("POPPLER_IMAGE_MEMORY_LIMIT_BYTES")
                && s.contains("not run again under POPPLER_WIDE_MEMORY_LIMIT_BYTES")
                && s.contains("deadline"),
            "{s}"
        );
    }

    // Unix's tools (sh, yes, cat, sleep, false) under poppler's address-space
    // limit; Windows refuses every run (a_run_is_refused_on_windows).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_tool_reports_its_exit_status() {
        let e = text_with(Path::new("false"), Bytes::new(), PDF_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            matches!(
                &e,
                PdfError::Failed {
                    tool: "pdftotext",
                    message,
                } if message.contains("exit status")
            ),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn real_pdfs_split_into_pages_and_classify() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        let text_pdf = test_files::pdf(&[Some("Hello page one"), Some("Second (page)")]);
        let t = text(Bytes::from(text_pdf)).await.unwrap();
        assert_eq!(t.pages.len(), 2, "{t:?}");
        assert!(t.pages[0].contains("Hello page one"), "{t:?}");
        assert!(t.pages[1].contains("Second (page)"), "{t:?}");
        assert_eq!(t.classify(), PdfClass::Text);
        assert!(
            t.marked().starts_with("--- page 1 ---\nHello page one"),
            "{}",
            t.marked()
        );

        let hybrid = test_files::pdf(&[Some("Has text"), None, Some("Also text")]);
        let t = text(Bytes::from(hybrid)).await.unwrap();
        assert_eq!(t.pages.len(), 3, "{t:?}");
        assert_eq!(t.textless, [2]);
        assert_eq!(t.classify(), PdfClass::Hybrid);

        let scanned = test_files::pdf(&[None, None]);
        let t = text(Bytes::from(scanned)).await.unwrap();
        assert_eq!(t.classify(), PdfClass::Scanned);
        assert_eq!(t.textless, [1, 2]);
    }

    #[tokio::test]
    async fn a_broken_pdf_is_a_failure_in_poppler_s_words() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        let e = text(Bytes::from_static(b"%PDF-1.4 this is not a pdf"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                PdfError::Failed {
                    tool: "pdftotext",
                    ..
                }
            ),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn one_page_renders_as_a_png_and_a_missing_page_is_an_error() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        let pdf = Bytes::from(test_files::pdf(&[Some("Hello"), Some("World")]));
        let png = render_page(pdf.clone(), 2).await.unwrap().png;
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
        // 300x200 pt at 150 dpi.
        let (w, h) = (
            u32::from_be_bytes(png[16..20].try_into().unwrap()),
            u32::from_be_bytes(png[20..24].try_into().unwrap()),
        );
        assert_eq!((w, h), (625, 417));
        // Page 9 of a two-page document: poppler writes nothing.
        let e = render_page(pdf, 9).await.unwrap_err();
        assert!(
            matches!(e, PdfError::EmptyPage(9) | PdfError::Failed { .. }),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn a_page_that_needs_more_than_the_memory_limit_is_stopped_and_named() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        // 300x200 pt at 5 000 dpi is a 20 800 x 13 900 pixel page (~870 MB).
        let pdf = Bytes::from(test_files::pdf(&[Some("Hello")]));
        let bounds = super::super::convert::run::Bounds {
            memory: 512 << 20,
            ..poppler(Limit::Image, PDF_TIMEOUT)
        };
        let e = super::super::convert::run::piped(
            Path::new(PDFTOPPM),
            &[
                "-r",
                "20000",
                "-png",
                "-singlefile",
                "-f",
                "1",
                "-l",
                "1",
                "-",
            ],
            pdf,
            bounds,
        )
        .await
        .map_err(|e| {
            let at = At {
                tool: PopplerTool::Render,
                limit: Limit::Image,
                piped: true,
            };
            into_pdf(e, at)
        })
        .unwrap_err();
        assert!(
            matches!(
                e,
                PdfError::OutOfMemory {
                    tool: "pdftoppm",
                    ..
                }
            ),
            "{e}"
        );
        assert!(
            e.to_string().contains("POPPLER_IMAGE_MEMORY_LIMIT_BYTES"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn ordinary_large_documents_fit_under_the_memory_limit() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        // A 300-page text book, and a 30-page scan rendered at 150 dpi.
        let words = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        let book: Vec<Option<&str>> = (0..300).map(|_| Some(words.as_str())).collect();
        let t = text(Bytes::from(test_files::pdf(&book))).await.unwrap();
        assert_eq!(t.pages.len(), 300);
        assert_eq!(t.classify(), PdfClass::Text);
        let scan = Bytes::from(test_files::pdf(&vec![None; 30]));
        for page in [1, 15, 30] {
            let png = render_page(scan.clone(), page).await.unwrap().png;
            assert!(png.starts_with(b"\x89PNG"), "page {page}");
        }
    }

    /// On Windows poppler is not run without its memory limit, which is not
    /// set there yet (Windows design §3.1): every run, a missing tool's
    /// included, is refused in those words before anything is started.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_run_is_refused_on_windows() {
        let e = text_with(
            Path::new("pdftotext-that-is-not-installed"),
            Bytes::from_static(b"%PDF"),
            PDF_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&e, PdfError::Failed { tool: "pdftotext", message }
                if message.contains("not supported on Windows yet")),
            "{e:?}"
        );
        let e = render_page_with(
            Path::new("pdftoppm-that-is-not-installed"),
            Bytes::from_static(b"%PDF"),
            1,
            PAGE_DPI,
            PDF_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&e, PdfError::Failed { tool: "pdftoppm", message }
                if message.contains("not supported on Windows yet")),
            "{e:?}"
        );
    }
}
