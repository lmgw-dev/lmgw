//! A file's pictures in its conversion (design §5.3, WP10): a PDF's picture
//! runs beside its text ([`pdf_images`]) and their figures placed in its
//! blocks ([`pdf_place`]); the step between reading a file and writing its
//! output, where a PDF's are cropped and an office file's bytes reserved
//! ([`export`]), and where they are described (`super::super::describe`,
//! an image file made the one picture it is first); and the output that
//! writes each picture as the request asks, with its description and the
//! answer's `pictures` list ([`output`]).

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ErrorCode, Output, WarningCode};

use super::super::budget::{OutputBudget, Spend};
use super::super::caps::{Format, ToolId};
use super::super::describe;
use super::super::doc::{Block, BlockKind, Figure, Source};
use super::super::ocr::spool::Spool;
use super::super::options::Plan;
use super::super::pictures::pdf::{gate, runs, Images};
use super::super::pictures::{self, office, render, Found};
use super::super::pool;
use super::super::tools::{Fact, ToolFacts};
use super::super::watch::Watched;
use super::output::{doc_output, markdown_out, Out};
use super::{panicked, Body, Failure, Plain, Read};
use crate::extract::pdf::PDFTOPPM;

/// The poppler tool `name` beside the `pdftotext` the snapshot found.
fn poppler_tool(facts: &ToolFacts, name: &str) -> PathBuf {
    if let Some(Fact::Found(f)) = facts.fact(ToolId::Poppler) {
        for w in &f.watched {
            if let Watched::Binary {
                name: n,
                at: Some(at),
            } = w
            {
                if n == name {
                    return at.path.clone();
                }
            }
        }
        if let Some(dir) = f.located.path.parent() {
            let beside = dir.join(name);
            if beside.exists() {
                return beside;
            }
        }
    }
    PathBuf::from(name)
}

/// A PDF's pictures (`pictures::pdf::runs`) with the `pdftohtml` at `html`:
/// `None` for `output: text`, which does no picture work at all unless its
/// pictures are described (their descriptions are its text).
pub(super) async fn pdf_images(
    src: Arc<Spool>,
    plan: &Plan,
    facts: &ToolFacts,
    html: Option<PathBuf>,
) -> Option<Images> {
    if plan.output == Output::Text && !plan.pictures.describe {
        return None;
    }
    let Some(html) = html else {
        return Some(Images {
            missing: true,
            ..Default::default()
        });
    };
    let bins = runs::Bins {
        html,
        info: poppler_tool(facts, gate::PDFINFO),
        list: poppler_tool(facts, gate::PDFIMAGES),
    };
    let span = plan.pages.as_ref().and_then(|p| p.span());
    let min_area = plan.pictures.min_area;
    Some(Box::pin(runs::find(&bins, src, span, plan.pages.clone(), min_area)).await)
}

/// `r` (a PDF's text) with `images`' figures placed in its blocks, in a
/// slot of the pool.
pub(super) async fn pdf_place(mut r: Read, images: Images, plan: &Plan) -> Result<Read, Failure> {
    let Body::Doc(doc) = std::mem::replace(&mut r.body, Body::Doc(Default::default())) else {
        return Ok(r);
    };
    let s = plan.pictures;
    let (doc, found) = pool::blocking(move || {
        let mut doc = doc;
        let found = pictures::pdf::place(&mut doc, &images, &s);
        (doc, found)
    })
    .await
    .map_err(|e| panicked(Some(Format::Pdf), e))?;
    r.body = Body::Pictured(Box::new(Body::Doc(doc)), found);
    Ok(r)
}

/// Make the bytes of `r`'s pictures where the request asks for them (a
/// PDF's crops, an office file's media counted against `budget`), describe
/// them when it asks for that, and move the pictures' notes into the file's
/// warnings. Bytes made only for the descriptions are given back once the
/// calls are done.
pub(super) async fn export(
    r: &mut Read,
    src: &Bytes,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
) {
    // An image file is its one picture whenever pictures are exported; a
    // placeholder only for a description that was made.
    let image = r.format == Format::Image;
    let exported = pictures::writes_bytes(&plan.pictures);
    if image && (exported || plan.describe.is_some()) {
        image_file(r, src);
    }
    let Body::Pictured(_, found) = &mut r.body else {
        return;
    };
    // An e-mail's attachments exported their own pictures as each was read
    // (`file/more.rs`), so the message's are reserved already.
    if pictures::needs_bytes(&plan.pictures) && r.format != Format::Email {
        if r.format == Format::Pdf {
            let bin = poppler_tool(facts, PDFTOPPM);
            let cutter = super::super::reader::path(facts);
            // Its renders read the file from a spool of its own (the PDF
            // reader's went with its text): one write, no pipe per page.
            let spool = Spool::of(plan.spool_dir.as_deref(), src.clone()).await;
            render::pdf(
                &spool,
                found,
                &plan.pictures,
                budget,
                &bin,
                cutter.as_deref(),
            )
            .await;
            // lmgw's reader cut the crops: the answer's tools name it.
            if found.pictures.iter().any(|p| p.image.is_some()) && !r.also.contains(&ToolId::Reader)
            {
                r.also.push(ToolId::Reader);
            }
        } else {
            charge(found, budget, &plan.pictures);
        }
    }
    r.warnings.append(&mut found.warnings);
    // An e-mail's attachments described their own pictures as each was
    // read; their calls are in the message's stats already.
    if let (Some(d), false) = (&plan.describe, r.format == Format::Email) {
        let described = describe::run(d, found).await;
        r.model.absorb(described.calls, described.models);
        r.model.pictures_small = r
            .model
            .pictures_small
            .saturating_add(described.below_min_area);
        r.partial |= described.partial;
        r.warnings.extend(described.warnings);
        // Bytes the answer does not write were made for the calls alone:
        // what they held goes back to the request.
        if !exported {
            for p in &mut found.pictures {
                p.image = None;
            }
            budget.give_back(std::mem::take(&mut found.charged));
        }
    }
    if image && !exported {
        undescribed_image_out(r);
    }
}

/// An image file whose picture was made only to be described: when no
/// description was made, it is no picture of the answer (no placeholder
/// that only a failed description would leave).
fn undescribed_image_out(r: &mut Read) {
    let Body::Pictured(inner, found) = &mut r.body else {
        return;
    };
    if found.pictures.iter().any(|p| p.description.is_some()) {
        return;
    }
    let Body::Doc(doc) = std::mem::replace(&mut **inner, Body::Doc(Default::default())) else {
        return;
    };
    let mut doc = doc;
    if let Some(first) = doc.parts.first_mut() {
        if first
            .blocks
            .first()
            .is_some_and(|b| b.kind == BlockKind::Figure)
        {
            first.blocks.remove(0);
        }
    }
    r.body = Body::Doc(doc);
}

/// An image file, read by text recognition, as the one picture it is: a
/// `figure` block before its first part's text with the file's bytes as
/// stored; when lmgw cannot tell its type, the note that says it is not
/// exported.
fn image_file(r: &mut Read, src: &Bytes) {
    let Body::Doc(doc) = &mut r.body else {
        return;
    };
    let picture = match describe::image_file(src) {
        Ok(p) => p,
        Err(note) => {
            r.warnings.push(note);
            return;
        }
    };
    let Some(first) = doc.parts.first_mut() else {
        r.warnings.push(pictures::warning(
            WarningCode::PicturesPartial,
            "the image file was not exported as a picture: its text recognition gave no part \
             to place it in"
                .to_string(),
            Vec::new(),
            None,
        ));
        return;
    };
    let mut figure = Block::new(BlockKind::Figure, String::new(), None, Source::Native);
    figure.figure = Some(Box::new(Figure {
        index: 0,
        caption: None,
        description: None,
    }));
    first.blocks.insert(0, figure);
    let found = Found {
        pictures: vec![picture],
        ..Default::default()
    };
    let doc = std::mem::take(doc);
    r.body = Body::Pictured(Box::new(Body::Doc(doc)), found);
}

/// An office file's picture bytes reserved from the request's budget as
/// `s` says ([`pictures::held`]) before any is written; the ones past it
/// lose their bytes, named.
pub(super) fn charge(found: &mut Found, budget: &OutputBudget, s: &pictures::Settings) {
    let mut over = 0u32;
    for p in &mut found.pictures {
        let Some(img) = &p.image else {
            continue;
        };
        let n = pictures::held(
            img.bytes.len(),
            p.caption.as_ref().map_or(0, String::len),
            s,
        );
        if budget.try_take(n) {
            found.charged = found.charged.saturating_add(n);
        } else {
            p.image = None;
            p.missing = Some("MAX_UNCOMPRESSED_BYTES");
            over += 1;
        }
    }
    if over > 0 {
        found.warnings.push(pictures::warning(
            WarningCode::PicturesPartial,
            format!(
                "{over} pictures were not exported: the request's output passed {}, what \
                 all its files may produce together; {}",
                budget.named(),
                pictures::over_budget_advice(s)
            ),
            Vec::new(),
            None,
        ));
    }
}

/// The output of `inner` with `found`'s pictures written as the request
/// asks and the `pictures` list counted, every take using the pictures'
/// reservation and what the reading already `charged` first
/// (`budget::Spend`): the bytes count once, and what no take used is given
/// back.
pub(super) fn output(
    inner: Body,
    found: Found,
    plan: &Plan,
    budget: &OutputBudget,
    charged: u64,
) -> Result<Out, (ErrorCode, String)> {
    let mode = plan.pictures.mode;
    let spend = Spend::new(budget, found.charged.saturating_add(charged));
    let mut out = match inner {
        // A reader's markdown with markers: each made the picture's line
        // (dropped for text), then shaped as any reader's markdown.
        Body::Markdown(md, plain) => {
            let write = |i: u32, out: &mut String| {
                pictures::line(mode, i, found.pictures.get(i as usize), out);
            };
            // A described picture's description, after the picture and its
            // caption: a paragraph in markdown, its words in text.
            let description = |i: u32| {
                found
                    .pictures
                    .get(i as usize)
                    .and_then(|p| p.description.as_deref())
            };
            let as_md = |i: u32| description(i).map(describe::paragraph);
            let as_text = |i: u32| description(i).map(|d| d.trim().to_string());
            let shown = if plan.output == Output::Text {
                office::lines(&md, &found, None, Some(&as_text))
            } else {
                office::lines(&md, &found, Some(&write), Some(&as_md))
            };
            let text_form = (plan.also_text && plan.output == Output::Markdown)
                .then(|| office::lines(&md, &found, None, Some(&as_text)));
            drop(md);
            let mut out = markdown_out(shown, plain, plan, &spend)?;
            if let Some(t) = text_form {
                let t = match plain {
                    Plain::Markdown => t,
                    Plain::Derive => super::super::shape::Shaped::new(&t).plain(),
                };
                let had = out.text.as_ref().map_or(0, String::len);
                spend.give_back(had as u64);
                spend.take(t.len())?;
                out.text = Some(t);
            }
            out
        }
        Body::Doc(mut doc) => {
            if !found.markers.is_empty() {
                office::figures(&mut doc, &found);
            }
            describe::stamp(&mut doc, &found);
            doc_output(Body::Doc(doc), plan, &spend, Some(&found))?
        }
        other => doc_output(other, plan, &spend, Some(&found))?,
    };
    let list = pictures::wire(&found, &plan.pictures, plan.output);
    spend.take(pictures::wire_size(&list))?;
    out.pictures = list;
    Ok(out)
}
