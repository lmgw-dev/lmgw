//! Tables from the text layer (design §5.1, tier 0). poppler writes a
//! table's columns as separate blocks (one per column, checked with a
//! LibreOffice table on 2026-11-18), so tables are found from lines, not
//! blocks. The page's lines are grouped into visual rows (lines whose
//! vertical centre lies inside the row's first line), each row's lines its
//! cells; then two passes:
//!
//! 1. **wide**: today's `table_like` rule on the geometry: a run of
//!    consecutive rows with [`TABLE_MIN_COLUMNS`] cells or more, no further
//!    apart than [`TABLE_ROW_GAP`] row heights, at least [`TABLE_MIN_ROWS`]
//!    long, is a table's core, and there must be as many columns;
//! 2. **narrow** (2 or 3 columns: a small table without borders, a
//!    label/value page such as a CV or an invoice): a run of at least
//!    [`TABLE_MIN_ROWS`] rows of 2 or 3 cells on one baseline (each cell's
//!    centre within [`TABLE_BASELINE_SHARE`] of the row's height), each cell
//!    in a column of its own, and no column of prose (half its cells
//!    [`TABLE_PROSE_WORDS`] words or more: the lines of a two-column page
//!    pair up too, but they are sentences).
//!
//! For both, the columns are the core cells' x-ranges merged where they
//! overlap; the rows just above and below join while every cell of theirs
//! lies in exactly one column (a header with empty cells); in a narrow
//! table a row of one cell in a column other than the first, right under a
//! row of the table, is that cell's text wrapping onto its next line (a long
//! value) and goes on with it. The first row is the header. A table must
//! fill at least [`TABLE_MIN_FILL`] of its grid (rows × columns) with
//! cells: lines staggered so that no two share a column are no table, and
//! their grid is never built.
//!
//! Not yet: a cell of a wide table whose text wraps onto a second line
//! becomes a row of its own, and a table beside a column of text takes that
//! column's lines as an extra column.

use super::super::doc::{Cell, Rect, Table};
use super::{Page, Role};
use crate::extract::vision_prompts::{TABLE_MIN_COLUMNS, TABLE_MIN_ROWS};

/// The most a table's rows may be apart, in row heights.
pub const TABLE_ROW_GAP: f32 = 1.5;

/// The least share of a table's grid (rows × columns) its cells fill.
pub const TABLE_MIN_FILL: f32 = 0.25;

/// How far a narrow table's cells may sit from the row's baseline: their
/// vertical centres at most this share of the row's first line's height
/// apart (the two columns of a page are often offset by a third of a line).
pub const TABLE_BASELINE_SHARE: f32 = 0.25;

/// A narrow table's cell with this many words or more reads as a line of
/// prose; a column where half the cells do is no table column.
pub const TABLE_PROSE_WORDS: usize = 5;

/// A table found on a page.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Found {
    pub rect: Rect,
    pub table: Table,
}

struct Row {
    /// Line indices, by their left edge.
    cells: Vec<usize>,
    rect: Rect,
    /// The line that opened the row (the topmost centre).
    anchor: Rect,
}

fn rows_of(page: &Page) -> Vec<Row> {
    let mut idx: Vec<usize> = (0..page.lines.len())
        .filter(|&i| page.lines[i].role == Role::Body)
        .collect();
    idx.sort_by(|&a, &b| page.lines[a].rect.cy().total_cmp(&page.lines[b].rect.cy()));
    let mut rows: Vec<Row> = Vec::new();
    for i in idx {
        let r = page.lines[i].rect;
        match rows.last_mut() {
            Some(row) if r.cy() >= row.anchor.y0 && r.cy() <= row.anchor.y1 => {
                row.cells.push(i);
                row.rect = row.rect.union(&r);
            }
            _ => rows.push(Row {
                cells: vec![i],
                rect: r,
                anchor: r,
            }),
        }
    }
    for row in &mut rows {
        row.cells
            .sort_by(|&a, &b| page.lines[a].rect.x0.total_cmp(&page.lines[b].rect.x0));
    }
    rows
}

fn close(a: &Row, b: &Row) -> bool {
    let gap = b.rect.y0 - a.rect.y1;
    gap <= TABLE_ROW_GAP * a.rect.height().max(b.rect.height())
}

/// The one column `x0..x1` lies in, or `None` (none, or more than one).
fn column_of(cols: &[(f32, f32)], x0: f32, x1: f32) -> Option<usize> {
    let first = cols.partition_point(|c| c.1 <= x0);
    // Two are enough to know it is more than one.
    let n = cols[first..]
        .iter()
        .take_while(|c| c.0 < x1)
        .take(2)
        .count();
    (n == 1).then_some(first)
}

/// Which pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Wide,
    Narrow,
}

/// A table's rows (the row's index, and whether it goes on with the row
/// above: a narrow table's wrapped cell) and its columns.
struct Cand {
    rows: Vec<(usize, bool)>,
    cols: Vec<(f32, f32)>,
}

/// Whether `row` can be one of a core's rows in `pass`.
fn core_row(page: &Page, row: &Row, pass: Pass) -> bool {
    match pass {
        Pass::Wide => row.cells.len() >= TABLE_MIN_COLUMNS,
        Pass::Narrow => {
            (2..TABLE_MIN_COLUMNS).contains(&row.cells.len())
                && row.cells.iter().all(|&c| {
                    let r = &page.lines[c].rect;
                    (r.cy() - row.anchor.cy()).abs()
                        <= TABLE_BASELINE_SHARE * row.anchor.height().max(0.0)
                })
        }
    }
}

/// The columns of `rows`: their cells' x-ranges merged where they overlap.
fn columns(page: &Page, rows: &[Row]) -> Vec<(f32, f32)> {
    let mut spans: Vec<(f32, f32)> = rows
        .iter()
        .flat_map(|r| {
            r.cells
                .iter()
                .map(|&c| (page.lines[c].rect.x0, page.lines[c].rect.x1))
        })
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cols: Vec<(f32, f32)> = Vec::new();
    for (a, b) in spans {
        match cols.last_mut() {
            Some(last) if a < last.1 => last.1 = last.1.max(b),
            _ => cols.push((a, b)),
        }
    }
    cols
}

/// Whether the core `rows` with `cols` is a narrow table: each row's cells
/// in columns of their own, and no column of prose.
fn narrow_ok(page: &Page, rows: &[Row], cols: &[(f32, f32)]) -> bool {
    if !(2..TABLE_MIN_COLUMNS).contains(&cols.len()) {
        return false;
    }
    let mut prose = vec![0usize; cols.len()];
    let mut cells = vec![0usize; cols.len()];
    for r in rows {
        let mut last = None;
        for &c in &r.cells {
            let l = &page.lines[c];
            let Some(col) = column_of(cols, l.rect.x0, l.rect.x1) else {
                return false;
            };
            if last == Some(col) {
                return false;
            }
            last = Some(col);
            cells[col] += 1;
            if l.text.split_whitespace().count() >= TABLE_PROSE_WORDS {
                prose[col] += 1;
            }
        }
    }
    prose.iter().zip(&cells).all(|(p, n)| p * 2 < *n)
}

/// The tables of one pass over `rows`; rows already a table's are `used`.
fn scan(page: &Page, rows: &[Row], used: &mut [bool], pass: Pass) -> Vec<Cand> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if used[i] || !core_row(page, &rows[i], pass) {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < rows.len()
            && !used[end]
            && core_row(page, &rows[end], pass)
            && close(&rows[end - 1], &rows[end])
        {
            end += 1;
        }
        if end - i < TABLE_MIN_ROWS {
            i = end;
            continue;
        }
        let cols = columns(page, &rows[i..end]);
        let ok = match pass {
            Pass::Wide => cols.len() >= TABLE_MIN_COLUMNS,
            Pass::Narrow => narrow_ok(page, &rows[i..end], &cols),
        };
        if !ok {
            i = end;
            continue;
        }
        let in_one = |c: usize| {
            let l = &page.lines[c].rect;
            column_of(&cols, l.x0, l.x1)
        };
        let fits = |r: &Row| r.cells.len() >= 2 && r.cells.iter().all(|&c| in_one(c).is_some());
        // A narrow table's wrapped cell: one cell, in a column past the first.
        let goes_on = |r: &Row| {
            pass == Pass::Narrow
                && r.cells.len() == 1
                && r.cells
                    .iter()
                    .all(|&c| in_one(c).is_some_and(|col| col > 0))
        };
        let mut start = i;
        while start > 0
            && !used[start - 1]
            && close(&rows[start - 1], &rows[start])
            && fits(&rows[start - 1])
        {
            start -= 1;
        }
        let mut members: Vec<(usize, bool)> = (start..end).map(|r| (r, false)).collect();
        while end < rows.len() && !used[end] && close(&rows[end - 1], &rows[end]) {
            if fits(&rows[end]) {
                members.push((end, false));
            } else if goes_on(&rows[end]) {
                members.push((end, true));
            } else {
                break;
            }
            end += 1;
        }
        for u in &mut used[start..end] {
            *u = true;
        }
        found.push(Cand {
            rows: members,
            cols,
        });
        i = end;
    }
    found
}

/// The table of `cand` and its lines, or `None` when its cells fill less
/// than [`TABLE_MIN_FILL`] of its grid.
fn build(page: &Page, rows: &[Row], cand: &Cand) -> Option<(Found, Vec<usize>)> {
    let cols = &cand.cols;
    let mut cells: Vec<Cell> = Vec::new();
    let mut lines = Vec::new();
    let mut rect: Option<Rect> = None;
    let mut row_no: Option<u32> = None;
    for &(ri, goes_on) in &cand.rows {
        if !goes_on || row_no.is_none() {
            row_no = Some(row_no.map_or(0, |x| x + 1));
        }
        let r = row_no.unwrap_or(0);
        for &c in &rows[ri].cells {
            let line = &page.lines[c];
            lines.push(c);
            rect = Some(rect.map_or(line.rect, |x| x.union(&line.rect)));
            let col = column_of(cols, line.rect.x0, line.rect.x1)
                .unwrap_or(0)
                .min(cols.len().saturating_sub(1)) as u32;
            // The same slot again (two lines in one cell, or a wrapped
            // value) goes on with its text. A row's lines come left to
            // right, so the slot is the last cell, or for a wrapped value
            // one of its row's few (a narrow table has at most three).
            let back = if goes_on { TABLE_MIN_COLUMNS } else { 1 };
            let at = cells
                .iter()
                .enumerate()
                .rev()
                .take(back)
                .take_while(|(_, x)| x.row == r)
                .find(|(_, x)| x.col == col)
                .map(|(k, _)| k);
            match at.and_then(|k| cells.get_mut(k)) {
                Some(x) => {
                    x.text.push(' ');
                    x.text.push_str(&line.text);
                }
                None => cells.push(Cell {
                    row: r,
                    col,
                    row_span: 1,
                    col_span: 1,
                    header: r == 0,
                    text: line.text.clone(),
                }),
            }
        }
    }
    let n_rows = row_no.map_or(0, |x| x + 1);
    let grid = f64::from(n_rows) * cols.len() as f64;
    if (cells.len() as f64) < f64::from(TABLE_MIN_FILL) * grid {
        return None;
    }
    cells.sort_by_key(|c| (c.row, c.col));
    Some((
        Found {
            rect: rect.unwrap_or_default(),
            table: Table {
                rows: n_rows,
                cols: cols.len() as u32,
                cells,
            },
        },
        lines,
    ))
}

/// Find the tables of `page` and mark their lines [`Role::Cell`].
pub(super) fn find(page: &mut Page) -> Vec<Found> {
    let rows = rows_of(page);
    let mut used = vec![false; rows.len()];
    let mut cands = scan(page, &rows, &mut used, Pass::Wide);
    cands.extend(scan(page, &rows, &mut used, Pass::Narrow));
    let mut out = Vec::new();
    for cand in &cands {
        let Some((found, lines)) = build(page, &rows, cand) else {
            continue;
        };
        let t = out.len() as u32;
        for c in lines {
            page.lines[c].role = Role::Cell(t);
        }
        out.push(found);
    }
    out
}
