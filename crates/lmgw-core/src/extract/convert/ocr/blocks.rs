//! A recognised page as a part of blocks (document-conversion design §5.1,
//! WP4): its lines go through the same layout pass as a PDF's text layer
//! ([`pdf_layout::layout`]), written in the form that pass reads
//! (`pdftotext -bbox-layout`'s pages, blocks, lines and words, in points),
//! so headings by size, columns in reading order and aligned cells as
//! tables come out of OCR as they do out of a text layer. Every block and
//! the part are then marked `ocr`.
//!
//! Pixels become points at the resolution the page was read at: the render's
//! for a PDF page, else what tesseract says it read the image at
//! (`scan_res`), else [`super::OCR_DPI`]. A line whose words stand further
//! apart than [`CELL_GAP_LINES`] line heights is split there, as poppler
//! splits a table's row into its cells, so the table pass sees them.
//!
//! **A line's size.** The layout pass ranks headings by the size of their
//! lines, which without font runs is the height of a line's box. A
//! recognised line's box is the height of its glyphs: a line of small
//! letters is lower than one with capitals and descenders in the same type,
//! which made body lines look like two sizes and counted 388 headings
//! against the reference's 159 on the corpus' PDFs (WP4's first
//! measurement). So a line is written with the height tesseract estimates
//! for its type (`x_size`), centred on its box, where tesseract gave one.
//!
//! **No headings by size yet.** With `x_size` the same PDFs still counted
//! 332 headings against 159 (2026-11-19, `ocr: force`, tesseract 5.5.4): a
//! recognised page's line sizes scatter more than a text layer's fonts do,
//! and each page is ranked on its own. Twice the reference's headings
//! mis-split any chunker downstream, so a recognised page's size-ranked
//! headings are written as paragraphs for now ([`part`]); its columns,
//! reading order and tables stay.
//!
//! Each page is laid out on its own: heading levels rank within the page,
//! and repeated page headers and footers are not looked for across
//! recognised pages yet.

use std::fmt::Write as _;

use quick_xml::escape::escape;

use super::super::doc::{Part, Rect, Source};
use super::super::pdf_layout;
use super::hocr::{Line, Page, Word};
use super::OCR_DPI;

/// A gap between two words of a line wider than this many line heights
/// splits the line: 2. Words of a justified line stand at most about one
/// height apart; a table's cells further.
pub const CELL_GAP_LINES: f32 = 2.0;

/// The page's lines split at their wide gaps: each piece with its own
/// block when it is not the line's first.
fn pieces(page: &Page) -> Vec<(u32, Rect, Vec<&Word>)> {
    let mut out = Vec::new();
    let mut extra: u32 = 0;
    // Blocks numbered past the page's own, for the split-off cells.
    let base = page.lines.iter().map(|l| l.block).max().unwrap_or(0);
    for l in &page.lines {
        let height = sized(l).height().max(1.0);
        let mut cur: Vec<&Word> = Vec::new();
        let mut block = l.block;
        for w in &l.words {
            if let Some(prev) = cur.last() {
                if w.rect.x0 - prev.rect.x1 > CELL_GAP_LINES * height {
                    out.push((block, span(l, &cur), std::mem::take(&mut cur)));
                    extra = extra.saturating_add(1);
                    block = base.saturating_add(extra);
                }
            }
            cur.push(w);
        }
        if !cur.is_empty() {
            out.push((block, span(l, &cur), cur));
        }
    }
    out
}

/// The line's box at the height of its type (module doc): `x_size` high
/// around the box's middle, or the box itself without one or when the line
/// is taller than wide (vertical text, whose size runs the other way).
fn sized(l: &Line) -> Rect {
    match l.x_size {
        Some(size) if l.rect.height() <= l.rect.width() => {
            let mid = l.rect.cy();
            Rect {
                y0: mid - size / 2.0,
                y1: mid + size / 2.0,
                ..l.rect
            }
        }
        _ => l.rect,
    }
}

/// The box of some of a line's words, the line's own height.
fn span(l: &Line, words: &[&Word]) -> Rect {
    let x = Rect::union_all(words.iter().map(|w| &w.rect)).unwrap_or(l.rect);
    let y = sized(l);
    Rect {
        x0: x.x0,
        y0: y.y0,
        x1: x.x1,
        y1: y.y1,
    }
}

fn rect_attrs(out: &mut String, r: &Rect, k: f32) {
    let _ = write!(
        out,
        " xMin=\"{:.2}\" yMin=\"{:.2}\" xMax=\"{:.2}\" yMax=\"{:.2}\"",
        r.x0 * k,
        r.y0 * k,
        r.x1 * k,
        r.y1 * k
    );
}

/// The page in `-bbox-layout`'s form, in points (`k` points a pixel). The
/// pass keeps poppler's block order where nothing else decides, so pieces
/// of one block stay together in the order tesseract read them.
fn bbox_xml(page: &Page, k: f32) -> String {
    let mut parts = pieces(page);
    // Pieces of one block together, the block's first piece deciding.
    let mut first_at: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (i, (b, _, _)) in parts.iter().enumerate() {
        first_at.entry(*b).or_insert(i);
    }
    parts.sort_by_key(|(b, _, _)| first_at.get(b).copied().unwrap_or(0));
    let mut out = String::new();
    let _ = write!(
        out,
        "<doc><page width=\"{:.2}\" height=\"{:.2}\"><flow>",
        page.width * k,
        page.height * k
    );
    let mut open: Option<u32> = None;
    for (block, rect, words) in &parts {
        if open != Some(*block) {
            if open.is_some() {
                out.push_str("</block>");
            }
            out.push_str("<block");
            rect_attrs(&mut out, rect, k);
            out.push('>');
            open = Some(*block);
        }
        out.push_str("<line");
        rect_attrs(&mut out, rect, k);
        out.push('>');
        for w in words {
            out.push_str("<word");
            rect_attrs(&mut out, &w.rect, k);
            out.push('>');
            out.push_str(&escape(w.text.as_str()));
            out.push_str("</word>");
        }
        out.push_str("</line>");
    }
    if open.is_some() {
        out.push_str("</block>");
    }
    out.push_str("</flow></page></doc>");
    out
}

/// The recognised `page` as part `number`, read at `dpi` (a render's), or
/// at what tesseract says it read the image at.
pub fn part(page: &Page, dpi: Option<u32>, number: u32) -> Result<Part, String> {
    let dpi = dpi
        .map(|d| d as f32)
        .or(page.dpi)
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(OCR_DPI as f32);
    let k = 72.0 / dpi;
    let xml = bbox_xml(page, k);
    let layout = pdf_layout::layout(xml.as_bytes(), None, number)?;
    let mut part = layout.doc.parts.into_iter().next().unwrap_or(Part {
        kind: super::super::doc::PartKind::Page,
        number,
        name: None,
        size: None,
        source: None,
        blocks: Vec::new(),
    });
    // A page tesseract read is read, words or none.
    part.source = Some(Source::Ocr);
    part.size = Some((page.width * k, page.height * k));
    for b in &mut part.blocks {
        b.source = Source::Ocr;
        // Not yet: headings ranked by size on a recognised page (module doc).
        if matches!(b.kind, super::super::doc::BlockKind::Heading(_)) {
            b.kind = super::super::doc::BlockKind::Paragraph;
        }
    }
    Ok(part)
}

#[cfg(test)]
mod tests {
    use super::super::super::doc::BlockKind;
    use super::*;

    fn word(x0: f32, x1: f32, y: f32, text: &str) -> Word {
        Word {
            rect: Rect {
                x0,
                y0: y,
                x1,
                y1: y + 40.0,
            },
            text: text.into(),
            confidence: Some(90.0),
        }
    }

    fn line(block: u32, words: Vec<Word>) -> Line {
        Line {
            rect: Rect::union_all(words.iter().map(|w| &w.rect)).unwrap(),
            block,
            x_size: None,
            words,
        }
    }

    /// Two body lines of one type, one of small letters only: their boxes
    /// differ, their type does not, and with tesseract's size neither is a
    /// heading; a line of larger type still is.
    #[test]
    fn a_lines_size_is_its_types_not_its_glyphs() {
        let body = |y: f32, h: f32, text: &str| Line {
            rect: Rect {
                x0: 300.0,
                y0: y,
                x1: 2000.0,
                y1: y + h,
            },
            block: 2,
            x_size: Some(45.0),
            words: vec![Word {
                rect: Rect {
                    x0: 300.0,
                    y0: y,
                    x1: 2000.0,
                    y1: y + h,
                },
                text: text.into(),
                confidence: Some(90.0),
            }],
        };
        let mut lines = vec![Line {
            block: 1,
            x_size: Some(90.0),
            ..body(300.0, 80.0, "Title")
        }];
        for i in 0..8 {
            // Alternating: capitals and descenders, then small letters only.
            let h = if i % 2 == 0 { 44.0 } else { 30.0 };
            lines.push(body(500.0 + 70.0 * i as f32, h, "some words of the body"));
        }
        let mut page = Page {
            width: 2480.0,
            height: 3508.0,
            dpi: Some(300.0),
            lines,
        };
        // The layout pass sees one size for the body and a larger title.
        let k = 72.0 / 300.0;
        let xml = bbox_xml(&page, k);
        let layout = pdf_layout::layout(xml.as_bytes(), None, 1).unwrap();
        let headings = |blocks: &[super::super::super::doc::Block]| {
            blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Heading(_)))
                .count()
        };
        assert_eq!(headings(&layout.doc.parts[0].blocks), 1);
        assert_eq!(sized(&page.lines[2]).height(), 45.0);
        // A recognised page's size-ranked headings are paragraphs for now.
        let p = part(&page, None, 1).unwrap();
        assert_eq!(headings(&p.blocks), 0);
        assert!(p.blocks.iter().any(|b| b.text.contains("Title")));
        // Without tesseract's size the boxes decide, as before.
        for l in &mut page.lines {
            l.x_size = None;
        }
        assert_eq!(sized(&page.lines[2]).height(), 30.0);
    }

    #[test]
    fn recognised_lines_become_ocr_blocks_in_points() {
        let page = Page {
            width: 2480.0,
            height: 3508.0,
            dpi: Some(300.0),
            lines: vec![
                line(
                    1,
                    vec![
                        word(300.0, 700.0, 300.0, "A"),
                        word(720.0, 900.0, 300.0, "<b>&"),
                    ],
                ),
                line(1, vec![word(300.0, 800.0, 360.0, "second")]),
            ],
        };
        let p = part(&page, None, 4).unwrap();
        assert_eq!(p.number, 4);
        assert_eq!(p.source, Some(Source::Ocr));
        let (w, h) = p.size.unwrap();
        assert!(
            (w - 595.2).abs() < 0.1 && (h - 841.9).abs() < 0.1,
            "{w} {h}"
        );
        assert!(p.blocks.iter().all(|b| b.source == Source::Ocr));
        let text: Vec<&str> = p.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(text.join("\n"), "A <b>& second");
        let bb = p.blocks[0].bbox.unwrap();
        assert!((bb.x0 - 72.0).abs() < 0.1, "{bb:?}");
    }

    #[test]
    fn wide_gaps_split_a_line_into_cells_and_a_grid_becomes_a_table() {
        let mut lines = Vec::new();
        for (i, row) in [
            ["Item", "Cost", "Count"],
            ["Pens", "4", "10"],
            ["Ink", "9", "2"],
            ["Pads", "3", "7"],
        ]
        .iter()
        .enumerate()
        {
            let y = 300.0 + 80.0 * i as f32;
            lines.push(line(
                i as u32 + 1,
                vec![
                    word(300.0, 500.0, y, row[0]),
                    word(1000.0, 1200.0, y, row[1]),
                    word(1700.0, 1900.0, y, row[2]),
                ],
            ));
        }
        let page = Page {
            width: 2480.0,
            height: 3508.0,
            dpi: Some(300.0),
            lines,
        };
        assert_eq!(pieces(&page).len(), 12);
        let p = part(&page, Some(300), 1).unwrap();
        let table = p.blocks.iter().find_map(|b| match &b.kind {
            BlockKind::Table(t) => Some(t),
            _ => None,
        });
        let t = table.expect("a table");
        assert_eq!((t.rows, t.cols), (4, 3));
    }

    #[test]
    fn an_empty_page_is_a_read_page_without_blocks() {
        let page = Page {
            width: 100.0,
            height: 100.0,
            dpi: None,
            lines: Vec::new(),
        };
        let p = part(&page, None, 2).unwrap();
        assert_eq!((p.source, p.blocks.len()), (Some(Source::Ocr), 0));
    }
}
