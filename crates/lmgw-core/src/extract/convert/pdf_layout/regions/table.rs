//! A table region's lines as a grid. The model says where the table is;
//! its rows and columns are read from where the lines sit, as poppler's
//! geometry reads them ([`super::super::table`]), without having to decide
//! whether there is a table:
//!
//! - a row is the lines whose vertical centre lies inside the row's first
//!   line (each visual line of the table is a row);
//! - the columns are the x-ranges of the lines of the rows that have more
//!   than one, merged where they overlap; a line across several columns (a
//!   title over the table, a heading spanning columns) is put in the first
//!   column it covers and spans those it covers;
//! - lines of one row in one column are one cell, left to right;
//! - the first row is the header.
//!
//! A grid its cells fill less than WP3's [`TABLE_MIN_FILL`] of is no table
//! ([`fills`]): the region's lines are text, as WP3's finder refuses such
//! a grid, so a staggered region cannot make a grid many times its text.
//!
//! A cell that wraps onto a second line is a row of its own, and a header
//! over two lines is two rows: a table-structure model is not built yet
//! (WP8 build notes). Sorting aside, the pass is linear in the lines.

use super::super::super::doc::{Cell, Rect, Table};
use super::super::table::{Found, TABLE_MIN_FILL};
use super::super::Line;

/// How far two cells' x-ranges may overlap and still be two columns, in
/// points.
const COLUMN_SLACK_PT: f32 = 0.5;

/// The x-ranges of `spans` (sorted by their left edge) merged where they
/// overlap.
fn merged(spans: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut cols: Vec<(f32, f32)> = Vec::new();
    for &(a, b) in spans {
        match cols.last_mut() {
            Some(c) if a < c.1 - COLUMN_SLACK_PT => c.1 = c.1.max(b),
            _ => cols.push((a, b)),
        }
    }
    cols
}

/// The columns `x0..x1` covers: the first, and how many.
fn covered(cols: &[(f32, f32)], x0: f32, x1: f32) -> (usize, usize) {
    let first = cols
        .partition_point(|c| c.1 <= x0 + COLUMN_SLACK_PT)
        .min(cols.len().saturating_sub(1));
    let end = cols.partition_point(|c| c.0 < x1 - COLUMN_SLACK_PT);
    (first, end.saturating_sub(first).max(1))
}

/// Whether the grid of `lines` (indices into `all`) is filled enough to be
/// a table (module doc); a region without lines is (it is a picture of a
/// table).
pub(super) fn fills(all: &[Line], lines: &[usize]) -> bool {
    match build(all, lines, Rect::default()) {
        None => true,
        Some(f) => {
            let grid = f64::from(f.table.rows) * f64::from(f.table.cols);
            f.table.cells.len() as f64 >= f64::from(TABLE_MIN_FILL) * grid
        }
    }
}

/// The table of the region `rect` from its `lines` (indices into `all`);
/// `None` when it has none.
pub(super) fn build(all: &[Line], lines: &[usize], rect: Rect) -> Option<Found> {
    let mut ls: Vec<&Line> = lines.iter().filter_map(|&i| all.get(i)).collect();
    if ls.is_empty() {
        return None;
    }
    ls.sort_by(|a, b| a.rect.cy().total_cmp(&b.rect.cy()));
    let mut rows: Vec<Vec<&Line>> = Vec::new();
    let mut anchor = Rect::default();
    for l in ls {
        match rows.last_mut() {
            Some(row) if l.rect.cy() >= anchor.y0 && l.rect.cy() <= anchor.y1 => row.push(l),
            _ => {
                anchor = l.rect;
                rows.push(vec![l]);
            }
        }
    }
    // Columns from the rows with several cells; from every line when no
    // row has more than one.
    let spans = |several: bool| {
        let mut s: Vec<(f32, f32)> = rows
            .iter()
            .filter(|r| !several || r.len() > 1)
            .flatten()
            .map(|l| (l.rect.x0, l.rect.x1))
            .collect();
        s.sort_by(|a, b| a.0.total_cmp(&b.0));
        s
    };
    let mut cols = merged(&spans(true));
    if cols.is_empty() {
        cols = merged(&spans(false));
    }
    let mut cells: Vec<Cell> = Vec::new();
    for (r, row) in rows.iter_mut().enumerate() {
        row.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0));
        let row_no = u32::try_from(r).unwrap_or(u32::MAX);
        for l in row.iter() {
            let (col, span) = covered(&cols, l.rect.x0, l.rect.x1);
            let col = u32::try_from(col).unwrap_or(u32::MAX);
            match cells.last_mut() {
                Some(c) if c.row == row_no && c.col == col => {
                    c.text.push(' ');
                    c.text.push_str(&l.text);
                }
                _ => cells.push(Cell {
                    row: row_no,
                    col,
                    row_span: 1,
                    col_span: u32::try_from(span).unwrap_or(u32::MAX),
                    header: r == 0,
                    text: l.text.clone(),
                }),
            }
        }
    }
    Some(Found {
        rect,
        table: Table {
            rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            cols: u32::try_from(cols.len()).unwrap_or(u32::MAX),
            cells,
        },
    })
}
