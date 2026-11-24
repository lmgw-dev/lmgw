//! A [`Doc`] as markdown or plain text (design §7.1): blocks in reading
//! order separated by a blank line, parts by the request's `page_break`
//! marker (or a blank line). Repeated page headers and footers are left out
//! (json keeps them); parts that were not read write nothing.
//!
//! Markdown: `#` headings, `- ` list items (a numbered one, which only the
//! layout model's regions mark, with its own marker: `1. `), tables as
//! `| a | b |` rows under
//! a `| --- |` separator (or compact, or as lines of cells), code fenced,
//! formulas as `$$` blocks, a picture as docling's `<!-- image -->` line.
//! Plain text: the same words without the markup, tables as lines of cells
//! separated by tabs.

use super::{Block, BlockKind, Doc, Table};

/// How markdown writes a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStyle {
    /// `| a | b |`.
    Padded,
    /// `|a|b|` (`compact_tables`).
    Compact,
    /// Each row a line of its cells separated by tabs (`tables: false`).
    Lines,
}

/// A cell's text inside a markdown row: one line, `|` escaped.
fn cell(text: &str) -> String {
    text.replace(['\n', '\r'], " ").replace('|', "\\|")
}

fn table_md(t: &Table, style: TableStyle, out: &mut String) {
    if style == TableStyle::Lines {
        out.push_str(&t.as_lines());
        return;
    }
    let (open, gap, close, dash) = match style {
        TableStyle::Compact => ("|", "|", "|", "---|"),
        _ => ("| ", " | ", " |", " --- |"),
    };
    // Row by row from the sparse cells: the grid is never held.
    t.for_each_row(|r, cells| {
        if r > 0 {
            out.push('\n');
        }
        out.push_str(open);
        t.write_row(cells, gap, out, |text, o| o.push_str(&cell(text)));
        out.push_str(close);
        if r == 0 {
            out.push_str("\n|");
            for _ in 0..t.cols {
                out.push_str(dash);
            }
        }
    });
}

/// A paragraph line that markdown would read as markup, escaped where the
/// markup starts: a heading (`#`), a whole line in pipes (a table row), a
/// code fence (`` ``` ``, `~~~`), HTML (`<`: a comment or a block tag would
/// hide or forge what follows), a quote (`>`), a list marker (`- `, `* `,
/// `+ `, and `1. `/`1) ` with the backslash before the dot), and a line of
/// only `-`, `=`, `*` or `_` (a thematic break, or a setext underline that
/// would make the line above a heading). Leading spaces go (four would make
/// a code block).
pub(crate) fn escaped(text: &str) -> String {
    let t = text.trim_start();
    let first = t.chars().next();
    let marker = |c: char| {
        t.strip_prefix(c)
            .is_some_and(|r| r.is_empty() || r.starts_with(' '))
    };
    let rule = !t.is_empty()
        && t.chars()
            .all(|c| matches!(c, '-' | '=' | '*' | '_' | ' ' | '\t'))
        && t.chars().any(|c| c != ' ' && c != '\t');
    let markup = matches!(first, Some('#' | '<' | '>'))
        || t.starts_with("```")
        || t.starts_with("~~~")
        || (t.starts_with('|') && t.trim_end().ends_with('|'))
        || marker('-')
        || marker('*')
        || marker('+')
        || rule;
    if markup {
        return format!("\\{t}");
    }
    // An ordinal start: up to nine digits, then `.` or `)`, then a space or
    // the end. CommonMark escapes punctuation only, so the backslash goes
    // before the dot.
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &t[digits..];
        let after = rest.get(1..).unwrap_or("");
        if (rest.starts_with('.') || rest.starts_with(')'))
            && (after.is_empty() || after.starts_with(' '))
        {
            return format!("{}\\{rest}", &t[..digits]);
        }
    }
    t.to_string()
}

/// How markdown writes a figure: the picture's line (`super::super::pictures`
/// writes placeholder, embedded and referenced pictures).
pub type FigureLine<'a> = &'a dyn Fn(&Block, &mut String);

/// docling's placeholder line, what a figure is without a picture writer.
pub const PLACEHOLDER: &str = "<!-- image -->";

fn placeholder(_: &Block, out: &mut String) {
    out.push_str(PLACEHOLDER);
}

/// The fence a code block is written in: three backticks, or one more than
/// the longest run of backticks that starts a line of `code`, so no line of
/// the code can close the block and turn what follows into markdown (a
/// model's answer can hold any line; CommonMark closes a fence only with a
/// line of at least as many backticks).
fn code_fence(code: &str) -> String {
    let longest = code
        .lines()
        .map(|l| l.trim_start().bytes().take_while(|b| *b == b'`').count())
        .max()
        .unwrap_or(0);
    "`".repeat(longest.saturating_add(1).max(3))
}

/// One block as markdown; `false` for what markdown leaves out.
fn block_md(b: &Block, style: TableStyle, figure: FigureLine<'_>, out: &mut String) -> bool {
    match &b.kind {
        BlockKind::Heading(n) => {
            let n = usize::from((*n).clamp(1, 6));
            out.push_str(&"#".repeat(n));
            out.push(' ');
            out.push_str(&b.text.replace('\n', " "));
        }
        BlockKind::Paragraph | BlockKind::Caption => {
            let lines: Vec<String> = b.text.lines().map(escaped).collect();
            out.push_str(&lines.join("\n"));
        }
        BlockKind::ListItem => {
            out.push_str("- ");
            out.push_str(&escaped(&b.text.replace('\n', " ")));
        }
        BlockKind::NumberedItem(marker) => {
            out.push_str(marker);
            out.push(' ');
            out.push_str(&escaped(&b.text.replace('\n', " ")));
        }
        BlockKind::Table(t) => table_md(t, style, out),
        BlockKind::Code => {
            let fence = code_fence(&b.text);
            out.push_str(&fence);
            out.push('\n');
            out.push_str(&b.text);
            out.push('\n');
            out.push_str(&fence);
        }
        // A formula whose text holds `$$` would end its own block there and
        // leave the rest as markdown: written as the text it is instead.
        BlockKind::Formula if b.text.contains("$$") => {
            let lines: Vec<String> = b.text.lines().map(escaped).collect();
            out.push_str(&lines.join("\n"));
        }
        BlockKind::Formula => {
            out.push_str("$$\n");
            out.push_str(&b.text);
            out.push_str("\n$$");
        }
        BlockKind::Figure => figure(b, out),
        BlockKind::PageHeader | BlockKind::PageFooter => return false,
    }
    true
}

/// One block as plain text; `false` for what text leaves out.
fn block_text(b: &Block, out: &mut String) -> bool {
    match &b.kind {
        BlockKind::ListItem => {
            out.push_str("- ");
            out.push_str(&b.text);
        }
        BlockKind::NumberedItem(marker) => {
            out.push_str(marker);
            out.push(' ');
            out.push_str(&b.text);
        }
        BlockKind::Table(t) => out.push_str(&t.as_lines()),
        BlockKind::Figure | BlockKind::PageHeader | BlockKind::PageFooter => return false,
        _ => out.push_str(&b.text),
    }
    true
}

/// Write each part's blocks through `one`, blocks separated by a blank
/// line, written parts by `sep`.
fn write(doc: &Doc, sep: &str, mut one: impl FnMut(&Block, &mut String) -> bool) -> String {
    let mut out = String::new();
    let mut wrote_part = false;
    for p in &doc.parts {
        let mut page = String::new();
        for b in &p.blocks {
            let at = page.len();
            if !page.is_empty() {
                page.push_str("\n\n");
            }
            if !one(b, &mut page) {
                page.truncate(at);
            }
        }
        if page.is_empty() {
            continue;
        }
        if wrote_part {
            out.push_str(sep);
        }
        out.push_str(&page);
        wrote_part = true;
    }
    out
}

/// The markdown; parts separated by `sep`.
pub fn markdown(doc: &Doc, style: TableStyle, sep: &str) -> String {
    markdown_with(doc, style, sep, &placeholder)
}

/// [`markdown`] with each figure written by `figure`.
pub fn markdown_with(doc: &Doc, style: TableStyle, sep: &str, figure: FigureLine<'_>) -> String {
    write(doc, sep, |b, out| block_md(b, style, figure, out))
}

/// The plain text; parts separated by `sep`.
pub fn text(doc: &Doc, sep: &str) -> String {
    write(doc, sep, block_text)
}

/// What is written after a block, when anything is (a picture's
/// description after the picture or its caption, `describe::after_blocks`).
pub type After<'a> = &'a dyn Fn(&Block) -> Option<String>;

/// `one` with what `after` says written after the block: a blank line
/// between them, or alone when the block itself writes nothing.
fn then<'a>(
    mut one: impl FnMut(&Block, &mut String) -> bool + 'a,
    after: After<'a>,
) -> impl FnMut(&Block, &mut String) -> bool + 'a {
    move |b, out| {
        let at = out.len();
        let wrote = one(b, out);
        let Some(more) = after(b) else {
            return wrote;
        };
        if wrote {
            out.push_str("\n\n");
        } else {
            out.truncate(at);
        }
        out.push_str(&more);
        true
    }
}

/// [`markdown_with`], with `after` written after the blocks it names.
pub fn markdown_after(
    doc: &Doc,
    style: TableStyle,
    sep: &str,
    figure: FigureLine<'_>,
    after: After<'_>,
) -> String {
    write(
        doc,
        sep,
        then(|b, out| block_md(b, style, figure, out), after),
    )
}

/// [`text`], with `after` written after the blocks it names.
pub fn text_after(doc: &Doc, sep: &str, after: After<'_>) -> String {
    write(doc, sep, then(block_text, after))
}

#[cfg(test)]
mod tests {
    use super::super::{Cell, Part, PartKind, Source};
    use super::*;

    fn b(kind: BlockKind, text: &str) -> Block {
        Block::new(kind, text.into(), None, Source::TextLayer)
    }

    fn page(n: u32, blocks: Vec<Block>) -> Part {
        Part {
            kind: PartKind::Page,
            number: n,
            name: None,
            size: None,
            source: Some(Source::TextLayer),
            blocks,
        }
    }

    fn table() -> Table {
        let c = |row, col, text: &str| Cell {
            row,
            col,
            row_span: 1,
            col_span: 1,
            header: row == 0,
            text: text.into(),
        };
        Table {
            rows: 2,
            cols: 2,
            cells: vec![
                c(0, 0, "Item"),
                c(0, 1, "Cost"),
                c(1, 0, "Pa|per"),
                c(1, 1, "3"),
            ],
        }
    }

    fn doc() -> Doc {
        Doc {
            parts: vec![
                page(
                    1,
                    vec![
                        b(BlockKind::PageHeader, "Example Corp"),
                        b(BlockKind::Heading(1), "Example Report"),
                        b(BlockKind::Paragraph, "# not a heading"),
                        b(BlockKind::ListItem, "one"),
                        b(BlockKind::Table(table()), ""),
                        b(BlockKind::PageFooter, "Page 1"),
                    ],
                ),
                page(2, vec![]),
                page(3, vec![b(BlockKind::Paragraph, "End.")]),
            ],
        }
    }

    #[test]
    fn markdown_writes_blocks_and_leaves_out_headers_and_unread_pages() {
        let md = markdown(&doc(), TableStyle::Padded, "\n\n---\n\n");
        assert_eq!(
            md,
            "# Example Report\n\n\\# not a heading\n\n- one\n\n| Item | Cost |\n| --- | --- |\n\
             | Pa\\|per | 3 |\n\n---\n\nEnd."
        );
        let compact = markdown(&doc(), TableStyle::Compact, "\n\n");
        assert!(
            compact.contains("|Item|Cost|\n|---|---|\n|Pa\\|per|3|"),
            "{compact}"
        );
        let lines = markdown(&doc(), TableStyle::Lines, "\n\n");
        assert!(lines.contains("Item\tCost\nPa|per\t3"), "{lines}");
    }

    #[test]
    fn text_has_the_words_without_the_markup() {
        let t = text(&doc(), "\n\n");
        assert_eq!(
            t,
            "Example Report\n\n# not a heading\n\n- one\n\nItem\tCost\nPa|per\t3\n\nEnd."
        );
    }

    /// Review nit 3: a code or formula block's text is a model's (or a
    /// file's) and can hold a fence or a `$$` line; neither may end the
    /// block and leave the rest as live markdown.
    #[test]
    fn a_fence_or_dollars_inside_a_block_cannot_end_it() {
        let hostile = "x = 1\n```\n# Forged heading\n<!-- hidden -->\n  ````\ny";
        let d = Doc {
            parts: vec![page(1, vec![b(BlockKind::Code, hostile)])],
        };
        let md = markdown(&d, TableStyle::Padded, "");
        // One more backtick than the longest run that starts a line.
        assert_eq!(md, format!("`````\n{hostile}\n`````"));
        // Ordinary code keeps the three backticks, a run inside a line too.
        for plain in ["let x = 1;", "say `hi` and ``` inline"] {
            let d = Doc {
                parts: vec![page(1, vec![b(BlockKind::Code, plain)])],
            };
            assert_eq!(
                markdown(&d, TableStyle::Padded, ""),
                format!("```\n{plain}\n```")
            );
        }
        // A formula holding `$$` is written as escaped text, not as a block
        // its own text could close.
        let d = Doc {
            parts: vec![page(
                1,
                vec![
                    b(BlockKind::Formula, "E = mc^2"),
                    b(
                        BlockKind::Formula,
                        "x\n$$\n# Forged heading\n<!-- hidden -->",
                    ),
                ],
            )],
        };
        assert_eq!(
            markdown(&d, TableStyle::Padded, ""),
            "$$\nE = mc^2\n$$\n\nx\n$$\n\\# Forged heading\n\\<!-- hidden -->"
        );
    }

    /// What would open markup in markdown is escaped where it starts, so a
    /// PDF line starting with a fence, HTML, a quote, a list marker or an
    /// ordinal stays the text it is.
    #[test]
    fn markup_at_a_line_start_is_escaped() {
        for (line, want) in [
            ("```rust", "\\```rust"),
            ("~~~", "\\~~~"),
            ("<!-- hidden -->", "\\<!-- hidden -->"),
            ("<div>forged</div>", "\\<div>forged</div>"),
            ("> quoted", "\\> quoted"),
            ("- dash", "\\- dash"),
            ("+ plus", "\\+ plus"),
            ("* star", "\\* star"),
            ("1999. The company grew", "1999\\. The company grew"),
            ("12) next", "12\\) next"),
            ("1.", "1\\."),
            ("---", "\\---"),
            ("= =", "\\= ="),
            ("#tag", "\\#tag"),
            ("| a | b |", "\\| a | b |"),
            ("    indented", "indented"),
            ("3.5 percent", "3.5 percent"),
            ("1234567890. long", "1234567890. long"),
            ("-5 % less", "-5 % less"),
            ("a < b", "a < b"),
        ] {
            assert_eq!(escaped(line), want, "{line}");
        }
        let d = Doc {
            parts: vec![page(
                1,
                vec![
                    b(BlockKind::Paragraph, "```\nnot code"),
                    b(BlockKind::ListItem, "<!-- x -->"),
                ],
            )],
        };
        assert_eq!(
            markdown(&d, TableStyle::Padded, ""),
            "\\```\nnot code\n\n- \\<!-- x -->"
        );
    }

    /// Only a block the layout model's regions marked as numbered is an
    /// ordered item; a bullet item whose text starts with a number (a PDF's
    /// "• 24. Dezember", a markdown file's `- 1. x`) stays a bullet item
    /// with its ordinal escaped, as before the model, and the rest of a
    /// numbered item is escaped like any item's text.
    #[test]
    fn only_a_marked_item_is_numbered() {
        let d = Doc {
            parts: vec![page(
                1,
                vec![
                    b(BlockKind::ListItem, "24. Dezember: example day"),
                    b(BlockKind::ListItem, "1. x"),
                    b(BlockKind::NumberedItem("3.".into()), "Third"),
                    b(BlockKind::NumberedItem("4)".into()), "# not a heading"),
                    b(BlockKind::NumberedItem("5.".into()), "```\nnot code"),
                ],
            )],
        };
        assert_eq!(
            markdown(&d, TableStyle::Padded, ""),
            "- 24\\. Dezember: example day\n\n- 1\\. x\n\n3. Third\n\n4) \\# not a \
             heading\n\n5. \\``` not code"
        );
        assert_eq!(
            text(&d, ""),
            "- 24. Dezember: example day\n\n- 1. x\n\n3. Third\n\n4) # not a heading\n\n\
             5. ```\nnot code"
        );
        // A markdown file's own `- 1. x` comes back as it went in.
        let md = Doc {
            parts: vec![page(1, super::super::from_md::blocks("- 1. x\n- 2) y\n"))],
        };
        assert_eq!(
            markdown(&md, TableStyle::Padded, ""),
            "- 1\\. x\n\n- 2\\) y"
        );
    }

    #[test]
    fn a_part_of_only_headers_writes_nothing_and_no_separator() {
        let d = Doc {
            parts: vec![
                page(1, vec![b(BlockKind::Paragraph, "A")]),
                page(2, vec![b(BlockKind::PageHeader, "H")]),
                page(3, vec![b(BlockKind::Paragraph, "B")]),
            ],
        };
        assert_eq!(markdown(&d, TableStyle::Padded, "|SEP|"), "A|SEP|B");
    }
}
