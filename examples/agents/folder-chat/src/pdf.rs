//! PDF text through poppler's `pdftotext`, an external binary (poppler-utils
//! in the image; on the host for the tests).
//!
//! An external tool rather than a Rust PDF crate: poppler is what every Linux
//! desktop's viewer uses, so "the text the owner sees" and "the text we index"
//! come from the same engine, and a malformed PDF crashes a child process
//! rather than the agent. `-layout` keeps columns and tables in reading order;
//! `-enc UTF-8` makes the output decodable; `-` `-` reads the PDF from stdin
//! and writes the text to stdout, so nothing is ever written next to the
//! owner's file and **no path is ever handed to pdftotext**: the agent reads
//! the file itself, through [`crate::source::read_beneath`], and pipes those
//! bytes in — the same bytes the sync hashes, so the stored hash always
//! belongs to the text that was indexed. Pages come back separated by form
//! feeds, which the chunker turns into `page N` headings.
//!
//! Every run is bounded by [`PDF_EXTRACT_TIMEOUT`] and killed when it is
//! passed; the sync records that PDF as a `pdf_timeout` skip naming the
//! constant.
//!
//! **Page images** for the vision model ([`crate::vision`]) come from the same
//! poppler, through `pdftoppm` ([`render_page`]), the same way: the PDF's
//! bytes on stdin, one PNG on stdout, bounded by the same timeout, and no path
//! ever handed to the tool.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// The binary, looked up on `PATH`.
pub const PDFTOTEXT: &str = "pdftotext";

/// The page renderer, looked up on `PATH` (poppler-utils, like
/// [`PDFTOTEXT`]).
pub const PDFTOPPM: &str = "pdftoppm";

/// How long one `pdftotext` run may take before it is killed and the PDF is
/// skipped (`pdf_timeout`, the skip's text names this constant and its value).
///
/// Five minutes. Poppler extracts the text of ordinary documents at hundreds
/// of pages a second, so even a several-thousand-page book finishes in well
/// under a minute on a slow CPU; a run that is still going after five
/// minutes is a pathological or hostile file (a decompression bomb, a content
/// stream that loops), and waiting longer would only stall every sync at that
/// one file. It is generous on purpose: a false timeout costs a document,
/// while an over-long one costs only minutes of one sync. The same bound
/// applies to the MCP `read` tool and to `pdftotext -v`.
pub const PDF_EXTRACT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PdfError {
    #[error(
        "pdftotext is not installed (it comes with poppler-utils), so this PDF could not be \
         indexed; an earlier index of it, if there is one, is kept until it can be"
    )]
    ToolMissing,
    #[error("pdftotext failed: {0}")]
    Failed(String),
    #[error(
        "pdftotext did not finish within {} s (PDF_EXTRACT_TIMEOUT, {} s) and was stopped; a PDF \
         that takes this long is usually malformed",
        .after.as_secs_f64(),
        PDF_EXTRACT_TIMEOUT.as_secs()
    )]
    Timeout { after: Duration },
}

/// Why one page could not be rendered for the vision model. The sync records
/// the page as failed with this text, and reads it again on the next sync.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error(
        "pdftoppm is not installed (it comes with poppler-utils), so the page could not be \
         rendered for the vision model"
    )]
    ToolMissing,
    #[error("pdftoppm failed: {0}")]
    Failed(String),
    #[error(
        "pdftoppm did not render the page within {} s (PDF_EXTRACT_TIMEOUT, {} s) and was stopped",
        .after.as_secs_f64(),
        PDF_EXTRACT_TIMEOUT.as_secs()
    )]
    Timeout { after: Duration },
    #[error("pdftoppm finished but wrote no image")]
    Empty,
}

/// How a piped poppler run ended, before it is worded for its tool.
enum RunError {
    Missing,
    Failed(String),
    Timeout(Duration),
}

impl From<RunError> for PdfError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Missing => Self::ToolMissing,
            RunError::Failed(m) => Self::Failed(m),
            RunError::Timeout(after) => Self::Timeout { after },
        }
    }
}

impl From<RunError> for RenderError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Missing => Self::ToolMissing,
            RunError::Failed(m) => Self::Failed(m),
            RunError::Timeout(after) => Self::Timeout { after },
        }
    }
}

/// Extract the text of `path`, pages separated by form feeds.
///
/// **The agent never calls this**: it resolves `path` again, which is what
/// [`extract_bytes`] exists to avoid. It stays for a test that proves reading
/// by path and piping the bytes give the same text. `path` must be absolute:
/// a relative name starting with `-` would be read as an option.
pub async fn extract(path: &Path) -> Result<String, PdfError> {
    extract_with(PDFTOTEXT, path).await
}

/// [`extract`] with the binary named, so a test can prove what a missing one
/// turns into without emptying `PATH` under every other test in the process.
async fn extract_with(bin: &str, path: &Path) -> Result<String, PdfError> {
    debug_assert!(path.is_absolute(), "pdftotext needs an absolute path");
    let out = tokio::process::Command::new(bin)
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| spawn_error(bin, e))?;
    if !out.status.success() {
        return Err(failed(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn spawn_error(bin: &str, e: std::io::Error) -> PdfError {
    if e.kind() == std::io::ErrorKind::NotFound {
        PdfError::ToolMissing
    } else {
        PdfError::Failed(format!("could not run {bin}: {e}"))
    }
}

fn failed(out: &std::process::Output) -> PdfError {
    PdfError::Failed(failure_text(out))
}

/// A failed run in the tool's own words: its stderr and its exit status.
fn failure_text(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    format!("{} ({})", stderr.trim(), out.status)
}

/// Extract the text of a PDF held in memory, piped to `pdftotext` on stdin,
/// bounded by [`PDF_EXTRACT_TIMEOUT`] — what the MCP `read` tool uses. The
/// text is byte for byte what the sync indexed (the same engine, flags and
/// decoding: [`decode`]).
pub async fn extract_bytes(bytes: Vec<u8>) -> Result<String, PdfError> {
    let raw = extract_bytes_raw(Path::new(PDFTOTEXT), Arc::new(bytes), PDF_EXTRACT_TIMEOUT).await?;
    Ok(decode(raw))
}

/// pdftotext's raw output. `-enc UTF-8` promises UTF-8; a stray invalid byte
/// from a broken font map becomes U+FFFD rather than costing the whole
/// document. Linear in the output — the sync calls it from `spawn_blocking`.
pub fn decode(raw: Vec<u8>) -> String {
    match String::from_utf8(raw) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Run `bin` (normally [`PDFTOTEXT`]; a test points it at a script) over
/// `bytes` on stdin and return its stdout undecoded, killing it after
/// `timeout`. The child is awaited asynchronously — nothing here blocks a
/// runtime worker — and killed with the future if the sync is dropped.
///
/// The bytes are shared rather than handed over, so the sync can keep them for
/// [`render_page`] without a second copy of the file in memory.
pub async fn extract_bytes_raw(
    bin: &Path,
    bytes: Arc<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<u8>, PdfError> {
    Ok(run_piped(bin, &["-layout", "-enc", "UTF-8", "-", "-"], bytes, timeout).await?)
}

/// Render page `page` (1-based, as [`crate::chunk::pdf_pages`] numbers them)
/// of the PDF in `bytes` as one PNG at `dpi`, with `bin` (normally
/// [`PDFTOPPM`]): `pdftoppm -r <dpi> -png -singlefile -f N -l N -`, the bytes
/// on stdin and the image on stdout, killed after `timeout` like
/// [`extract_bytes_raw`].
pub async fn render_page(
    bin: &Path,
    bytes: Arc<Vec<u8>>,
    page: u32,
    dpi: u32,
    timeout: Duration,
) -> Result<Vec<u8>, RenderError> {
    let (dpi, page) = (dpi.to_string(), page.to_string());
    let args = [
        "-r",
        dpi.as_str(),
        "-png",
        "-singlefile",
        "-f",
        page.as_str(),
        "-l",
        page.as_str(),
        "-",
    ];
    let png = run_piped(bin, &args, bytes, timeout).await?;
    if png.is_empty() {
        return Err(RenderError::Empty);
    }
    Ok(png)
}

/// The piped run both tools share: `bin args…` with `bytes` on stdin, stdout
/// back undecoded, killed after `timeout` (or when the future is dropped).
async fn run_piped(
    bin: &Path,
    args: &[&str],
    bytes: Arc<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<u8>, RunError> {
    use tokio::io::AsyncWriteExt;
    let name = bin.display().to_string();
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timeout, or a sync that is dropped mid-file, must not leave the
        // tool behind.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match spawn_error(&name, e) {
            PdfError::ToolMissing => RunError::Missing,
            other => RunError::Failed(other.to_string()),
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RunError::Failed(format!("{name}'s stdin was not piped")))?;
    // Written from its own task while the output is read: a PDF larger than
    // the pipe buffer would otherwise deadlock against the tool's output. A
    // write that fails because the tool gave up early is not the error worth
    // reporting — its exit status and stderr are.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    });
    // Dropping `wait_with_output` on a timeout drops the child, which
    // `kill_on_drop` kills.
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| RunError::Failed(format!("{name}: {e}")))?,
        Err(_) => {
            writer.abort();
            return Err(RunError::Timeout(timeout));
        }
    };
    let _ = writer.await;
    if !out.status.success() {
        return Err(RunError::Failed(failure_text(&out)));
    }
    Ok(out.stdout)
}

/// `pdftotext -v`'s first line (poppler prints it on stderr), e.g.
/// `pdftotext version 24.02.0` — recorded with the index, so a poppler update
/// re-extracts the PDFs whose text it may have changed. `None` when the tool
/// cannot be run or says nothing within [`PDF_EXTRACT_TIMEOUT`].
pub async fn version(bin: &Path) -> Option<String> {
    let run = tokio::process::Command::new(bin)
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(PDF_EXTRACT_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    let text = [out.stderr, out.stdout]
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// Whether `pdftoppm` can be run here — what the page-image tests gate on.
pub async fn pdftoppm_available() -> bool {
    tokio::process::Command::new(PDFTOPPM)
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

/// Whether `pdftotext` can be run here — what the PDF test gates on.
pub async fn available() -> bool {
    tokio::process::Command::new(PDFTOTEXT)
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_binary_is_an_error_not_a_panic() {
        let e = extract_with(
            "pdftotext-that-is-not-installed",
            Path::new("/nonexistent.pdf"),
        )
        .await
        .unwrap_err();
        assert_eq!(e, PdfError::ToolMissing);
        assert!(e.to_string().contains("poppler-utils"));
        let missing = Path::new("pdftotext-that-is-not-installed");
        let e = extract_bytes_raw(missing, Arc::new(b"%PDF".to_vec()), PDF_EXTRACT_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(e, PdfError::ToolMissing);
        assert_eq!(version(missing).await, None);
        let e = render_page(
            Path::new("pdftoppm-that-is-not-installed"),
            Arc::new(b"%PDF".to_vec()),
            1,
            150,
            PDF_EXTRACT_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert_eq!(e, RenderError::ToolMissing);
        assert!(e.to_string().contains("poppler-utils"), "{e}");
    }

    #[test]
    fn a_timeout_names_the_constant_and_its_value() {
        let e = PdfError::Timeout {
            after: PDF_EXTRACT_TIMEOUT,
        };
        let s = e.to_string();
        assert!(
            s.contains("PDF_EXTRACT_TIMEOUT") && s.contains("300 s"),
            "{s}"
        );
        let s = RenderError::Timeout {
            after: PDF_EXTRACT_TIMEOUT,
        }
        .to_string();
        assert!(
            s.contains("PDF_EXTRACT_TIMEOUT") && s.contains("300 s"),
            "{s}"
        );
    }

    #[tokio::test]
    async fn a_broken_pdf_is_a_failure_with_poppler_s_words() {
        if !available().await {
            eprintln!("skipped: pdftotext is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("broken.pdf");
        std::fs::write(&p, b"not a pdf at all").unwrap();
        assert!(matches!(extract(&p).await, Err(PdfError::Failed(_))));
        assert!(matches!(
            extract_bytes(b"not a pdf at all".to_vec()).await,
            Err(PdfError::Failed(_))
        ));
        let v = version(Path::new(PDFTOTEXT)).await.unwrap();
        assert!(v.starts_with("pdftotext version"), "{v}");
    }
}
