//! An OTSL table's cells as a [`Table`], and an element's box.

use super::vocab::CellTok;
use super::GRID;
use crate::extract::convert::doc::{Cell, Rect, Table};

/// The box of four location tokens on a page of `size` points.
pub(super) fn bbox(locs: &[f32], size: Option<(f32, f32)>) -> Option<Rect> {
    let (w, h) = size?;
    let [x0, y0, x1, y1] = <[f32; 4]>::try_from(locs).ok()?;
    Some(Rect {
        x0: x0.min(x1) / GRID * w,
        y0: y0.min(y1) / GRID * h,
        x1: x0.max(x1) / GRID * w,
        y1: y0.max(y1) / GRID * h,
    })
}

/// A table from its OTSL rows, and how many joined cells carried text. A
/// joined cell (`lcel`, `ucel`, `xcel`) grows the span of the cell it
/// belongs to; text written after one (OTSL has none there) joins that
/// cell's text, or is a cell of its own when there is none to join. Only
/// the cell tokens the answer has are held, and a cell without text takes
/// no entry.
pub(super) fn table(rows: Vec<Vec<(CellTok, String)>>) -> (Table, u32) {
    let rows: Vec<_> = rows.into_iter().filter(|r| !r.is_empty()).collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut cells: Vec<Cell> = Vec::new();
    let mut misplaced = 0u32;
    // Which cell (its index in `cells`) owns each slot of the row above and
    // of this row.
    let mut above: Vec<Option<usize>> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let mut here: Vec<Option<usize>> = Vec::with_capacity(row.len());
        for (c, (tok, text)) in row.iter().enumerate() {
            let text = text.trim();
            let mut owner = match tok {
                CellTok::Left => c
                    .checked_sub(1)
                    .and_then(|l| here.get(l).copied().flatten()),
                CellTok::Up | CellTok::Cross => above.get(c).copied().flatten(),
                _ => None,
            };
            if tok.joined() && !text.is_empty() {
                misplaced += 1;
            }
            match owner.and_then(|i| cells.get_mut(i)) {
                Some(cell) => {
                    cell.col_span = cell.col_span.max(c as u32 + 1 - cell.col.min(c as u32));
                    cell.row_span = cell.row_span.max(r as u32 + 1 - cell.row.min(r as u32));
                    if !text.is_empty() {
                        cell.text.push(' ');
                        cell.text.push_str(text);
                    }
                }
                None if !text.is_empty() => {
                    cells.push(Cell {
                        row: r as u32,
                        col: c as u32,
                        row_span: 1,
                        col_span: 1,
                        header: *tok == CellTok::ColumnHeader,
                        text: text.to_string(),
                    });
                    owner = Some(cells.len() - 1);
                }
                None => {}
            }
            here.push(owner);
        }
        above = here;
    }
    let table = Table {
        rows: rows.len() as u32,
        cols: cols as u32,
        cells,
    };
    (table, misplaced)
}
