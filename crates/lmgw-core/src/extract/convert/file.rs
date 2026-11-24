//! One file of a conversion (document-conversion design §7.1, §7.2, §7.6):
//! sniffed, resolved through the capability table ([`caps::resolve`], the
//! one the Settings page renders, so the two cannot disagree), read by its
//! handler, and shaped as the request asks. Every gap is a warning or the
//! file's named error; one file's failure is its own.
//!
//! The CPU-bound work of a file (an office reader, a PDF's page assembly, the
//! shaping of the markdown) runs on a blocking thread in a slot of the tool
//! pool ([`pool::blocking`], design D19), never on the async runtime; a
//! reader that panics is that file's `unreadable`, and its slot is given back.
//! Each step takes its slot after the one before has given its own back, so
//! no file ever holds one slot while waiting for another. A file's output is
//! counted against the request's [`OutputBudget`].

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use lmgw_api_types::documents::convert::{
    ConvertError, ConvertStats, ConvertWarning, ConvertedDocument, DocStatus, ErrorCode, Output,
    PartsBySource, WarningCode,
};
use lmgw_api_types::documents::model_help::ModelCalls;
use lmgw_api_types::documents::HintKind;

use super::budget::OutputBudget;
use super::caps::{self, Format, ToolId};
use super::doc::{from_md, Doc, Part, PartKind, Source};
use super::options::Plan;
use super::pool;
use super::report;
use super::tools::ToolFacts;
use crate::extract::{office, sniff, OfficeFormat, OfficeText, SniffError};

mod heard;
mod memory;
mod more;
mod output;
mod pdf;
mod pictured;
mod via;

use output::output;

/// One file as the request carried it.
#[derive(Debug, Clone)]
pub struct Input {
    pub index: u32,
    pub name: String,
    /// The bytes, or why there are none (base64 that does not decode).
    pub bytes: Result<Bytes, String>,
}

/// A file's outcome, and the tool that read it.
#[derive(Debug, Clone)]
pub struct Done {
    pub doc: ConvertedDocument,
    pub tool: Option<ToolId>,
    /// Other tools it ran.
    pub also: Vec<ToolId>,
}

/// The plain form (`output: text`) of a reader's markdown.
#[derive(Clone, Copy)]
enum Plain {
    /// Made from the markdown.
    Derive,
    /// The markdown itself (a text file).
    Markdown,
}

/// What a handler read.
enum Body {
    /// A reader's markdown (office files), and its plain form.
    Markdown(String, Plain),
    /// A text file as it is; `markdown` when its name says it is markdown
    /// (`.md`, `.markdown`, …), whose blocks `output: json` reads.
    Text { text: String, markdown: bool },
    /// The structure every output is written from (a PDF's layout, an
    /// office file's parts for `output: json`).
    Doc(Doc),
    /// One of the above with the file's pictures (`figure` blocks in a
    /// `Doc`, markers in an office reader's markdown; [`pictured`]).
    Pictured(Box<Body>, super::pictures::Found),
}

/// What a handler read: its body, and how it was read.
struct Read {
    format: Format,
    parts: u32,
    by_source: PartsBySource,
    body: Body,
    warnings: Vec<ConvertWarning>,
    /// Parts were left out (pages without text).
    partial: bool,
    tool: Option<ToolId>,
    /// Other tools it ran (tesseract for a PDF's scanned pages, the tools
    /// of an e-mail's attachments).
    also: Vec<ToolId>,
    /// Level-2 headings the parts' own titles stand for in markdown
    /// (`## Slide 1`, `## Sheet: Costs`) that `json` carries as the part's
    /// number or name instead, so `stats` counts the same for both.
    part_headings: u32,
    /// What of its output is already counted against the request's budget
    /// (an e-mail's attachments, each charged as it was read).
    charged: u64,
    /// What the page-reading model did for the file (`stats.model_calls`,
    /// `stats.models`); the default for a file no model was asked about.
    model: super::model_help::pdf::Stage,
}

struct Failure {
    format: Option<Format>,
    error: ConvertError,
    tool: Option<ToolId>,
    /// The model calls made for the file before it failed, and who
    /// answered them: paid for, so its `stats` keeps them.
    model: (u32, Vec<ModelCalls>),
}

fn fail(format: Option<Format>, code: ErrorCode, message: impl Into<String>) -> Failure {
    Failure {
        format,
        error: ConvertError {
            code,
            message: message.into(),
            tool: None,
        },
        tool: None,
        model: Default::default(),
    }
}

fn warning(code: WarningCode, message: String, parts: Vec<u32>) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: None,
        parts,
        option: None,
    }
}

/// A reader that panicked: the file's own `unreadable` (the panic is
/// logged, its text kept out of the answer).
fn panicked(format: Option<Format>, e: tokio::task::JoinError) -> Failure {
    tracing::error!("a document reader stopped on a file: {e}");
    fail(
        format,
        ErrorCode::Unreadable,
        "lmgw's reader stopped unexpectedly on this file (a bug in the reader); the file was \
         skipped and the others were converted",
    )
}

/// The step `f` on a blocking thread in a slot of the tool pool; a reader
/// that panics is the file's own `unreadable`.
async fn on_slot<T, F>(format: Option<Format>, f: F) -> Result<T, Failure>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    pool::blocking(f).await.map_err(|e| panicked(format, e))
}

/// A failed file's entry.
fn failed(index: u32, name: String, f: Failure, time_ms: u64) -> Done {
    Done {
        doc: ConvertedDocument {
            index,
            name,
            status: DocStatus::Failed,
            format: f.format.map(|f| f.as_str().to_string()),
            parts: 0,
            content: None,
            structure: None,
            text: None,
            pictures: Vec::new(),
            warnings: Vec::new(),
            stats: ConvertStats {
                time_ms,
                model_calls: f.model.0,
                models: f.model.1,
                ..Default::default()
            },
            error: Some(f.error),
        },
        tool: f.tool,
        also: Vec::new(),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Convert `input` by `plan`, with the tools `facts` found; its output is
/// counted against `budget`, the request's.
pub async fn convert(
    input: Input,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
) -> Done {
    let started = Instant::now();
    // What its poppler runs say of their memory (`pdf::wide`), its warnings.
    let notes = crate::extract::pdf::wide::notes::Notes::new();
    let Input { index, name, bytes } = input;
    let read_all = async {
        let outcome = match bytes {
            Ok(bytes) => read(bytes.clone(), &name, plan, facts, budget)
                .await
                .map(|r| (r, bytes)),
            Err(why) => Err(fail(None, ErrorCode::InvalidBase64, why)),
        };
        match outcome {
            Ok((mut r, bytes)) => {
                pictured::export(&mut r, &bytes, plan, facts, budget).await;
                Ok(r)
            }
            Err(f) => Err(f),
        }
    };
    let outcome = crate::extract::pdf::wide::notes::scope(notes.clone(), Box::pin(read_all)).await;
    let mut r = match outcome {
        Ok(r) => r,
        Err(f) => return failed(index, name, f, elapsed_ms(started)),
    };
    r.warnings.extend(memory::warnings(notes.take()));
    let (format, tool) = (r.format, r.tool);
    let (plan, budget) = (plan.clone(), budget.clone());
    let kept = name.clone();
    let shaped = on_slot(Some(format), move || {
        let time_ms = elapsed_ms(started);
        finish(index, name, r, &plan, time_ms, &budget)
    })
    .await;
    match shaped {
        Ok(done) => done,
        Err(mut f) => {
            f.tool = tool;
            failed(index, kept, f, elapsed_ms(started))
        }
    }
}

/// Whether a text file's name says it is a web page.
fn is_html_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".html", ".htm", ".xhtml"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Whether a text file's name says it is markdown.
fn is_markdown(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".md", ".markdown", ".mdown", ".mkd", ".mkdn"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// The shaped content, its counts and its status.
fn finish(
    index: u32,
    name: String,
    r: Read,
    plan: &Plan,
    time_ms: u64,
    budget: &OutputBudget,
) -> Done {
    let out = match output(r.body, plan, budget, r.charged) {
        Ok(o) => o,
        Err((code, message)) => {
            let mut f = fail(Some(r.format), code, message);
            f.tool = r.tool;
            return failed(index, name, f, time_ms);
        }
    };
    let mut headings = out.counts.headings;
    headings[1] = headings[1].saturating_add(r.part_headings);
    let pictures_count = u32::try_from(out.pictures.len()).unwrap_or(u32::MAX);
    Done {
        doc: ConvertedDocument {
            index,
            name,
            status: if r.partial {
                DocStatus::Partial
            } else {
                DocStatus::Converted
            },
            format: Some(r.format.as_str().to_string()),
            parts: r.parts,
            content: out.content,
            structure: out.structure,
            text: out.text,
            pictures: out.pictures,
            warnings: r.warnings,
            stats: ConvertStats {
                parts_by_source: r.by_source,
                headings,
                tables: out.counts.tables,
                table_cells: out.counts.table_cells,
                pictures: pictures_count,
                model_calls: r.model.calls,
                models: r.model.models,
                pictures_below_describe_min_area: r.model.pictures_small,
                time_ms,
            },
            error: None,
        },
        tool: r.tool,
        also: r.also,
    }
}

/// How a sniffer refusal fails the file: a format lmgw names but has no
/// reader for yet, or one it does not recognise.
fn refused(e: SniffError) -> Failure {
    let code = match e {
        SniffError::LegacyOffice | SniffError::Video(_) => ErrorCode::NoReader,
        SniffError::Unrecognised | SniffError::UnknownZip | SniffError::ManifestTooLarge(_) => {
            ErrorCode::Unsupported
        }
        SniffError::OutlookMessage => ErrorCode::NoReader,
        SniffError::Encrypted => ErrorCode::Unreadable,
    };
    fail(None, code, e.to_string())
}

/// Why nothing reads `format` here, in the page's words, with the command
/// that would change it.
fn unread(format: Format, r: &caps::Resolution, facts: &ToolFacts, plan: &Plan) -> Failure {
    if let Some(why) = r.no_reader {
        let also = super::model_help::picture_note(format, plan.model_help.is_some());
        return fail(Some(format), ErrorCode::NoReader, format!("{why}{also}"));
    }
    let mut message = report::needs_said(&r.missing, facts, plan.platform);
    let unconfined = message.contains(super::confine::NEEDS_LANDLOCK);
    for t in r.missing.iter().filter(|_| !unconfined) {
        let h = report::hint(caps::tool(*t), plan.platform);
        if h.kind == HintKind::Command && !h.text.contains('\n') {
            message.push_str(&format!(" ({})", h.text));
        }
    }
    let mut f = fail(Some(format), ErrorCode::ToolMissing, message);
    f.error.tool = r.missing.first().map(|t| t.as_str().to_string());
    f
}

async fn read(
    bytes: Bytes,
    name: &str,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
) -> Result<Read, Failure> {
    read_at(bytes, name, plan, facts, budget, 0).await
}

/// [`read`] of a file `depth` attachments deep (an e-mail's, `more.rs`).
async fn read_at(
    bytes: Bytes,
    name: &str,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
    depth: u32,
) -> Result<Read, Failure> {
    let sniffed = sniff::sniff_convert_async(bytes.clone())
        .await
        .map_err(refused)?;
    let mut format = Format::of(&sniffed);
    // Text named as a page that does not start like one (`<h1>` first).
    if format == Format::Text && is_html_name(name) {
        format = Format::Html;
    }
    // A text markup named as one (`.tex`, `.rst`, …; WP7).
    if format == Format::Text {
        format = Format::markup_by_name(name).unwrap_or(format);
    }
    let resolved = caps::resolve(format, facts);
    let Some(handler) = resolved.handler else {
        return Err(unread(format, &resolved, facts, plan));
    };
    let mut warnings = Vec::new();
    if plan.pages.is_some() && format != Format::Pdf {
        warnings.push(warning(
            WarningCode::PagesNotApplicable,
            format!(
                "pages applies to PDF pages; this {} file has none, so it was read whole",
                format.as_str()
            ),
            Vec::new(),
        ));
    }
    if plan.page_break.is_some() && format != Format::Pdf {
        let mut w = warning(
            WarningCode::OptionNotApplicable,
            format!(
                "page_break goes between PDF pages; this {} file has none, so nothing was \
                 written",
                format.as_str()
            ),
            Vec::new(),
        );
        w.option = Some("page_break".to_string());
        warnings.push(w);
    }
    let mut r = match handler.id {
        "text.native" => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            Read {
                format,
                parts: 1,
                by_source: PartsBySource {
                    native: 1,
                    ..Default::default()
                },
                body: Body::Text {
                    text,
                    markdown: is_markdown(name),
                },
                warnings: Vec::new(),
                partial: false,
                tool: None,
                also: Vec::new(),
                part_headings: 0,
                charged: 0,
                model: Default::default(),
            }
        }
        "office.native" => read_office(format, &sniffed, bytes, plan).await?,
        "pdf.poppler" => pdf::read(bytes, plan, facts).await?,
        "image.tesseract" => more::image(bytes, plan, facts).await?,
        "xls.calamine" => more::xls(bytes, plan.output, facts).await?,
        "html.native" => more::html(bytes, plan, facts).await?,
        "epub.native" => more::epub(bytes, plan, facts).await?,
        "email.native" => more::email(bytes, plan, facts, budget, depth).await?,
        "legacy.libreoffice" | "rtf.libreoffice" => {
            via::legacy(bytes, format, plan, facts, budget, depth).await?
        }
        "docx_math.pandoc" | "rtf.pandoc" | "markup.pandoc" => {
            via::pandoc(bytes, format, plan, facts).await?
        }
        "docx_math.native" => via::docx_native(bytes, plan, &resolved.passed_over).await?,
        "markup.text" => via::markup_text(&bytes, format, name, plan),
        "audio.asr" => heard::audio(bytes, &sniffed, plan).await?,
        "video.ffmpeg" => heard::video(bytes, &sniffed, plan, facts, budget).await?,
        other => {
            return Err(fail(
                Some(format),
                ErrorCode::Unreadable,
                format!("the handler {other} is not wired to the conversion yet"),
            ))
        }
    };
    warnings.append(&mut r.warnings);
    r.warnings = warnings;
    Ok(r)
}

/// An office file's parts as blocks, for `output: json`: a deck's slides,
/// a workbook's sheets (named), a document as one part.
fn office_doc(of: OfficeFormat, t: &OfficeText) -> Doc {
    let kind = match of {
        OfficeFormat::Pptx | OfficeFormat::Odp => PartKind::Slide,
        f if f.is_spreadsheet() => PartKind::Sheet,
        _ => PartKind::Document,
    };
    Doc {
        parts: t
            .parts
            .iter()
            .enumerate()
            .map(|(i, p)| Part {
                kind,
                number: u32::try_from(i + 1).unwrap_or(u32::MAX),
                name: match kind {
                    PartKind::Sheet => p
                        .heading
                        .as_deref()
                        .map(|h| h.strip_prefix("Sheet: ").unwrap_or(h).to_string()),
                    _ => None,
                },
                size: None,
                source: Some(Source::Native),
                blocks: from_md::blocks(&p.body),
            })
            .collect(),
    }
}

async fn read_office(
    format: Format,
    sniffed: &sniff::Sniffed,
    bytes: Bytes,
    plan: &Plan,
) -> Result<Read, Failure> {
    let output = plan.output;
    let Some(of) = sniffed.office_format() else {
        return Err(fail(
            Some(format),
            ErrorCode::Unsupported,
            "not an office format lmgw reads",
        ));
    };
    // The reading, the pictures' places (and bytes) and the markdown (or
    // the blocks) all run in the slot.
    let pictures = plan.pictures;
    let (parts, body, titled) = on_slot(Some(format), move || {
        office::extract_with_media(of, &bytes).map(|(t, media)| {
            let titled = t.parts.iter().filter(|p| p.heading.is_some()).count();
            let found = super::pictures::office::collect(of, &bytes, &t, media, &pictures);
            let body = if output == Output::Json {
                Body::Doc(office_doc(of, &t))
            } else {
                Body::Markdown(t.markdown(), Plain::Derive)
            };
            (t.parts.len(), Body::Pictured(Box::new(body), found), titled)
        })
    })
    .await?
    .map_err(|e| fail(Some(format), ErrorCode::Unreadable, e.to_string()))?;
    let parts = u32::try_from(parts).unwrap_or(u32::MAX);
    Ok(Read {
        format,
        parts,
        by_source: PartsBySource {
            native: parts,
            ..Default::default()
        },
        body,
        warnings: Vec::new(),
        partial: false,
        tool: None,
        also: Vec::new(),
        part_headings: if output == Output::Json {
            u32::try_from(titled).unwrap_or(u32::MAX)
        } else {
            0
        },
        charged: 0,
        model: Default::default(),
    })
}

#[cfg(test)]
mod tests;
