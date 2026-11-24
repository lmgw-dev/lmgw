//! The handlers WP4 added (document-conversion design §4.2): image files
//! through text recognition ([`super::super::ocr`]), and legacy `.xls`,
//! HTML, EPUB and e-mail, each parsed by lmgw's reader executable
//! ([`super::super::reader`]) on a slot of the tool pool and given to the
//! conversion as a [`Read`]. Nothing of these files is parsed in this
//! process: what arrives here is the reader's bounded answer (markdown,
//! cells, a message's fields and its attachments' bytes).
//!
//! An e-mail's attachments are read through the capability table like files
//! of their own (§4.2, "attachments recurse through the table"): each is
//! sniffed and resolved, read by its handler (in a slot of its own, by a
//! child where its format needs one) and its parts follow the message's,
//! named after the attachment; one that cannot be read is a `part_failed`
//! warning and the rest is returned. A message attached to a message is
//! read the same way, at most [`MAX_ATTACHMENT_DEPTH`] deep. What each
//! attachment converts to is charged to the request's output budget as it
//! is read, so a message cannot hold more converted text than a request of
//! plain files could: once the budget is spent, the attachments not yet
//! read are named and left.

use std::sync::Arc;

use bytes::Bytes;
use lmgw_api_types::documents::convert::{
    ConvertWarning, ErrorCode, Output, PartsBySource, WarningCode,
};
use lmgw_doc_reader::wire::{Answer, Op};

use super::super::budget::OutputBudget;
use super::super::caps::{Format, ToolId};
use super::super::doc::{from_md, Block, BlockKind, Doc, Part, PartKind, Source};
use super::super::options::Plan;
use super::super::pictures::{self, office, Found};
use super::super::reader::{self, ReadError};
use super::super::run::Input;
use super::super::tools::ToolFacts;
use super::super::{html, ocr, xls};
use super::output::text_doc;
use super::{fail, on_slot, warning, Body, Failure, Plain, Read};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

mod late;

/// How deep messages attached to messages are read: 3 (a forward of a
/// forward of a forward). A deeper one is named, not read.
pub const MAX_ATTACHMENT_DEPTH: u32 = 3;

/// `md`'s blocks, docling's picture placeholder as a figure.
fn blocks(md: &str) -> Vec<Block> {
    from_md::blocks(md)
        .into_iter()
        .map(|b| {
            if b.kind == BlockKind::Paragraph && b.text.trim() == html::PICTURE {
                Block::new(BlockKind::Figure, String::new(), None, Source::Native)
            } else {
                b
            }
        })
        .collect()
}

fn document(blocks: Vec<Block>, name: Option<String>) -> Part {
    Part {
        kind: PartKind::Document,
        number: 1,
        name,
        size: None,
        source: Some(Source::Native),
        blocks,
    }
}

/// A reader's markdown as the body `output` asks for: the markdown itself,
/// or its blocks for `json`.
fn markdown_body(md: String, output: Output) -> Body {
    if output == Output::Json {
        Body::Doc(Doc {
            parts: vec![document(blocks(&md), None)],
        })
    } else {
        Body::Markdown(md, Plain::Derive)
    }
}

fn native(format: Format, body: Body, notes: Vec<String>) -> Read {
    let warnings: Vec<ConvertWarning> = notes
        .into_iter()
        .map(|n| warning(WarningCode::PartFailed, n, Vec::new()))
        .collect();
    Read {
        format,
        parts: 1,
        by_source: PartsBySource {
            native: 1,
            ..Default::default()
        },
        body,
        partial: !warnings.is_empty(),
        warnings,
        tool: Some(ToolId::Reader),
        also: Vec::new(),
        part_headings: 0,
        charged: 0,
        model: Default::default(),
    }
}

/// How a read by lmgw's reader fails the file.
fn unread(format: Format, e: ReadError) -> Failure {
    let (code, message) = e.failure();
    let mut f = fail(Some(format), code, message);
    f.tool = Some(ToolId::Reader);
    if code == ErrorCode::ToolMissing {
        f.error.tool = Some(ToolId::Reader.as_str().to_string());
    }
    f
}

/// An image file, read by tesseract (a TIFF page by page).
pub(super) async fn image(bytes: Bytes, plan: &Plan, facts: &ToolFacts) -> Result<Read, Failure> {
    let r = ocr::pages::image_file(bytes, plan, facts)
        .await
        .map_err(|(code, why, tool)| {
            let mut f = fail(Some(Format::Image), code, why);
            f.tool = Some(tool);
            if code == ErrorCode::ToolMissing {
                f.error.tool = Some(tool.as_str().to_string());
            }
            f
        })?;
    let parts = u32::try_from(r.doc.parts.len()).unwrap_or(u32::MAX);
    // Asked for the pictures themselves, the image file is its one picture
    // (`pictured::export`).
    let warnings = r.warnings;
    Ok(Read {
        format: Format::Image,
        parts,
        by_source: PartsBySource {
            ocr: r.read,
            ..Default::default()
        },
        body: Body::Doc(r.doc),
        warnings,
        partial: r.partial,
        tool: Some(ToolId::Tesseract),
        also: vec![ToolId::Reader],
        part_headings: 0,
        charged: 0,
        model: Default::default(),
    })
}

/// A legacy `.xls` workbook: each sheet a part.
pub(super) async fn xls(bytes: Bytes, output: Output, facts: &ToolFacts) -> Result<Read, Failure> {
    let frame = reader::read(facts, Op::Xls, Input::Bytes(bytes), MAX_UNCOMPRESSED_BYTES)
        .await
        .map_err(|e| unread(Format::Xls, e))?;
    let Answer::Sheets(cells) = frame.answer else {
        return Err(unread(Format::Xls, reader::unexpected()));
    };
    // The tables are lmgw's own work on the bounded cells, in a slot.
    let sheets = on_slot(Some(Format::Xls), move || xls::parts(cells))
        .await?
        .map_err(|why| fail(Some(Format::Xls), ErrorCode::Unreadable, why))?;
    let parts = u32::try_from(sheets.len()).unwrap_or(u32::MAX);
    let body = if output == Output::Json {
        Body::Doc(Doc {
            parts: sheets
                .iter()
                .enumerate()
                .map(|(i, p)| Part {
                    kind: PartKind::Sheet,
                    number: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    name: p
                        .heading
                        .as_deref()
                        .map(|h| h.strip_prefix("Sheet: ").unwrap_or(h).to_string()),
                    size: None,
                    source: Some(Source::Native),
                    blocks: from_md::blocks(&p.body),
                })
                .collect(),
        })
    } else {
        let text = crate::extract::OfficeText {
            format: crate::extract::OfficeFormat::Xlsx,
            parts: sheets,
        };
        Body::Markdown(text.markdown(), Plain::Derive)
    };
    Ok(Read {
        format: Format::Xls,
        parts,
        by_source: PartsBySource {
            native: parts,
            ..Default::default()
        },
        body,
        warnings: Vec::new(),
        partial: false,
        tool: Some(ToolId::Reader),
        also: Vec::new(),
        part_headings: if output == Output::Json { parts } else { 0 },
        charged: 0,
        model: Default::default(),
    })
}

/// An HTML page.
pub(super) async fn html(bytes: Bytes, plan: &Plan, facts: &ToolFacts) -> Result<Read, Failure> {
    let output = plan.output;
    let input = Input::Bytes(bytes.clone());
    let frame = reader::read(facts, Op::Html, input, MAX_UNCOMPRESSED_BYTES)
        .await
        .map_err(|e| unread(Format::Html, e))?;
    let Answer::Page(page) = frame.answer else {
        return Err(unread(Format::Html, reader::unexpected()));
    };
    let Some(deep) = page.too_deep else {
        let body = markdown_body(page.markdown, output);
        let mut r = native(Format::Html, body, Vec::new());
        r.warnings.extend(pictures_not_yet(
            plan,
            has_placeholder(&r.body),
            "web pages",
        ));
        return Ok(r);
    };
    // Too deep for the markdown writer: read flat by lmgw's own reducer.
    let text = on_slot(Some(Format::Html), move || html::flat(&bytes)).await?;
    Ok(native(
        Format::Html,
        markdown_body(text, output),
        vec![html::flat_note(deep)],
    ))
}

/// An EPUB book: its chapters in reading order, one document.
pub(super) async fn epub(bytes: Bytes, plan: &Plan, facts: &ToolFacts) -> Result<Read, Failure> {
    let output = plan.output;
    let op = Op::Epub {
        budget: MAX_UNCOMPRESSED_BYTES,
    };
    let frame = reader::read(facts, op, Input::Bytes(bytes), MAX_UNCOMPRESSED_BYTES)
        .await
        .map_err(|e| unread(Format::Epub, e))?;
    let Answer::Book(book) = frame.answer else {
        return Err(unread(Format::Epub, reader::unexpected()));
    };
    let md = book.chapters.join("\n\n");
    let mut r = native(Format::Epub, markdown_body(md, output), book.notes);
    // Its pictures are placeholders until EPUB picture export is built.
    if output != Output::Text && has_placeholder(&r.body) {
        r.warnings.push(pictures::epub_not_yet());
    }
    Ok(r)
}

/// A body that is plain text, as paragraphs between its empty lines.
fn plain_blocks(text: &str) -> Vec<Block> {
    text.replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim_end)
        .filter(|p| !p.trim().is_empty())
        .map(|p| Block::new(BlockKind::Paragraph, p.to_string(), None, Source::Native))
        .collect()
}

/// A read attachment's parts, each named after it. Its pictures (an office
/// file's or a PDF's, already exported as the request asks) join the
/// message's `pictures`, their figure blocks renumbered after the ones
/// before them.
fn attachment_parts(body: Body, name: &str, pictures: &mut Option<Found>) -> Vec<Part> {
    let (inner, found) = match body {
        Body::Pictured(inner, found) => (*inner, Some(found)),
        other => (other, None),
    };
    let mut parts = match inner {
        Body::Doc(d) => d.parts,
        Body::Markdown(md, _) => vec![document(blocks(&md), None)],
        Body::Text { text, markdown } => text_doc(text, markdown).parts,
        // A reader never nests them (`pictured::output` refuses it too).
        Body::Pictured(..) => Vec::new(),
    };
    if let Some(mut found) = found {
        if !found.markers.is_empty() {
            let mut doc = Doc { parts };
            office::figures(&mut doc, &found);
            parts = doc.parts;
        }
        if found.pictures.is_empty() {
            return named(parts, name);
        }
        let all = pictures.get_or_insert_with(Found::default);
        let before = u32::try_from(all.pictures.len()).unwrap_or(u32::MAX);
        for b in parts.iter_mut().flat_map(|p| p.blocks.iter_mut()) {
            if let Some(f) = &mut b.figure {
                f.index = f.index.saturating_add(before);
            }
        }
        all.pictures.append(&mut found.pictures);
        all.charged = all.charged.saturating_add(found.charged);
    }
    named(parts, name)
}

/// `parts` named after the attachment `name`, under its heading.
fn named(mut parts: Vec<Part>, name: &str) -> Vec<Part> {
    for p in &mut parts {
        p.name = Some(name.to_string());
    }
    if let Some(first) = parts.first_mut() {
        first.blocks.insert(
            0,
            Block::new(
                BlockKind::Heading(2),
                format!("Attachment: {name}"),
                None,
                Source::Native,
            ),
        );
    }
    parts
}

/// What one block's output takes besides its text, at most: json's fields
/// (kind, level, source, a box of four numbers, the braces and quotes) are
/// about 120 bytes; markdown's marks and blank lines a few. An attachment is
/// charged this much over before its output is written, so the message
/// that keeps it also has room to write it; what the output did not use is
/// given back (`budget::Spend`).
pub const BLOCK_ALLOWANCE: u64 = 160;

/// What a document takes of the output at least: its blocks' text with
/// [`BLOCK_ALLOWANCE`] each, and its tables' grids.
fn doc_bytes(d: &Doc) -> u64 {
    d.parts
        .iter()
        .flat_map(|p| &p.blocks)
        .map(|b| (b.text.len() as u64).saturating_add(BLOCK_ALLOWANCE))
        .fold(d.grid_cost(), u64::saturating_add)
}

/// What a read body takes of the output at least (its pictures' bytes are
/// reserved apart, [`reserved`]).
fn body_bytes(body: &Body) -> u64 {
    match body {
        Body::Markdown(md, _) => md.len() as u64,
        Body::Text { text, .. } => text.len() as u64,
        Body::Doc(d) => doc_bytes(d),
        Body::Pictured(inner, _) => body_bytes(inner),
    }
}

/// What a read body's pictures reserved of the request's output.
fn reserved(body: &Body) -> u64 {
    match body {
        Body::Pictured(_, found) => found.charged,
        _ => 0,
    }
}

/// The note a file with pictures gets where its reader does not export
/// them yet (as a workbook's, `pictures::office`): `what` names the file's
/// kind. `output: text` writes no pictures, so it gets none.
fn pictures_not_yet(plan: &Plan, has: bool, what: &str) -> Option<ConvertWarning> {
    if !has || plan.output == Output::Text {
        return None;
    }
    let mut w = warning(
        WarningCode::NotYet,
        format!(
            "pictures in {what} are not exported yet: each is a placeholder (<!-- image -->, a \
             figure block in json) and the file's pictures list is empty; a picture on another \
             server is never fetched"
        ),
        Vec::new(),
    );
    w.option = Some("pictures".to_string());
    Some(w)
}

/// Whether `body` has a picture placeholder (a reader's `<!-- image -->`).
pub(super) fn has_placeholder(body: &Body) -> bool {
    match body {
        Body::Markdown(md, _) => md.lines().any(|l| l.trim() == html::PICTURE),
        Body::Doc(d) => d
            .parts
            .iter()
            .flat_map(|p| &p.blocks)
            .any(|b| b.kind == BlockKind::Figure && b.figure.is_none()),
        _ => false,
    }
}

/// An e-mail message, its attachments read through the table (module doc).
pub(super) async fn email(
    bytes: Bytes,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
    depth: u32,
) -> Result<Read, Failure> {
    // The body as a document, and the attachments, which decoding never
    // makes larger than the message.
    let bound = MAX_UNCOMPRESSED_BYTES.saturating_add(bytes.len() as u64);
    let frame = reader::read(facts, Op::Email, Input::Bytes(bytes), bound)
        .await
        .map_err(|e| unread(Format::Email, e))?;
    let lens: Vec<u64> = match &frame.answer {
        Answer::Mail(m) => m.attachments.iter().map(|a| a.len).collect(),
        _ => return Err(unread(Format::Email, reader::unexpected())),
    };
    let Some(blobs) = frame.blobs(&lens) else {
        return Err(unread(Format::Email, reader::unexpected()));
    };
    let Answer::Mail(mail) = frame.answer else {
        return Err(unread(Format::Email, reader::unexpected()));
    };
    let mut head = Vec::new();
    if let Some(s) = &mail.subject {
        head.push(Block::new(
            BlockKind::Heading(1),
            s.clone(),
            None,
            Source::Native,
        ));
    }
    if !mail.fields.is_empty() {
        head.push(Block::new(
            BlockKind::Paragraph,
            mail.fields.join("\n"),
            None,
            Source::Native,
        ));
    }
    head.extend(if mail.plain {
        plain_blocks(&mail.body)
    } else {
        blocks(&mail.body)
    });
    let body_pictures = head
        .iter()
        .any(|b| b.kind == BlockKind::Figure && b.figure.is_none());
    let mut doc = Doc {
        parts: vec![document(head, None)],
    };
    let mut warnings: Vec<ConvertWarning> = mail
        .notes
        .into_iter()
        .map(|n| warning(WarningCode::PartFailed, n, Vec::new()))
        .collect();
    let mut by_source = PartsBySource {
        native: 1,
        ..Default::default()
    };
    let mut partial = !warnings.is_empty();
    let mut also: Vec<ToolId> = Vec::new();
    // What the message itself and its read attachments take of the
    // request's output, charged as each is read. An attachment that no
    // longer fits is left out without being counted, so the message keeps
    // what did fit.
    let mut charged = doc_bytes(&doc);
    let mut spent = !budget.try_take(charged);
    if spent {
        // Not taken: the message's output counts it in full.
        charged = 0;
    }
    let mut pictures: Option<Found> = None;
    // The page-reading model's calls for the attachments (an attached
    // scan), paid for whether the attachment was read or not.
    let mut model = super::super::model_help::pdf::Stage::default();
    let mut too_deep: Vec<String> = Vec::new();
    let mut left: Vec<String> = Vec::new();
    // Attachments the caller's document_timeout ended or left unread.
    let mut late: Vec<String> = Vec::new();
    for (a, data) in mail.attachments.into_iter().zip(blobs) {
        if spent {
            left.push(a.name);
            continue;
        }
        // Once a model stage took the message's end over, nothing else ends
        // it at the deadline (`batch::within_deadline`): the attachments
        // after the deadline are not read, and one still being read there
        // ends (`late::read`).
        if late::expired() {
            late.push(a.name);
            continue;
        }
        if depth >= MAX_ATTACHMENT_DEPTH {
            too_deep.push(format!("{} ({})", a.name, a.content_type));
            continue;
        }
        let data_kept = data.clone();
        let read = Box::pin(super::read_at(
            data,
            &a.name,
            plan,
            facts,
            budget,
            depth + 1,
        ));
        let read = match late::read(read).await {
            Ok(read) => read,
            Err((calls, models)) => {
                // Its calls were sent whether it is kept or not.
                model.absorb(calls, models);
                late.push(a.name);
                continue;
            }
        };
        match read {
            Ok(mut r) => {
                // Its pictures' bytes as the request asks (a PDF's crops, an
                // office file's media), each reserved from the budget; one
                // that does not fit stays a placeholder, named.
                super::pictured::export(&mut r, &data_kept, plan, facts, budget).await;
                // Its model calls were made whether it is kept or not.
                model.absorb(r.model.calls, std::mem::take(&mut r.model.models));
                model.pictures_small = model.pictures_small.saturating_add(r.model.pictures_small);
                // A message attached to this one has charged its own.
                let size = body_bytes(&r.body);
                if !budget.try_take(size.saturating_sub(r.charged)) {
                    // Left out: what it reserved goes back.
                    budget.give_back(r.charged.saturating_add(reserved(&r.body)));
                    spent = true;
                    left.push(a.name);
                    continue;
                }
                charged = charged.saturating_add(size.max(r.charged));
                by_source.native += r.by_source.native;
                by_source.text_layer += r.by_source.text_layer;
                by_source.ocr += r.by_source.ocr;
                by_source.vision += r.by_source.vision;
                by_source.asr += r.by_source.asr;
                partial |= r.partial;
                also.extend(r.tool);
                also.extend(r.also);
                warnings.extend(r.warnings.into_iter().map(|mut w| {
                    w.message = format!("attachment {}: {}", a.name, w.message);
                    // Its parts are numbered in the attachment.
                    w.parts.clear();
                    w
                }));
                doc.parts
                    .extend(attachment_parts(r.body, &a.name, &mut pictures));
            }
            Err(f) if f.error.code == ErrorCode::DeadlineExceeded => {
                // Its own model call ended at the message's deadline (a
                // transcription): named with the attachments the deadline
                // ended.
                partial = true;
                model.absorb(f.model.0, f.model.1);
                late.push(a.name);
            }
            Err(f) => {
                partial = true;
                model.absorb(f.model.0, f.model.1);
                let mut w = warning(
                    WarningCode::PartFailed,
                    format!(
                        "the attachment {} ({}) was not read: {}",
                        a.name, a.content_type, f.error.message
                    ),
                    Vec::new(),
                );
                w.tool = f.error.tool;
                warnings.push(w);
            }
        }
    }
    if !too_deep.is_empty() {
        partial = true;
        warnings.push(warning(
            WarningCode::PartFailed,
            format!(
                "{} attachment(s) are in a message attached more than MAX_ATTACHMENT_DEPTH \
                 ({MAX_ATTACHMENT_DEPTH}) messages deep and were not read: {}",
                too_deep.len(),
                too_deep.join(", ")
            ),
            Vec::new(),
        ));
    }
    if let Some(w) = late::note(&late) {
        partial = true;
        warnings.push(w);
    }
    if spent && !left.is_empty() {
        let why = format!(
            "MAX_UNCOMPRESSED_BYTES, {}, for all of the request's files together",
            crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
        );
        partial = true;
        warnings.push(warning(
            WarningCode::PartFailed,
            format!(
                "{} attachment(s) were not read, because the request's output was used up \
                 ({why}): {}",
                left.len(),
                left.join(", ")
            ),
            Vec::new(),
        ));
    }
    also.sort();
    also.dedup();
    let parts = u32::try_from(doc.parts.len()).unwrap_or(u32::MAX);
    // The message's own pictures (its HTML body's) are placeholders; its
    // attachments' were exported as each was read.
    if let Some(w) = pictures_not_yet(plan, body_pictures, "e-mail bodies") {
        warnings.push(w);
    }
    let body = match pictures {
        Some(found) => Body::Pictured(Box::new(Body::Doc(doc)), found),
        None => Body::Doc(doc),
    };
    Ok(Read {
        format: Format::Email,
        parts,
        by_source,
        body,
        warnings,
        partial,
        tool: Some(ToolId::Reader),
        also,
        part_headings: 0,
        charged,
        model,
    })
}
