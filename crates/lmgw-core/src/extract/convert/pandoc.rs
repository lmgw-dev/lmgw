//! pandoc (document-conversion design §4.2, §5.1, WP7): RTF, LaTeX,
//! reStructuredText, Org, MediaWiki, DocBook and FictionBook, and a Word
//! file with equations (D11: pandoc's `docx` reader writes OMML as TeX
//! math). Never a format lmgw's own readers read (D4).
//!
//! `pandoc --sandbox --data-dir=/dev/null/… -f <format> -t html5
//! --wrap=none --mathjax`, the file on stdin, HTML on stdout, then lmgw's
//! reader (`reader.rs`, the web-page path) writes it as markdown:
//! - `--sandbox`: the document cannot make pandoc read a file (LaTeX's
//!   `\input`, RST's `include`, Org's `#+INCLUDE`) or fetch anything; the
//!   probe checks the pandoc it finds honours it ([`check_sandbox`]), and a
//!   pandoc without it (before 2.15) is shown as failing its check, so no
//!   format is read through it;
//! - no filter, no Lua, no `--resource-path`, no defaults file, no
//!   template: none is passed, and `--data-dir` names a path that cannot
//!   exist, so the person's `~/.local/share/pandoc` is never read;
//! - the input format is named (`-f`), never guessed from a name;
//! - HTML, not markdown, comes out: pandoc's markdown writers cannot write
//!   a table whose cells hold lists or several paragraphs without raw HTML,
//!   and write `[TABLE]` in its place when raw HTML is off (seen with an
//!   RTF from LibreOffice, pandoc 3.7.0.2). lmgw's reader writes such
//!   tables with its own table writer, skips scripts, frames and objects a
//!   document's raw HTML may carry, and writes every picture as docling's
//!   placeholder, so raw HTML does not reach the markdown;
//! - `--mathjax` keeps TeX math as TeX (`\(…\)`, `\[…\]` in
//!   `span.math`), which the reader writes as `$…$` and `$$…$$`.
//!
//! The run is bounded like every tool's (`run.rs`): [`PANDOC_TIMEOUT`],
//! [`PANDOC_MEMORY_LIMIT_BYTES`], its stdout by `MAX_UNCOMPRESSED_BYTES`,
//! a slot of the tool pool, its own process group. DocBook and FictionBook
//! input is bounded by its nesting before pandoc runs ([`depth`]: past
//! `PANDOC_MAX_XML_DEPTH` the file fails with `too_deep`).

use std::path::Path;
use std::time::Duration;

pub mod depth;

use bytes::Bytes;

use super::caps::Format;
use super::run::{self, Bounds, RunError};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

/// How long one pandoc run may take: five minutes, as poppler's
/// (`PDF_TIMEOUT`). A book-length LaTeX file converts in a few seconds;
/// a run still going after five minutes is a file that makes the parser
/// loop.
pub const PANDOC_TIMEOUT: Duration = Duration::from_secs(300);

/// pandoc's address space: 4 GiB, what poppler's limit was when it was set
/// (poppler's was lowered from measured peaks on 2026-11-20, pandoc's was
/// not measured then and keeps its figure). pandoc 3.7.0.2's runtime starts
/// within 1 GiB (measured 2026-11-19); a long document's tree takes the rest.
pub const PANDOC_MEMORY_LIMIT_BYTES: u64 = 4 << 30;

/// The `--data-dir` every run is given: a path under `/dev/null`, which
/// no one can create, so no data directory is read from the person's home
/// or the environment.
const NO_DATA_DIR: &str = "--data-dir=/dev/null/lmgw-no-pandoc-data";

/// pandoc's name for `format` (`-f`).
pub fn input_format(format: Format) -> Option<&'static str> {
    Some(match format {
        Format::Rtf => "rtf",
        Format::DocxMath => "docx",
        Format::Latex => "latex",
        Format::Rst => "rst",
        Format::Org => "org",
        Format::MediaWiki => "mediawiki",
        Format::DocBook => "docbook",
        Format::Fb2 => "fb2",
        _ => return None,
    })
}

/// The arguments for reading a `from` file.
fn args(from: &str) -> [&str; 8] {
    [
        "--sandbox",
        NO_DATA_DIR,
        "-f",
        from,
        "-t",
        "html5",
        "--wrap=none",
        "--mathjax",
    ]
}

/// The bounds of a real run.
pub fn bounds() -> Bounds {
    Bounds {
        timeout: PANDOC_TIMEOUT,
        output: MAX_UNCOMPRESSED_BYTES,
        memory: PANDOC_MEMORY_LIMIT_BYTES,
    }
}

/// `bytes` (a `format` file) as HTML, by the pandoc at `bin`.
pub async fn to_html(
    bin: &Path,
    helper: Option<&Path>,
    bytes: Bytes,
    format: Format,
    bounds: Bounds,
) -> Result<Bytes, RunError> {
    let Some(from) = input_format(format) else {
        return Err(RunError::Failed(format!(
            "lmgw does not read {} files through pandoc",
            format.as_str()
        )));
    };
    let args = args(from);
    // Confined where lmgw's reader can confine it (`confine.rs`): stdin to
    // stdout, `/usr` and the loader cache, no file written, no network.
    let rules = super::confine::filter(None, MAX_UNCOMPRESSED_BYTES);
    let tool = run::Tool {
        confine: helper.map(|helper| run::Confined {
            helper,
            rules: &rules,
        }),
        ..run::Tool::new(bin, &args)
    };
    let slot = super::pool::global().acquire().await;
    run::run_held(&slot, tool, run::Input::Bytes(bytes), bounds)
        .await
        .map(Bytes::from)
}

/// How a pandoc run fails the file: the code and the message.
pub fn said(e: &RunError) -> (lmgw_api_types::documents::convert::ErrorCode, String) {
    use lmgw_api_types::documents::convert::ErrorCode;
    match e {
        RunError::Missing => (
            ErrorCode::ToolMissing,
            "pandoc was not found when it was run".to_string(),
        ),
        RunError::NoReader => (ErrorCode::ToolMissing, super::run::start::no_reader()),
        RunError::NotStarted(m) => (ErrorCode::ToolNotStarted, m.clone()),
        RunError::Timeout => (
            ErrorCode::Unreadable,
            format!(
                "pandoc did not finish within PANDOC_TIMEOUT ({} s) and was stopped",
                PANDOC_TIMEOUT.as_secs()
            ),
        ),
        RunError::TooLarge | RunError::FileTooLarge(_) => (
            ErrorCode::OutputTooLarge,
            format!(
                "pandoc's output passed MAX_UNCOMPRESSED_BYTES ({}) and was stopped",
                crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
            ),
        ),
        RunError::OutOfMemory => (
            ErrorCode::Unreadable,
            format!(
                "pandoc ran out of PANDOC_MEMORY_LIMIT_BYTES ({}) and was stopped",
                crate::hf::fmt_bytes(PANDOC_MEMORY_LIMIT_BYTES)
            ),
        ),
        RunError::Failed(m) => (
            ErrorCode::Unreadable,
            format!("pandoc could not read this file: {m}"),
        ),
    }
}

/// What the probe's sandbox check reads: a file next to the LaTeX that
/// asks for it.
const CANARY: &str = "lmgw-sandbox-canary";

/// Whether the pandoc at `bin` honours `--sandbox` (module doc), checked in
/// the probe's private directory `dir` by `run` (the probe's own runner:
/// `(args, dir) -> Ok((stdout, stderr, success))`): a LaTeX file there
/// `\input`s a file beside it; a sandboxed pandoc leaves it out. `Err`
/// says why the tool is not used.
pub async fn check_sandbox<F, Fut>(dir: &Path, run: F) -> Result<(), String>
where
    F: FnOnce(Vec<std::ffi::OsString>) -> Fut,
    Fut: std::future::Future<Output = Result<(String, String, bool), String>>,
{
    let canary = dir.join("canary.tex");
    let main = dir.join("main.tex");
    std::fs::write(&canary, CANARY)
        .and_then(|()| std::fs::write(&main, "\\input{canary.tex}\n\nx\n"))
        .map_err(|e| format!("pandoc's sandbox could not be checked: {e}"))?;
    let args = vec![
        "--sandbox".into(),
        NO_DATA_DIR.into(),
        "-f".into(),
        "latex".into(),
        "-t".into(),
        "plain".into(),
        main.into_os_string(),
    ];
    let (stdout, stderr, ok) = run(args).await?;
    if !ok {
        let said = format!("{stdout}\n{stderr}");
        if said.contains("sandbox") {
            return Err(
                "this pandoc has no --sandbox (pandoc 2.15 or newer has it); lmgw reads \
                 files through pandoc only sandboxed, so it does not use this one"
                    .to_string(),
            );
        }
        return Err(format!(
            "pandoc failed lmgw's sandbox check: {}",
            run::first_lines(&said, true)
        ));
    }
    if stdout.contains(CANARY) {
        return Err(
            "this pandoc read a file a document asked for despite --sandbox; lmgw does not \
             use it"
                .to_string(),
        );
    }
    Ok(())
}
