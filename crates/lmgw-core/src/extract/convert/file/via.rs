//! The handlers WP7 added (document-conversion design §4.2): files read by
//! turning them into a file lmgw already reads, and files a model hears.
//!
//! - Old Word and PowerPoint files (and RTF where pandoc is missing):
//!   LibreOffice writes a docx or pptx (`libreoffice.rs`), which is then
//!   read through the table like any file ([`super::read_at`]), so its
//!   headings, tables, pictures and `json` are a docx's or pptx's, a docx
//!   with equations goes to pandoc in turn, and the answer's tools name
//!   LibreOffice and the reader that read its result.
//! - RTF, the text markups and a Word file with equations: pandoc writes
//!   HTML (`pandoc.rs`), which lmgw's reader writes as markdown, as it does
//!   a web page ([`super::more::html`]).
//! - Without pandoc, a text markup is read as the text it is and a Word
//!   file with equations by lmgw's own reader; each says what pandoc would
//!   add (D4's lesser rows).
//! - Audio and video are heard by the speech-to-text model (`heard.rs`).

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertWarning, ErrorCode, PartsBySource, WarningCode};
use lmgw_api_types::documents::HintKind;

use super::super::budget::OutputBudget;
use super::super::caps::{self, Format, ToolId};
use super::super::libreoffice::{self, LoError, Target};
use super::super::options::Plan;
use super::super::pandoc;
use super::super::pool;
use super::super::report;
use super::super::tools::{Fact, ToolFacts};
use super::{fail, warning, Body, Failure, Read};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::sniff::{Kind, Sniffed};

/// The path of a found tool.
pub(super) fn path(facts: &ToolFacts, id: ToolId) -> Option<std::path::PathBuf> {
    match facts.fact(id) {
        Some(Fact::Found(f)) => Some(f.located.path.clone()),
        _ => None,
    }
}

/// A file that failed by `code`, naming `tool`.
pub(super) fn failed_by(format: Format, tool: ToolId, code: ErrorCode, message: String) -> Failure {
    let mut f = fail(Some(format), code, message);
    f.tool = Some(tool);
    if code == ErrorCode::ToolMissing {
        f.error.tool = Some(tool.as_str().to_string());
    }
    f
}

/// What installs `tool` here, as a sentence's end (` (sudo dnf install
/// pandoc-cli)`), or nothing where there is no one-line command.
fn install(tool: ToolId, plan: &Plan) -> String {
    let h = report::hint(caps::tool(tool), plan.platform);
    if h.kind == HintKind::Command && !h.text.contains('\n') {
        format!(" ({})", h.text)
    } else {
        String::new()
    }
}

/// An old Word or PowerPoint file, or RTF, through LibreOffice.
pub(super) async fn legacy(
    bytes: Bytes,
    format: Format,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &std::sync::Arc<OutputBudget>,
    depth: u32,
) -> Result<Read, Failure> {
    let lo = ToolId::LibreOffice;
    let Some(bin) = path(facts, lo) else {
        return Err(failed_by(
            format,
            lo,
            ErrorCode::ToolMissing,
            "LibreOffice is not installed".to_string(),
        ));
    };
    let to = if format == Format::Ppt {
        Target::Pptx
    } else {
        Target::Docx
    };
    // LibreOffice runs only confined (`confine.rs`).
    let helper = super::super::confine::required(facts)
        .await
        .map_err(|m| failed_by(format, lo, ErrorCode::ToolMissing, m))?;
    let office = libreoffice::Office {
        bin: &bin,
        helper: &helper,
    };
    let root = libreoffice::root(plan.spool_dir.as_deref());
    let file_size = MAX_UNCOMPRESSED_BYTES.min(budget.left());
    let slot = pool::global().acquire().await;
    let out = libreoffice::convert(
        &slot,
        office,
        &root,
        bytes,
        format,
        to,
        libreoffice::bounds(),
        file_size,
    )
    .await;
    drop(slot);
    let out = out.map_err(|e| {
        let code = match e {
            LoError::Missing => ErrorCode::ToolMissing,
            LoError::TooLarge(_) | LoError::FileTooLarge(_) => ErrorCode::OutputTooLarge,
            LoError::NotStarted(_) | LoError::SocketTaken | LoError::NoResult => {
                ErrorCode::ToolNotStarted
            }
            _ => ErrorCode::Unreadable,
        };
        failed_by(format, lo, code, e.said())
    })?;
    let name = if to == Target::Pptx {
        "converted.pptx"
    } else {
        "converted.docx"
    };
    let mut r = Box::pin(super::read_at(out, name, plan, facts, budget, depth))
        .await
        .map_err(|mut f| {
            f.format = Some(format);
            f.tool = Some(lo);
            f
        })?;
    // The outer file's notes on `pages` and `page_break` are its own.
    r.warnings.retain(|w| {
        w.code != WarningCode::PagesNotApplicable
            && !(w.code == WarningCode::OptionNotApplicable
                && w.option.as_deref() == Some("page_break"))
    });
    r.format = format;
    if let Some(t) = r.tool.replace(lo) {
        r.also.push(t);
    }
    r.also.sort();
    r.also.dedup();
    Ok(r)
}

/// RTF, a text markup or a Word file with equations, through pandoc and
/// lmgw's reader.
pub(super) async fn pandoc(
    bytes: Bytes,
    format: Format,
    plan: &Plan,
    facts: &ToolFacts,
) -> Result<Read, Failure> {
    let tool = ToolId::Pandoc;
    let Some(bin) = path(facts, tool) else {
        return Err(failed_by(
            format,
            tool,
            ErrorCode::ToolMissing,
            "pandoc is not installed".to_string(),
        ));
    };
    // Nesting bounded before pandoc's XML readers run (`pandoc::depth`).
    if pandoc::depth::scanned(format) {
        let b = bytes.clone();
        if let Some(m) = super::on_slot(Some(format), move || pandoc::depth::too_deep(&b)).await? {
            return Err(failed_by(format, tool, ErrorCode::TooDeep, m));
        }
    }
    let helper = super::super::confine::helper_for(facts, tool, &bin);
    let html = pandoc::to_html(&bin, helper.as_deref(), bytes, format, pandoc::bounds())
        .await
        .map_err(|e| {
            let (code, message) = pandoc::said(&e);
            failed_by(format, tool, code, message)
        })?;
    let mut r = super::more::html(html, plan, facts)
        .await
        .map_err(|mut f| {
            f.format = Some(format);
            f
        })?;
    // Its pictures are placeholders, so a description has nothing to be
    // made from (in every output, text included): named.
    if plan.describe.is_some() && super::more::has_placeholder(&r.body) {
        r.warnings.push(not_described_through_pandoc());
    }
    for w in &mut r.warnings {
        if w.code == WarningCode::NotYet && w.option.as_deref() == Some("pictures") {
            w.message = w.message.replace(
                "pictures in web pages",
                "pictures in files read through pandoc",
            );
        }
    }
    r.format = format;
    r.tool = Some(tool);
    r.also = vec![ToolId::Reader];
    Ok(r)
}

/// The note for `describe_pictures` on a file read through pandoc.
fn not_described_through_pandoc() -> ConvertWarning {
    let mut w = warning(
        WarningCode::NotYet,
        "pictures in files read through pandoc are not described yet: they are not \
         exported, so no picture of this file was sent to the picture-description model"
            .to_string(),
        Vec::new(),
    );
    w.option = Some("describe_pictures".to_string());
    w
}

/// A Word file with equations read by lmgw's own reader, pandoc passed
/// over: its equations' characters run together, named.
pub(super) async fn docx_native(
    bytes: Bytes,
    plan: &Plan,
    passed_over: &[ToolId],
) -> Result<Read, Failure> {
    let docx = Sniffed {
        kind: Kind::Office,
        mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        sub: "docx",
    };
    let mut r = super::read_office(Format::DocxMath, &docx, bytes, plan).await?;
    let mut w = warning(
        WarningCode::EquationsDropped,
        format!(
            "this Word file has equations, read without pandoc as their characters run \
             together (E = mc² as E=mc2), not as LaTeX; install pandoc to get them as \
             LaTeX{}",
            passed_over
                .first()
                .map_or_else(String::new, |t| install(*t, plan))
        ),
        Vec::new(),
    );
    w.tool = Some(ToolId::Pandoc.as_str().to_string());
    r.warnings.push(w);
    Ok(r)
}

/// A text markup read as the text it is, pandoc passed over: named.
pub(super) fn markup_text(bytes: &Bytes, format: Format, name: &str, plan: &Plan) -> Read {
    let mut w = warning(
        WarningCode::ToolMissing,
        format!(
            "this {} file was read as plain text, markup included; install pandoc to read \
             its headings, lists and tables{}",
            format.as_str(),
            install(ToolId::Pandoc, plan)
        ),
        Vec::new(),
    );
    w.tool = Some(ToolId::Pandoc.as_str().to_string());
    Read {
        format,
        parts: 1,
        by_source: PartsBySource {
            native: 1,
            ..Default::default()
        },
        body: Body::Text {
            text: String::from_utf8_lossy(bytes).into_owned(),
            markdown: super::is_markdown(name),
        },
        warnings: vec![w],
        partial: false,
        tool: None,
        also: Vec::new(),
        part_headings: 0,
        charged: 0,
        model: Default::default(),
    }
}
