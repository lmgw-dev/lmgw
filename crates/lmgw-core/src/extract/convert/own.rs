//! Chat attachments and knowledge bases, lmgw's own readers of uploads,
//! resolved through the capability table too (document-conversion design
//! §1, §4.2: "the chat and knowledge-base refusals keep their wording until
//! those callers also move to `resolve`", WP7).
//!
//! The step taken here is the smallest that makes them resolve through
//! [`caps::resolve`] without changing what they already read:
//! - **A file their own sniffer reads stays on its path.** Text as it is,
//!   PDFs through `extract/pdf.rs` with Chat's page images and a knowledge
//!   base's vision pages, office files through `office.rs`, pictures and
//!   audio as before. The text an existing knowledge base was indexed from
//!   is therefore what a re-ingest reads again; nothing is re-indexed.
//! - **A file it refuses** is sniffed again by the conversion's sniffer
//!   and, when the table has a reader for it that needs no model, read by
//!   the conversion's own handler ([`super::file::convert`], markdown):
//!   old Word and PowerPoint files through LibreOffice, old Excel files and
//!   EPUB books through lmgw's reader. Its steps take tool-pool slots as
//!   lmgw's own caller, as the other Chat and knowledge-base reads do.
//! - **What the table cannot read here is refused by name**: a missing tool
//!   with what to install (the conversion's `tool_missing` words), a
//!   password-protected Office file, an Outlook message; anything else
//!   keeps the old refusal and its list of what is accepted.
//!
//! Not yet: pictures (a TIFF scan, by text recognition) and videos (their
//! sound) in Chat and knowledge bases, which the table reads with tools
//! and models the two have their own paths for (Chat sends pictures to the
//! model and transcribes audio with its own speech-to-text model; a
//! knowledge base takes neither); the formats their sniffer reads as text
//! (HTML, e-mail, RTF and the text markups) or as office files (a Word
//! file with equations) moving to the table's readers, which would change
//! the text existing knowledge bases were indexed from.

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertOptions, DocStatus, Output, WarningCode};
use lmgw_api_types::documents::Tier;

use super::budget::OutputBudget;
use super::caps::{self, Format};
use super::file::{self, Input};
use super::{options, report};
use crate::extract::sniff::{sniff_convert_async, Kind, SniffError, Sniffed};
use crate::state::SharedState;

/// The formats this path takes: the table reads them without a model, and
/// Chat and knowledge bases have no path of their own for them.
fn taken(format: Format) -> bool {
    matches!(
        format,
        Format::Doc | Format::Ppt | Format::Xls | Format::Epub | Format::Rtf
    )
}

/// What a refused file came to.
#[derive(Debug, Clone)]
pub struct Converted {
    /// What it is: kind `office`, its own format as `sub`.
    pub sniffed: Sniffed,
    pub markdown: String,
    /// Its parts (slides, sheets; one for a document).
    pub parts: u32,
    /// Its conversion's warnings, in words.
    pub notes: Vec<String>,
}

/// Whether a file `extract::sniff` refused with `refused` is one this path
/// reads here: what it is, or the refusal in words.
pub async fn accepted(
    state: &SharedState,
    bytes: &Bytes,
    refused: SniffError,
) -> Result<Sniffed, String> {
    let s = match sniff_convert_async(bytes.clone()).await {
        Ok(s) => s,
        Err(e @ (SniffError::Encrypted | SniffError::OutlookMessage)) => return Err(e.to_string()),
        Err(_) => return Err(refused.to_string()),
    };
    let format = Format::of(&s);
    if !taken(format) {
        return Err(refused.to_string());
    }
    let facts = state.doc_tools.current().await;
    let r = caps::resolve(format, &facts);
    match r.handler {
        Some(h) if h.tier == Tier::Zero => Ok(Sniffed {
            kind: Kind::Office,
            mime: s.mime,
            sub: format.as_str(),
        }),
        Some(_) => Err(refused.to_string()),
        None => {
            let platform = report::platform(state.deployment().is_container());
            let why = r.no_reader.map_or_else(
                || report::needs_said(&r.missing, &facts, platform),
                str::to_string,
            );
            let mut hint = String::new();
            for t in &r.missing {
                let h = report::hint(caps::tool(*t), platform);
                if h.kind == lmgw_api_types::documents::HintKind::Command && !h.text.contains('\n')
                {
                    hint = format!(" ({})", h.text);
                }
            }
            Err(format!(
                "this {} file is {why}{hint}, or save it as a newer format (.docx, .pptx, \
                 .xlsx) and send that",
                format.as_str()
            ))
        }
    }
}

/// Read a file `extract::sniff` refused with `refused`, named `name`,
/// through the table ([`accepted`] first): its markdown, or why not.
pub async fn read(
    state: &SharedState,
    name: &str,
    bytes: Bytes,
    refused: SniffError,
) -> Result<Converted, String> {
    let sniffed = accepted(state, &bytes, refused).await?;
    let platform = report::platform(state.deployment().is_container());
    let (plan, _) = options::plan(&ConvertOptions::default(), platform).map_err(|r| r.message)?;
    let plan = plan.spooling_under(&state.data_dir);
    let facts = state.doc_tools.current().await;
    let budget = std::sync::Arc::new(OutputBudget::request());
    let input = Input {
        index: 0,
        name: name.to_string(),
        bytes: Ok(bytes),
    };
    let done = Box::pin(file::convert(input, &plan, &facts, &budget)).await;
    let doc = done.doc;
    if doc.status == DocStatus::Failed {
        return Err(doc
            .error
            .map_or_else(|| "the file could not be read".to_string(), |e| e.message));
    }
    debug_assert_eq!(plan.output, Output::Markdown);
    Ok(Converted {
        sniffed,
        markdown: doc.content.unwrap_or_default(),
        parts: doc.parts,
        // Chat and knowledge bases take the text, not its pictures: notes on
        // the pictures' export are not theirs.
        notes: doc
            .warnings
            .into_iter()
            .filter(|w| {
                !matches!(
                    w.code,
                    WarningCode::PicturesLeftOut | WarningCode::PicturesPartial
                ) && w.option.as_deref() != Some("pictures")
            })
            .map(|w| w.message)
            .collect(),
    })
}
