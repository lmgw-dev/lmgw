//! A file's output (design §7.1): its body written as the request asks
//! (markdown, text or json), counted against the request's output budget
//! through the file's [`Spend`], which holds the bytes its pictures
//! reserved before.

use lmgw_api_types::documents::convert::{ErrorCode, Output};
use lmgw_api_types::documents::structure::DocPart;

use super::super::budget::{OutputBudget, Spend};
use super::super::doc::render::{self, TableStyle};
use super::super::doc::{from_md, Block, BlockKind, Doc, Part, PartKind, Source};
use super::super::doc_json;
use super::super::options::Plan;
use super::super::shape::{Counts, Shaped};
use super::{pictured, Body, Plain};

/// A text file as one part: its markdown's blocks, or (not markdown: code,
/// YAML, CSV, plain text) one block of the text verbatim.
pub(super) fn text_doc(text: String, markdown: bool) -> Doc {
    let blocks = if markdown {
        from_md::blocks(&text)
    } else if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::new(BlockKind::Paragraph, text, None, Source::Native)]
    };
    Doc {
        parts: vec![Part {
            kind: PartKind::Document,
            number: 1,
            name: None,
            size: None,
            source: Some(Source::Native),
            blocks,
        }],
    }
}

/// A reader's markdown shaped as the request asks, with its counts.
fn shaped(markdown: String, plain: Plain, plan: &Plan) -> (Counts, String) {
    // The lines are classified once, for the counts and for the form asked.
    let lines = Shaped::new(&markdown);
    let shaped = match (plan.output, plain) {
        (Output::Text, Plain::Markdown) => None,
        (Output::Text, Plain::Derive) => Some(lines.plain()),
        _ if !plan.tables => Some(lines.tables_as_lines()),
        _ if plan.compact_tables => Some(lines.compact_tables()),
        _ => None,
    };
    let counts = lines.counts();
    drop(lines);
    (counts, shaped.unwrap_or(markdown))
}

/// What a file's output is: its content or its structure.
pub(super) struct Out {
    pub(super) counts: Counts,
    pub(super) content: Option<String>,
    pub(super) structure: Option<Vec<DocPart>>,
    /// The text form beside markdown content ([`Plan::also_text`]).
    pub(super) text: Option<String>,
    /// The answer's `pictures` list.
    pub(super) pictures: Vec<lmgw_api_types::documents::pictures::DocPicture>,
}

/// The output of `body` as `plan` asks, counted against `budget`: a
/// table's grid is charged before it is written ([`Doc::grid_cost`]), the
/// rest once it is. `charged` is what the reading already took of the
/// budget for this output (an e-mail's attachments, each charged as it was
/// read, `file/more.rs`): the takes use it first, like a picture's
/// reservation, so it counts once, and what they did not use is given back.
pub(super) fn output(
    body: Body,
    plan: &Plan,
    budget: &OutputBudget,
    charged: u64,
) -> Result<Out, (ErrorCode, String)> {
    let body = match body {
        Body::Pictured(inner, found) => {
            return pictured::output(*inner, found, plan, budget, charged)
        }
        other => other,
    };
    doc_output(body, plan, &Spend::new(budget, charged), None)
}

/// [`output`] of a body without pictures, or with `found`'s.
pub(super) fn doc_output(
    body: Body,
    plan: &Plan,
    budget: &Spend<'_>,
    found: Option<&super::super::pictures::Found>,
) -> Result<Out, (ErrorCode, String)> {
    let doc = match (plan.output, body) {
        (Output::Json, Body::Text { text, markdown }) => text_doc(text, markdown),
        (_, Body::Doc(doc)) => doc,
        (_, Body::Text { text, .. }) => return markdown_out(text, Plain::Markdown, plan, budget),
        (_, Body::Markdown(md, plain)) => return markdown_out(md, plain, plan, budget),
        (_, Body::Pictured(..)) => {
            return Err((
                ErrorCode::Unreadable,
                "a file's pictures were nested twice".to_string(),
            ))
        }
    };
    let mut doc = doc;
    let counts = doc.counts();
    // Json writes tables as cells unless they become lines.
    let grids = plan.output != Output::Json || !plan.tables;
    let reserved = if grids { doc.grid_cost() } else { 0 };
    budget.take_u64(reserved)?;
    let rest = |n: usize| budget.take_u64((n as u64).saturating_sub(reserved));
    if plan.output == Output::Json {
        if !plan.tables {
            doc.tables_as_lines();
        }
        let parts = doc_json::parts(&doc);
        rest(doc_json::size(&parts))?;
        return Ok(Out {
            counts,
            content: None,
            structure: Some(parts),
            text: None,
            pictures: Vec::new(),
        });
    }
    let sep = match &plan.page_break {
        Some(m) => format!("\n\n{m}\n\n"),
        None => "\n\n".to_string(),
    };
    // A described picture's description, after the picture and its caption
    // (`describe::after_blocks`).
    let described = found.filter(|f| f.pictures.iter().any(|p| p.description.is_some()));
    let after = |text: bool| described.map(|f| super::super::describe::after_blocks(&doc, f, text));
    let text_of = |doc: &super::super::doc::Doc| match after(true) {
        Some(map) => render::text_after(doc, &sep, &|b| {
            map.get(&(std::ptr::from_ref(b) as usize)).cloned()
        }),
        None => render::text(doc, &sep),
    };
    let content = if plan.output == Output::Text {
        text_of(&doc)
    } else {
        let style = if !plan.tables {
            TableStyle::Lines
        } else if plan.compact_tables {
            TableStyle::Compact
        } else {
            TableStyle::Padded
        };
        match found {
            Some(f) => {
                let mode = plan.pictures.mode;
                let line = |b: &Block, out: &mut String| {
                    let i = b.figure.as_ref().map_or(u32::MAX, |f| f.index);
                    super::super::pictures::line(mode, i, f.pictures.get(i as usize), out);
                };
                match after(false) {
                    Some(map) => render::markdown_after(&doc, style, &sep, &line, &|b| {
                        map.get(&(std::ptr::from_ref(b) as usize)).cloned()
                    }),
                    None => render::markdown_with(&doc, style, &sep, &line),
                }
            }
            None => render::markdown(&doc, style, &sep),
        }
    };
    rest(content.len())?;
    // Both forms asked: the text is the block model's own text form, the
    // one `output: text` writes, never the markdown stripped.
    let text = if plan.also_text && plan.output == Output::Markdown {
        let t = text_of(&doc);
        budget.take(t.len())?;
        Some(t)
    } else {
        None
    };
    Ok(Out {
        counts,
        content: Some(content),
        structure: None,
        text,
        pictures: Vec::new(),
    })
}

/// A reader's markdown (or a text file) as content, counted.
pub(super) fn markdown_out(
    md: String,
    plain: Plain,
    plan: &Plan,
    budget: &Spend<'_>,
) -> Result<Out, (ErrorCode, String)> {
    let kept = (plan.also_text && plan.output == Output::Markdown).then(|| md.clone());
    let (counts, content) = shaped(md, plain, plan);
    budget.take(content.len())?;
    let text = match kept {
        Some(md) => {
            let t = match plain {
                Plain::Markdown => md,
                Plain::Derive => Shaped::new(&md).plain(),
            };
            budget.take(t.len())?;
            Some(t)
        }
        None => None,
    };
    Ok(Out {
        counts,
        content: Some(content),
        structure: None,
        text,
        pictures: Vec::new(),
    })
}
