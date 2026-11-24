//! `output: "json"` (document-conversion design §7.1): a [`Doc`] in its wire
//! form, [`DocPart`]s of [`DocBlock`]s, built from the same intermediate the
//! markdown and the text are written from ([`super::doc::render`]).
//!
//! Boxes are rounded to a tenth of a point: poppler's six decimals say
//! nothing a reader can use, and they would double the answer's size.

use lmgw_api_types::documents::structure::{
    BlockKind as WireKind, BlockSource, DocBlock, DocCell, DocPart, DocTable, PartKind as WireParts,
};

use super::doc::{Block, BlockKind, Doc, Part, PartKind, Rect, Source, Table};

fn round(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

fn source(s: Source) -> BlockSource {
    match s {
        Source::Native => BlockSource::Native,
        Source::TextLayer => BlockSource::TextLayer,
        Source::Ocr => BlockSource::Ocr,
        Source::Vision => BlockSource::Vision,
        Source::Onnx => BlockSource::Onnx,
        Source::Asr => BlockSource::Asr,
    }
}

fn bbox(r: &Rect) -> [f64; 4] {
    [round(r.x0), round(r.y0), round(r.x1), round(r.y1)]
}

fn table(t: &Table) -> DocTable {
    DocTable {
        rows: t.rows,
        cols: t.cols,
        cells: t
            .cells
            .iter()
            .map(|c| DocCell {
                row: c.row,
                col: c.col,
                row_span: c.row_span,
                col_span: c.col_span,
                header: c.header,
                text: c.text.clone(),
            })
            .collect(),
    }
}

fn block(b: &Block) -> DocBlock {
    let (kind, level, tbl) = match &b.kind {
        BlockKind::Heading(n) => (WireKind::Heading, Some((*n).clamp(1, 6)), None),
        BlockKind::Paragraph => (WireKind::Paragraph, None, None),
        BlockKind::ListItem | BlockKind::NumberedItem(_) => (WireKind::ListItem, None, None),
        BlockKind::Table(t) => (WireKind::Table, None, Some(table(t))),
        BlockKind::Figure => (WireKind::Figure, None, None),
        BlockKind::Caption => (WireKind::Caption, None, None),
        BlockKind::Code => (WireKind::Code, None, None),
        BlockKind::Formula => (WireKind::Formula, None, None),
        BlockKind::PageHeader => (WireKind::PageHeader, None, None),
        BlockKind::PageFooter => (WireKind::PageFooter, None, None),
    };
    DocBlock {
        kind,
        level,
        // A figure has no text of its own (its caption is `caption`).
        text: (tbl.is_none() && !(kind == WireKind::Figure && b.text.is_empty())).then(|| match &b
            .kind
        {
            BlockKind::NumberedItem(marker) => format!("{marker} {}", b.text),
            _ => b.text.clone(),
        }),
        bbox: b.bbox.as_ref().map(bbox),
        source: source(b.source),
        table: tbl,
        picture: b.figure.as_ref().map(|f| super::pictures::id(f.index)),
        caption: b.figure.as_ref().and_then(|f| f.caption.clone()),
        description: b.figure.as_ref().and_then(|f| f.description.clone()),
    }
}

fn part(p: &Part) -> DocPart {
    DocPart {
        kind: match p.kind {
            PartKind::Page => WireParts::Page,
            PartKind::Slide => WireParts::Slide,
            PartKind::Sheet => WireParts::Sheet,
            PartKind::Document => WireParts::Document,
        },
        number: p.number,
        name: p.name.clone(),
        width_pt: p.size.map(|s| round(s.0)),
        height_pt: p.size.map(|s| round(s.1)),
        source: p.source.map(source),
        blocks: p.blocks.iter().map(block).collect(),
    }
}

/// `doc`'s parts in their wire form.
pub fn parts(doc: &Doc) -> Vec<DocPart> {
    doc.parts.iter().map(part).collect()
}

/// A writer that only counts.
struct Count(usize);

impl std::io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes `parts` take as JSON, what the request's output budget counts;
/// counted as written, nothing kept.
pub fn size(parts: &[DocPart]) -> usize {
    let mut c = Count(0);
    // Writing to a counter cannot fail, and the types serialise.
    let _ = serde_json::to_writer(&mut c, parts);
    c.0
}

#[cfg(test)]
mod tests {
    use super::super::doc::Cell;
    use super::*;

    #[test]
    fn the_wire_form_matches_the_design_s_shape() {
        let rect = Rect {
            x0: 72.04,
            y0: 60.0,
            x1: 410.333,
            y1: 88.96,
        };
        let doc = Doc {
            parts: vec![Part {
                kind: PartKind::Page,
                number: 1,
                name: None,
                size: Some((595.3039, 841.8898)),
                source: Some(Source::TextLayer),
                blocks: vec![
                    Block::new(
                        BlockKind::Heading(1),
                        "Example Report".into(),
                        Some(rect),
                        Source::TextLayer,
                    ),
                    Block::new(
                        BlockKind::Table(Table {
                            rows: 1,
                            cols: 1,
                            cells: vec![Cell {
                                row: 0,
                                col: 0,
                                row_span: 1,
                                col_span: 1,
                                header: true,
                                text: "Item".into(),
                            }],
                        }),
                        String::new(),
                        None,
                        Source::TextLayer,
                    ),
                ],
            }],
        };
        let p = parts(&doc);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{
                "kind": "page", "number": 1, "width_pt": 595.3, "height_pt": 841.9,
                "source": "text_layer",
                "blocks": [
                    {"kind": "heading", "level": 1, "text": "Example Report",
                     "bbox": [72.0, 60.0, 410.3, 89.0], "source": "text_layer"},
                    {"kind": "table", "bbox": null, "source": "text_layer",
                     "table": {"rows": 1, "cols": 1, "cells": [
                        {"row": 0, "col": 0, "row_span": 1, "col_span": 1, "header": true,
                         "text": "Item"}]}}
                ]
            }])
        );
        assert_eq!(size(&p), serde_json::to_vec(&p).unwrap().len());
    }
}
