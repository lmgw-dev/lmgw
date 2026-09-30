//! PDF text and page images through poppler's `pdftotext` and `pdftoppm`,
//! external binaries (poppler-utils; the RPM depends on it). Ported from
//! folder-chat's `pdf.rs`, which explains why an external tool: it is the
//! engine every Linux desktop viewer uses, a malformed PDF crashes a child
//! process rather than the gateway, and nothing is written to disk — the PDF
//! goes to the tool's stdin and the result comes back on stdout, so **no path
//! is ever handed to poppler**.
//!
//! Every run is bounded by [`PDF_TIMEOUT`] and killed when it passes (or when
//! the calling future is dropped). Its stdout is read as it arrives, against
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

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;

use super::office::MAX_UNCOMPRESSED_BYTES;

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

/// How many lines of a failed run's stderr go into the error message (the rest
/// is counted as `(… N more lines)`, or `(… more)` when stderr was longer than
/// the 64 KiB kept and the count is unknown). Display only: the run's outcome never
/// depends on it, and stderr is always drained.
pub const STDERR_LINES_SHOWN: usize = 20;

/// The address space one poppler child may use (`RLIMIT_AS`): 4 GiB.
///
/// A hostile PDF can ask for more memory than the machine has (a page
/// declared 100 000 pt wide rendered at 150 dpi, a stream that inflates
/// without end); with no limit the child grows until the kernel's OOM killer
/// picks a victim, and that may be the gateway or the desktop. The limit makes
/// the child fail alone, and the refusal names this constant
/// ([`PdfError::OutOfMemory`]). Ordinary documents are far below it: a page
/// at 150 dpi is under 10 MB of pixels, an A0 poster about 100 MB, and
/// pdftotext of a several-hundred-page book stays under a few hundred MB; the
/// rest of the 4 GiB is the address space poppler's own libraries map.
pub const POPPLER_MEMORY_LIMIT_BYTES: u64 = 4 << 30;

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
        "{tool} produced more than MAX_UNCOMPRESSED_BYTES ({} MiB) of output and was stopped",
        MAX_UNCOMPRESSED_BYTES >> 20
    )]
    OutputTooLarge { tool: &'static str },
    #[error(
        "{tool} needed more than POPPLER_MEMORY_LIMIT_BYTES ({} GiB) of memory and was stopped; \
         the PDF is probably malformed or declares an enormous page",
        POPPLER_MEMORY_LIMIT_BYTES >> 30
    )]
    OutOfMemory { tool: &'static str },
    #[error("pdftoppm finished but wrote no image (is page {0} in the document?)")]
    EmptyPage(u32),
}

/// How a piped poppler run ended, before it is worded for its tool.
enum RunError {
    Missing,
    Failed(String),
    Timeout,
    TooLarge,
    OutOfMemory,
}

impl RunError {
    fn into_pdf(self, tool: &'static str) -> PdfError {
        match self {
            Self::Missing => PdfError::ToolMissing,
            Self::Failed(message) => PdfError::Failed { tool, message },
            Self::Timeout => PdfError::Timeout { tool },
            Self::TooLarge => PdfError::OutputTooLarge { tool },
            Self::OutOfMemory => PdfError::OutOfMemory { tool },
        }
    }
}

/// The text of a PDF, page by page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfText {
    /// `pages[0]` is page 1. A page without text is an empty (or blank) string.
    pub pages: Vec<String>,
    /// The 1-based numbers of the pages that have no text.
    pub textless: Vec<u32>,
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
        Self { pages, textless }
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
    let raw = run_piped(bin, &["-layout", "-enc", "UTF-8", "-", "-"], bytes, timeout)
        .await
        .map_err(|e| e.into_pdf("pdftotext"))?;
    Ok(PdfText::from_raw(&String::from_utf8_lossy(&raw)))
}

/// Render page `page` (1-based) of the PDF in `bytes` as one PNG at
/// [`PAGE_DPI`].
pub async fn render_page(bytes: Bytes, page: u32) -> Result<Vec<u8>, PdfError> {
    render_page_with(Path::new(PDFTOPPM), bytes, page, PAGE_DPI, PDF_TIMEOUT).await
}

/// [`render_page`] with the binary, resolution and timeout named.
pub async fn render_page_with(
    bin: &Path,
    bytes: Bytes,
    page: u32,
    dpi: u32,
    timeout: Duration,
) -> Result<Vec<u8>, PdfError> {
    let (dpi, n) = (dpi.to_string(), page.to_string());
    let args = ["-r", &dpi, "-png", "-singlefile", "-f", &n, "-l", &n, "-"];
    let png = run_piped(bin, &args, bytes, timeout)
        .await
        .map_err(|e| e.into_pdf("pdftoppm"))?;
    if png.is_empty() {
        return Err(PdfError::EmptyPage(page));
    }
    Ok(png)
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

/// `bin args…` with `bytes` on stdin, stdout back undecoded, killed after
/// `timeout` (or when the future is dropped) or when stdout passes
/// [`MAX_UNCOMPRESSED_BYTES`], and limited to [`POPPLER_MEMORY_LIMIT_BYTES`]
/// of address space.
async fn run_piped(
    bin: &Path,
    args: &[&str],
    bytes: Bytes,
    timeout: Duration,
) -> Result<Vec<u8>, RunError> {
    run_piped_within(
        bin,
        args,
        bytes,
        timeout,
        MAX_UNCOMPRESSED_BYTES,
        POPPLER_MEMORY_LIMIT_BYTES,
    )
    .await
}

/// [`run_piped`] with the stdout limit and the child's address-space limit
/// named (tests use small ones).
async fn run_piped_within(
    bin: &Path,
    args: &[&str],
    bytes: Bytes,
    timeout: Duration,
    limit: u64,
    mem_limit: u64,
) -> Result<Vec<u8>, RunError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let name = bin.display().to_string();
    let mut cmd = tokio::process::Command::new(bin);
    // SAFETY: the hook runs in the forked child before exec and calls only
    // `setrlimit`, which is async-signal-safe; it allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            let lim = libc::rlimit {
                rlim_cur: mem_limit as libc::rlim_t,
                rlim_max: mem_limit as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timeout, or a caller that is dropped mid-file, must not leave the
        // tool behind.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RunError::Missing
            } else {
                RunError::Failed(format!("could not run {name}: {e}"))
            }
        })?;
    let pipe = |what: &str| RunError::Failed(format!("{name}'s {what} was not piped"));
    let mut stdin = child.stdin.take().ok_or_else(|| pipe("stdin"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| pipe("stdout"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| pipe("stderr"))?;
    // Written from its own task while the output is read: a PDF larger than
    // the pipe buffer would otherwise deadlock against the tool's output. A
    // write that fails because the tool gave up early is not the error worth
    // reporting — its exit status and stderr are.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    });
    // stderr is drained to the end (so the tool never blocks on it) but only
    // its first bytes are kept, for the message.
    let err_reader = tokio::spawn(async move {
        let mut kept = Vec::new();
        let mut cut = false;
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            if kept.len() < STDERR_KEEP_BYTES {
                kept.extend_from_slice(&chunk[..n]);
            } else {
                cut = true;
            }
        }
        // A chunk that crossed the bound is kept whole; only what came after
        // it is lost.
        (kept, cut)
    });
    // stdout is read as it comes, so a run that produces more than `limit` is
    // stopped when it crosses it, not after it has filled memory. Dropping
    // this future on a timeout drops the child, which `kill_on_drop` kills.
    let work = async {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = stdout
                .read(&mut chunk)
                .await
                .map_err(|e| RunError::Failed(format!("{name}: {e}")))?;
            if n == 0 {
                break;
            }
            if (out.len() + n) as u64 > limit {
                return Err(RunError::TooLarge);
            }
            out.extend_from_slice(&chunk[..n]);
        }
        let status = child
            .wait()
            .await
            .map_err(|e| RunError::Failed(format!("{name}: {e}")))?;
        Ok((out, status))
    };
    let (out, status) = match tokio::time::timeout(timeout, work).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            writer.abort();
            err_reader.abort();
            return Err(e);
        }
        Err(_) => {
            writer.abort();
            err_reader.abort();
            return Err(RunError::Timeout);
        }
    };
    let _ = writer.await;
    // Poppler reports a failed allocation on stderr and can still exit 0 with
    // a stub (a 1x1 image for a page that did not fit), so stderr is read for
    // a success too; the pipe closes when the child exits.
    let (stderr, cut) = err_reader.await.unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    if ran_out_of_memory(&status, &stderr) {
        return Err(RunError::OutOfMemory);
    }
    if !status.success() {
        return Err(RunError::Failed(format!(
            "{} ({status})",
            first_lines(&stderr, !cut)
        )));
    }
    Ok(out)
}

/// How much of a run's stderr is kept to be shown: 64 KiB. Display only.
const STDERR_KEEP_BYTES: usize = 64 << 10;

/// Did the child die of the address-space limit? A failed allocation shows
/// as poppler's own out-of-memory message, a C++ `bad_alloc`, or the abort or
/// segfault that follows one.
fn ran_out_of_memory(status: &std::process::ExitStatus, stderr: &str) -> bool {
    use std::os::unix::process::ExitStatusExt;
    let s = stderr.to_ascii_lowercase();
    let says = s.contains("bad_alloc") || s.contains("out of memory") || s.contains("memory alloc");
    says || (matches!(status.signal(), Some(libc::SIGABRT | libc::SIGSEGV)) && s.contains("memory"))
}

/// The first [`STDERR_LINES_SHOWN`] lines of `text`, trimmed, with `(… N more
/// lines)` when there were more — or `(… more)` when `complete` is false: the
/// text is only what was kept of a longer stderr, so a count would be wrong.
fn first_lines(text: &str, complete: bool) -> String {
    let text = text.trim();
    let total = text.lines().count();
    let mut shown: Vec<&str> = text.lines().take(STDERR_LINES_SHOWN).collect();
    let more = if complete {
        format!(
            "(… {} more lines)",
            total.saturating_sub(STDERR_LINES_SHOWN)
        )
    } else {
        "(… more)".to_string()
    };
    if total > STDERR_LINES_SHOWN || !complete {
        shown.push(&more);
    }
    shown.join("\n")
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

    #[tokio::test]
    async fn a_run_that_overstays_is_killed_and_the_error_names_the_constant() {
        // `sleep` ignores stdin and outlives a 100 ms budget.
        let e = run_piped(
            Path::new("sleep"),
            &["30"],
            Bytes::new(),
            Duration::from_millis(100),
        )
        .await;
        let e = e.map_err(|e| e.into_pdf("pdftotext")).unwrap_err();
        assert_eq!(e, PdfError::Timeout { tool: "pdftotext" });
        let s = e.to_string();
        assert!(s.contains("PDF_TIMEOUT") && s.contains("300 s"), "{s}");
    }

    #[tokio::test]
    async fn a_large_input_does_not_deadlock_the_pipe() {
        // `cat` echoes stdin to stdout: 4 MiB is far past both pipe buffers,
        // so a writer that shared the reader's task would hang.
        let data = Bytes::from(vec![b'x'; 4 << 20]);
        let out = run_piped(Path::new("cat"), &[], data.clone(), Duration::from_secs(30))
            .await
            .ok()
            .unwrap();
        assert_eq!(out.len(), data.len());
    }

    #[tokio::test]
    async fn a_failing_tool_reports_its_exit_status() {
        let e = run_piped(
            Path::new("false"),
            &[],
            Bytes::new(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| e.into_pdf("pdftotext"))
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
        let png = render_page(pdf.clone(), 2).await.unwrap();
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
    async fn output_past_the_limit_is_stopped_and_named() {
        // `yes` writes forever; the run is killed when stdout crosses the limit.
        let started = std::time::Instant::now();
        let e = run_piped_within(
            Path::new("yes"),
            &[],
            Bytes::new(),
            PDF_TIMEOUT,
            1 << 20,
            POPPLER_MEMORY_LIMIT_BYTES,
        )
        .await
        .map_err(|e| e.into_pdf("pdftotext"))
        .unwrap_err();
        assert_eq!(e, PdfError::OutputTooLarge { tool: "pdftotext" });
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_stderr_longer_than_what_is_kept_says_more_without_a_count() {
        // 400 KB of stderr against the 64 KiB kept: the count would be a lie.
        let script = "yes 'a line of complaint text' | head -c 400000 >&2; exit 3";
        let Err(RunError::Failed(m)) =
            run_piped(Path::new("sh"), &["-c", script], Bytes::new(), PDF_TIMEOUT).await
        else {
            panic!("expected a failed run");
        };
        assert!(m.contains("\n(… more) ("), "{m}");
        assert!(!m.contains("more lines"), "{m}");
    }

    #[tokio::test]
    async fn a_page_that_needs_more_than_the_memory_limit_is_stopped_and_named() {
        if !available().await {
            eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
            return;
        }
        // 300x200 pt at 5 000 dpi is a 20 800 x 13 900 pixel page (~870 MB).
        let pdf = Bytes::from(test_files::pdf(&[Some("Hello")]));
        let e = run_piped_within(
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
            PDF_TIMEOUT,
            MAX_UNCOMPRESSED_BYTES,
            512 << 20,
        )
        .await
        .map_err(|e| e.into_pdf("pdftoppm"))
        .unwrap_err();
        assert_eq!(e, PdfError::OutOfMemory { tool: "pdftoppm" }, "{e}");
        assert!(e.to_string().contains("POPPLER_MEMORY_LIMIT_BYTES"), "{e}");
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
            let png = render_page(scan.clone(), page).await.unwrap();
            assert!(png.starts_with(b"\x89PNG"), "page {page}");
        }
    }

    #[tokio::test]
    async fn a_long_stderr_is_shown_as_its_first_lines_and_a_count() {
        let script = "for i in $(seq 1 100); do echo line$i >&2; done; exit 3";
        let Err(RunError::Failed(m)) =
            run_piped(Path::new("sh"), &["-c", script], Bytes::new(), PDF_TIMEOUT).await
        else {
            panic!("expected a failed run");
        };
        assert!(m.starts_with("line1\nline2\n"), "{m}");
        assert!(m.contains("line20\n(… 80 more lines) ("), "{m}");
        assert!(!m.contains("line21"), "{m}");
    }
}
