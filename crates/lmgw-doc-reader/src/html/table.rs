//! lmgw's table writer for htmd: a GFM table whose rows are written as they
//! are, each cell and each caption walked exactly once.
//!
//! htmd's own writer pads every cell to its column's widest cell, so its
//! output is rows × widest cell (a page of two megabytes could ask for a
//! hundred gigabytes), and it walks a table that ends with no rows a second
//! time, which doubles the work at every level of tables nested in captions.
//! Here the output is linear in the page: rows are not padded, and a row
//! shorter than the header is left short (GFM fills it with empty cells).
//!
//! - The header is the first row when it is in a `thead` or made of `th`
//!   cells; otherwise an empty header row is written, as GFM has no table
//!   without one.
//! - A caption is a paragraph above the table.
//! - A table inside a cell, or anywhere only inline content can stand, is
//!   written as its cells' text in a line: GFM cells hold no blocks.
//! - `rowspan` and `colspan` are not read yet: a spanning cell is written
//!   once, in its first position.

use htmd::element_handler::{Context, Element, HandlerResult, Handlers};
use htmd::node::Node;

/// One cell's content on one line, with `|` escaped.
fn cell(handlers: &dyn Handlers, node: &Node<'_>) -> String {
    let content = handlers.walk_children_content(node, Context::INLINE);
    let mut out = String::with_capacity(content.len());
    let mut prev = ' ';
    for word in content.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in word.chars() {
            if c == '|' && prev != '\\' {
                out.push('\\');
            }
            out.push(c);
            prev = c;
        }
        prev = ' ';
    }
    out
}

/// The cells of a row, and whether all of them are header cells.
fn row(handlers: &dyn Handlers, tr: &Node<'_>) -> (Vec<String>, bool) {
    let mut cells = Vec::new();
    let mut all_th = true;
    for c in tr.children().iter() {
        match c.tag_name() {
            Some("th") => cells.push(cell(handlers, &c)),
            Some("td") => {
                all_th = false;
                cells.push(cell(handlers, &c));
            }
            _ => {}
        }
    }
    let header = all_th && !cells.is_empty();
    (cells, header)
}

fn push_row(out: &mut String, cells: &[String], at_least: usize) {
    out.push('|');
    for c in cells {
        out.push(' ');
        out.push_str(c);
        out.push_str(" |");
    }
    for _ in cells.len()..at_least {
        out.push_str("  |");
    }
    out.push('\n');
}

/// The table `element` as markdown (module doc).
pub fn write(handlers: &dyn Handlers, element: &Element) -> Option<HandlerResult> {
    let mut captions: Vec<String> = Vec::new();
    // Each row with whether it may be the header.
    let mut rows: Vec<(Vec<String>, bool)> = Vec::new();
    for child in element.node.children().iter() {
        match child.tag_name() {
            Some("caption") => {
                let c = cell(handlers, &child);
                if !c.is_empty() {
                    captions.push(c);
                }
            }
            Some(section @ ("thead" | "tbody" | "tfoot")) => {
                let head = section == "thead";
                for tr in child.children().iter() {
                    if tr.tag_name() == Some("tr") {
                        let (cells, th) = row(handlers, &tr);
                        rows.push((cells, head || th));
                    }
                }
            }
            Some("tr") => rows.push(row(handlers, &child)),
            _ => {}
        }
    }
    rows.retain(|(cells, _)| !cells.is_empty());
    if element.context.is_inline() {
        // No block can stand here: the text, in reading order.
        let mut line: Vec<String> = captions;
        line.extend(rows.into_iter().flat_map(|(cells, _)| cells));
        line.retain(|c| !c.is_empty());
        if line.is_empty() {
            return None;
        }
        // Set off from the words around it; the cell joins on white space.
        return Some(format!(" {} ", line.join(" ")).into());
    }
    let mut out = String::from("\n\n");
    for c in &captions {
        out.push_str(c);
        out.push_str("\n\n");
    }
    if rows.is_empty() {
        if captions.is_empty() {
            return None;
        }
        return Some(out.into());
    }
    let columns = rows.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut body = rows.iter();
    match rows.first() {
        Some((first, true)) => {
            push_row(&mut out, first, columns);
            body.next();
        }
        _ => push_row(&mut out, &[], columns),
    }
    out.push('|');
    for _ in 0..columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for (cells, _) in body {
        push_row(&mut out, cells, 0);
    }
    out.push('\n');
    Some(out.into())
}

#[cfg(test)]
mod tests {
    use super::super::markdown;

    fn md(page: &str) -> String {
        markdown(page).unwrap().text
    }

    #[test]
    fn rows_are_written_unpadded_with_the_header_the_table_has() {
        let t = md(
            "<table><caption>Costs <b>2026</b></caption><thead><tr><td>Item</td><td>Cost</td>\
                    </tr></thead><tbody><tr><td>A very long cell indeed</td><td>4</td></tr>\
                    <tr><td>x</td></tr><tr><td>a|b</td><td>line<br>two</td><td>extra</td></tr>\
                    </tbody></table>",
        );
        assert_eq!(
            t,
            "Costs **2026**\n\n| Item | Cost |  |\n| --- | --- | --- |\n\
             | A very long cell indeed | 4 |\n| x |\n| a\\|b | line two | extra |"
        );
        // No header in the page: an empty one, the rows all body.
        let t = md("<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>");
        assert_eq!(t, "|  |  |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |");
        assert_eq!(md("<table></table><p>after</p>"), "after");
    }

    #[test]
    fn a_table_in_a_cell_is_its_text_in_the_cell() {
        let t = md(
            "<table><tr><th>Outer</th></tr><tr><td>before <table><caption>cap</caption>\
                    <tr><td>in</td><td>ner</td></tr></table> after</td></tr></table>",
        );
        assert_eq!(t, "| Outer |\n| --- |\n| before cap in ner after |");
    }

    /// The reviewed 3 KB page: tables nested in captions, each ending
    /// without rows. htmd's writer walked each level twice (2^60 calls);
    /// here every caption is walked once.
    #[test]
    fn tables_nested_in_captions_are_walked_once() {
        let n = 60;
        let page = format!(
            "{}x{}",
            "<table><caption>".repeat(n),
            "</caption><thead></thead></table>".repeat(n)
        );
        let started = std::time::Instant::now();
        let t = md(&page);
        assert_eq!(t, "x");
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    /// The reviewed padding page: one wide cell and many rows. The output
    /// is the size of the page, not rows × the widest cell.
    #[test]
    fn a_wide_cell_does_not_widen_every_row() {
        let wide = "A".repeat(1 << 20);
        let page = format!(
            "<table><tr><th>h<tr><td>{wide}{}</table>",
            "<tr><td>x".repeat(100_000)
        );
        let t = md(&page);
        assert!(t.len() < 3 * page.len(), "{} from {}", t.len(), page.len());
        assert!(t.ends_with("| x |"), "{}", &t[t.len() - 20..]);
    }
}
