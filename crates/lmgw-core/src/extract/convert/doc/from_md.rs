//! The blocks of a markdown text (design §7.1): what `json` output gives
//! for the files lmgw's own readers write as markdown (office files, text
//! files). The markdown is the readers' own dialect (`shape.rs`): ATX
//! headings, `- ` list items, `| a | b |` tables under a `| --- |`
//! separator, fenced code; a markdown file is read the same way (other
//! text files are one block as they are, `file.rs`). Blocks have no box
//! (these parts have no geometry) and are `native`.
//!
//! Only the bullet markers the readers write (`-`, `*`, `+`) open a list
//! item; a line starting `12. ` stays a paragraph with its number (a date,
//! a year, an ordered list's number), so no text is dropped. Paragraph
//! lines keep their indentation; they are only trimmed to be classified.
//!
//! One pass over the lines: a table is told by its separator on the line
//! after its first row and its rows are taken while they last, so no line
//! is read twice.

use super::super::shape::{cells, heading_level, is_separator};
use super::{Block, BlockKind, Cell, Source, Table};

/// A list item's text after its marker (`- `, `* `, `+ `).
fn list_item(line: &str) -> Option<&str> {
    let t = line.trim_start();
    ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m))
}

fn table(rows: Vec<Vec<String>>) -> Table {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut cells = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, text) in row.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            cells.push(Cell {
                row: r as u32,
                col: c as u32,
                row_span: 1,
                col_span: 1,
                header: r == 0,
                text: text.clone(),
            });
        }
    }
    Table {
        rows: rows.len() as u32,
        cols: cols as u32,
        cells,
    }
}

/// What is being gathered.
enum Open {
    None,
    Paragraph(Vec<String>),
    Item(String),
}

fn block(kind: BlockKind, text: String) -> Block {
    Block::new(kind, text, None, Source::Native)
}

fn close(open: &mut Open, out: &mut Vec<Block>) {
    match std::mem::replace(open, Open::None) {
        Open::None => {}
        Open::Paragraph(lines) => out.push(block(BlockKind::Paragraph, lines.join("\n"))),
        Open::Item(text) => out.push(block(BlockKind::ListItem, text)),
    }
}

/// The blocks of `md`, in order.
pub fn blocks(md: &str) -> Vec<Block> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut open = Open::None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        if t.is_empty() {
            close(&mut open, &mut out);
            i += 1;
            continue;
        }
        if t.starts_with("```") {
            close(&mut open, &mut out);
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            // The closing fence (or the end of the text).
            i += 1;
            out.push(block(BlockKind::Code, code.join("\n")));
            continue;
        }
        // A picture's marker (an office reader's, `office_media`) is a
        // paragraph of its own, never a line of the one around it: a pptx
        // slide's shapes are one line apart, and a marker joined to its
        // title would make no `figure` block (security round B).
        if crate::extract::office_media::read_marker(t).is_some() {
            close(&mut open, &mut out);
            out.push(block(BlockKind::Paragraph, t.to_string()));
            i += 1;
            continue;
        }
        if let Some(n) = heading_level(line) {
            close(&mut open, &mut out);
            let text = line[n..].trim().to_string();
            out.push(block(BlockKind::Heading(n as u8), text));
            i += 1;
            continue;
        }
        if let Some(first) = cells(line) {
            let sep = lines.get(i + 1).and_then(|l| cells(l));
            if sep.as_deref().is_some_and(is_separator) {
                close(&mut open, &mut out);
                let mut rows = vec![first];
                i += 2;
                while let Some(row) = lines.get(i).and_then(|l| cells(l)) {
                    rows.push(row);
                    i += 1;
                }
                out.push(block(BlockKind::Table(table(rows)), String::new()));
                continue;
            }
        }
        if let Some(rest) = list_item(line) {
            close(&mut open, &mut out);
            open = Open::Item(rest.trim().to_string());
            i += 1;
            continue;
        }
        match &mut open {
            // An indented line goes on with the item.
            Open::Item(text) if line.starts_with([' ', '\t']) => {
                text.push(' ');
                text.push_str(t);
            }
            Open::Paragraph(lines) => lines.push(line.to_string()),
            _ => {
                close(&mut open, &mut out);
                open = Open::Paragraph(vec![line.to_string()]);
            }
        }
        i += 1;
    }
    close(&mut open, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(b: &[Block]) -> Vec<String> {
        b.iter()
            .map(|b| match &b.kind {
                BlockKind::Heading(n) => format!("h{n}:{}", b.text),
                BlockKind::Paragraph => format!("p:{}", b.text),
                BlockKind::ListItem => format!("li:{}", b.text),
                BlockKind::Table(t) => format!("table:{}x{}", t.rows, t.cols),
                BlockKind::Code => format!("code:{}", b.text),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn office_markdown_becomes_blocks() {
        let md = "# Example Report\n\nIntro line one\nline two\n\n## Costs\n\n| Item | Cost |\n\
                  | --- | --- |\n| Paper \\| ink | 3 |\n| Pens | |\n\n- one\n  more\n- two\n\
                  1. first\n\n```\nlet x = 1;\n```\n#not-a-heading";
        let b = blocks(md);
        assert_eq!(
            kinds(&b),
            [
                "h1:Example Report",
                "p:Intro line one\nline two",
                "h2:Costs",
                "table:3x2",
                "li:one more",
                "li:two",
                "p:1. first",
                "code:let x = 1;",
                "p:#not-a-heading",
            ]
        );
        let BlockKind::Table(t) = &b[3].kind else {
            panic!()
        };
        assert_eq!(t.as_lines(), "Item\tCost\nPaper | ink\t3\nPens\t");
        assert!(t.cells.iter().filter(|c| c.row == 0).all(|c| c.header));
        assert!(b
            .iter()
            .all(|b| b.bbox.is_none() && b.source == Source::Native));
    }

    /// A paragraph starting like an ordered list keeps its number, and
    /// paragraph lines keep their indentation.
    #[test]
    fn numbers_and_indentation_are_kept() {
        let md = "12. März 2025 – Protokoll\n\n1999. The company grew\n  and grew\n\n1) one";
        assert_eq!(
            kinds(&blocks(md)),
            [
                "p:12. März 2025 – Protokoll",
                "p:1999. The company grew\n  and grew",
                "p:1) one"
            ]
        );
    }

    #[test]
    fn pipes_without_a_separator_are_a_paragraph_and_an_open_fence_ends_the_text() {
        assert_eq!(kinds(&blocks("| a |\n| b |")), ["p:| a |\n| b |"]);
        assert_eq!(kinds(&blocks("```\ncode")), ["code:code"]);
        assert!(blocks("").is_empty());
    }

    /// 100 000 pipe lines without a separator, and a table of 100 000 rows:
    /// each line is read once.
    #[test]
    fn long_runs_are_linear() {
        let started = std::time::Instant::now();
        let md = "| a |\n".repeat(100_000);
        assert_eq!(blocks(&md).len(), 1);
        let md = format!("| a |\n| --- |\n{md}");
        let b = blocks(&md);
        assert!(matches!(&b[0].kind, BlockKind::Table(t) if t.rows == 100_001));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_picture_marker_is_a_paragraph_of_its_own() {
        let m = crate::extract::office_media::marker(0);
        let b = blocks(&format!("Title\n{m}\nAfter"));
        assert_eq!(kinds(&b), ["p:Title", &format!("p:{m}"), "p:After"]);
    }
}
