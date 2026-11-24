//! Reading order (design §5.1): what is left of poppler's blocks (headers,
//! footers and table cells taken out) and the tables, as units, sorted
//! column by column where the page has columns, else top to bottom.
//!
//! Columns: the x-ranges of the narrow units (at most [`COLUMN_MAX_SHARE`]
//! of the text's width) that have another narrow unit beside them
//! ([`beside`]) are merged; the gaps of at least [`COLUMN_GAP_MIN_PT`]
//! between them separate bands. A band is a column only when it is at least
//! [`COLUMN_MIN_WIDTH_SHARE`] of the text's width and its text sits beside
//! its neighbour's (their vertical extents overlap by
//! [`COLUMN_MIN_OVERLAP_PT`] or more); otherwise it joins its neighbour, so a
//! heading on the left and a caption in the middle, one under the other,
//! are no columns.
//!
//! Sections: a unit wider than its band (a title, an abstract across the
//! columns), and a narrow unit with nothing beside it that lies in no band
//! (a centred author line, an "Abstract" heading or a caption crossing the
//! gutter), separates the page into sections; each section is read band by
//! band, each band top to bottom. A unit with nothing beside it inside a
//! band (a column's last paragraph, where the other column ended higher)
//! stays in its column.
//!
//! Side by side but no columns: where a band too narrow to be a column was
//! joined to the one beside it (labels beside their values, a table's
//! columns, line numbers beside text), the units of the band that overlap
//! vertically are read row by row, line beside line, when at least
//! [`ROWWISE_MIN_PAIRED`] of their visual rows, and at least half of them,
//! hold lines of more than one unit; otherwise top to bottom, left to right.
//!
//! Every step sorts or sweeps once: `n log n` in the units and lines.

mod beside;

use super::super::doc::Rect;
use super::table::Found;
use super::{Line, Page, Role};

/// A unit at most this share of the text's width is narrow (can sit in a
/// column).
pub const COLUMN_MAX_SHARE: f32 = 0.6;

/// The least gap between two columns, in points.
pub const COLUMN_GAP_MIN_PT: f32 = 8.0;

/// A column's least width, as a share of the text's width.
pub const COLUMN_MIN_WIDTH_SHARE: f32 = 0.2;

/// How far two columns' text must overlap vertically, in points (two lines
/// of ordinary text).
pub const COLUMN_MIN_OVERLAP_PT: f32 = 20.0;

/// The fewest visual rows with lines of more than one unit that make
/// side-by-side units read row by row.
pub const ROWWISE_MIN_PAIRED: usize = 2;

/// What a unit is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum What {
    /// Lines of one poppler block, in poppler's order (indices into the
    /// page's lines).
    Lines(Vec<usize>),
    /// Side-by-side blocks read row by row: each visual row's lines, left
    /// to right.
    Rows(Vec<Vec<usize>>),
    Table(Found),
    /// A layout model's region, by its index ([`super::regions`]): ordered
    /// like any unit, never read row by row with another.
    Region(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Unit {
    pub rect: Rect,
    pub what: What,
}

#[derive(Debug, Clone, Copy)]
struct Band {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    /// A band too narrow to be a column was joined to it side by side.
    rowwise: bool,
}

impl Band {
    fn of(r: &Rect) -> Self {
        Self {
            x0: r.x0,
            x1: r.x1,
            y0: r.y0,
            y1: r.y1,
            rowwise: false,
        }
    }

    fn join(&mut self, o: &Band) {
        self.x0 = self.x0.min(o.x0);
        self.x1 = self.x1.max(o.x1);
        self.y0 = self.y0.min(o.y0);
        self.y1 = self.y1.max(o.y1);
        self.rowwise |= o.rowwise;
    }

    fn overlap(&self, o: &Band) -> f32 {
        self.y1.min(o.y1) - self.y0.max(o.y0)
    }
}

/// The bands of the narrow units that have one beside them.
fn bands(units: &[Unit], width: f32, narrow: &[bool], beside: &[bool]) -> Vec<Band> {
    let mut cols: Vec<&Rect> = units
        .iter()
        .enumerate()
        .filter(|(i, _)| narrow[*i] && beside[*i])
        .map(|(_, u)| &u.rect)
        .collect();
    cols.sort_by(|a, b| a.x0.total_cmp(&b.x0));
    let mut merged: Vec<Band> = Vec::new();
    for r in cols {
        match merged.last_mut() {
            Some(b) if r.x0 < b.x1 + COLUMN_GAP_MIN_PT => b.join(&Band::of(r)),
            _ => merged.push(Band::of(r)),
        }
    }
    let min_w = width * COLUMN_MIN_WIDTH_SHARE;
    let wide = |b: &Band| b.x1 - b.x0 >= min_w;
    let mut out: Vec<Band> = Vec::new();
    for b in merged {
        match out.last_mut() {
            Some(cur) => {
                let side_by_side = cur.overlap(&b) >= COLUMN_MIN_OVERLAP_PT;
                if side_by_side && wide(cur) && wide(&b) {
                    out.push(b);
                } else {
                    // Too narrow beside it: side by side, but no column.
                    cur.rowwise |= side_by_side;
                    cur.join(&b);
                }
            }
            None => out.push(b),
        }
    }
    // A last band too narrow to be a column joins the one before.
    if out.len() >= 2 && !wide(&out[out.len() - 1]) {
        if let Some(last) = out.pop() {
            if let Some(cur) = out.last_mut() {
                cur.rowwise |= cur.overlap(&last) >= COLUMN_MIN_OVERLAP_PT;
                cur.join(&last);
            }
        }
    }
    out
}

/// `page`'s units in reading order: its body lines by poppler block, and
/// `tables`.
pub(super) fn units(page: &Page, tables: Vec<Found>) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut last_block = None;
    for (i, l) in page.lines.iter().enumerate() {
        if l.role != Role::Body {
            // A block cut by a table (or a header) goes on as a unit of its
            // own, so each piece takes its own place in the order.
            last_block = None;
            continue;
        }
        match units.last_mut() {
            Some(Unit {
                rect,
                what: What::Lines(lines),
            }) if last_block == Some(l.block) => {
                lines.push(i);
                *rect = rect.union(&l.rect);
            }
            _ => units.push(Unit {
                rect: l.rect,
                what: What::Lines(vec![i]),
            }),
        }
        last_block = Some(l.block);
    }
    units.extend(tables.into_iter().map(|t| Unit {
        rect: t.rect,
        what: What::Table(t),
    }));
    order(page, units)
}

/// `units` of `page` in reading order.
pub(super) fn order(page: &Page, mut units: Vec<Unit>) -> Vec<Unit> {
    let (Some(left), Some(right)) = (
        units.iter().map(|u| u.rect.x0).reduce(f32::min),
        units.iter().map(|u| u.rect.x1).reduce(f32::max),
    ) else {
        return units;
    };
    let width = (right - left).max(0.0);
    units.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0));
    let narrow: Vec<bool> = units
        .iter()
        .map(|u| u.rect.width() <= width * COLUMN_MAX_SHARE)
        .collect();
    // Only narrow units count as beside each other: a wide one separates.
    let rects: Vec<Rect> = units
        .iter()
        .zip(&narrow)
        .filter(|(_, n)| **n)
        .map(|(u, _)| u.rect)
        .collect();
    let mut flags = beside::beside(&rects).into_iter();
    let beside: Vec<bool> = narrow
        .iter()
        .map(|n| *n && flags.next().unwrap_or(false))
        .collect();
    let bands = bands(&units, width, &narrow, &beside);
    // Each unit's band, or `None` when it separates sections.
    let band_of = |i: usize, r: &Rect| -> Option<usize> {
        if !narrow[i] {
            return None;
        }
        let b = bands
            .partition_point(|b| b.x1 < r.x0)
            .min(bands.len().checked_sub(1)?);
        let band = bands.get(b)?;
        (r.x0 >= band.x0 - COLUMN_GAP_MIN_PT && r.x1 <= band.x1 + COLUMN_GAP_MIN_PT).then_some(b)
    };
    let placed: Vec<Option<usize>> = units
        .iter()
        .enumerate()
        .map(|(i, u)| band_of(i, &u.rect))
        .collect();
    let mut out = Vec::with_capacity(units.len());
    let mut section: Vec<(usize, Unit)> = Vec::new();
    let flush = |section: &mut Vec<(usize, Unit)>, out: &mut Vec<Unit>| {
        // Stable: within a band, top to bottom as sorted.
        section.sort_by_key(|(b, _)| *b);
        let mut drained = section.drain(..).peekable();
        while let Some((b, u)) = drained.next() {
            if !bands[b].rowwise {
                out.push(u);
                continue;
            }
            let mut run = vec![u];
            while let Some((_, n)) = drained.next_if(|(nb, _)| *nb == b) {
                run.push(n);
            }
            rowwise(page, run, out);
        }
    };
    for (u, b) in units.into_iter().zip(placed) {
        match b {
            Some(b) => section.push((b, u)),
            None => {
                flush(&mut section, &mut out);
                out.push(u);
            }
        }
    }
    flush(&mut section, &mut out);
    out
}

/// One band's `units` (top to bottom) that is no column: those overlapping
/// vertically read row by row where their lines pair up, the rest top to
/// bottom, left to right.
fn rowwise(page: &Page, units: Vec<Unit>, out: &mut Vec<Unit>) {
    let mut cluster: Vec<Unit> = Vec::new();
    let mut bottom = f32::NEG_INFINITY;
    for u in units {
        if !cluster.is_empty() && u.rect.y0 >= bottom {
            emit(page, std::mem::take(&mut cluster), out);
        }
        bottom = if cluster.is_empty() {
            u.rect.y1
        } else {
            bottom.max(u.rect.y1)
        };
        cluster.push(u);
    }
    emit(page, cluster, out);
}

/// One cluster of vertically overlapping units: as rows when their lines
/// pair up (module doc), else top to bottom, left to right.
fn emit(page: &Page, mut cluster: Vec<Unit>, out: &mut Vec<Unit>) {
    let lines_only = cluster.iter().all(|u| matches!(u.what, What::Lines(_)));
    if cluster.len() >= 2 && lines_only {
        let mut idx: Vec<(usize, usize)> = Vec::new();
        for (k, u) in cluster.iter().enumerate() {
            if let What::Lines(l) = &u.what {
                idx.extend(l.iter().map(|&i| (i, k)));
            }
        }
        let rows = visual_rows(&page.lines, idx);
        let paired = rows
            .iter()
            .filter(|r| r.iter().any(|(_, k)| *k != r[0].1))
            .count();
        if paired >= ROWWISE_MIN_PAIRED && paired * 2 >= rows.len() {
            let rect = Rect::union_all(cluster.iter().map(|u| &u.rect)).unwrap_or_default();
            out.push(Unit {
                rect,
                what: What::Rows(
                    rows.into_iter()
                        .map(|r| r.into_iter().map(|(i, _)| i).collect())
                        .collect(),
                ),
            });
            return;
        }
    }
    cluster.sort_by(|a, b| {
        a.rect
            .y0
            .total_cmp(&b.rect.y0)
            .then(a.rect.x0.total_cmp(&b.rect.x0))
    });
    out.extend(cluster);
}

/// `idx` (a line and its unit) grouped into visual rows: lines whose
/// vertical centre lies inside the row's first line, each row left to right.
fn visual_rows(lines: &[Line], mut idx: Vec<(usize, usize)>) -> Vec<Vec<(usize, usize)>> {
    let rect = |i: usize| lines.get(i).map(|l| l.rect).unwrap_or_default();
    idx.sort_by(|a, b| rect(a.0).cy().total_cmp(&rect(b.0).cy()));
    let mut rows: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut anchor = Rect::default();
    for (i, k) in idx {
        let r = rect(i);
        match rows.last_mut() {
            Some(row) if r.cy() >= anchor.y0 && r.cy() <= anchor.y1 => row.push((i, k)),
            _ => {
                anchor = r;
                rows.push(vec![(i, k)]);
            }
        }
    }
    for row in &mut rows {
        row.sort_by(|a, b| rect(a.0).x0.total_cmp(&rect(b.0).x0));
    }
    rows
}

#[cfg(test)]
mod tests;
