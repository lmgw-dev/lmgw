//! A converted file's structure (document-conversion design §7.1): the one
//! intermediate every output is written from. A [`Doc`] is parts (pages,
//! slides, sheets, or one document), each with its blocks in reading order;
//! a block has its kind, its text, its box where the part has geometry,
//! and how it was read ([`Source`]).
//!
//! Who makes one:
//! - a PDF's layout pass ([`super::pdf_layout`]) from poppler's geometry,
//!   source `text_layer`;
//! - lmgw's own readers' markdown (office files, text) through
//!   [`from_md`], source `native`, without boxes.
//!
//! Later readers fill the same model: text recognition (`ocr`, words and
//! lines with boxes per page), a page read by a vision model (`vision`,
//! replacing the page's blocks), a layout model's regions and picture
//! export (`figure` blocks with a page and a box, in reading order). Each
//! part and each block says how it was read, so a page can mix them.
//!
//! What reads one:
//! - [`render`]: markdown and plain text;
//! - [`super::doc_json`]: the `json` output's wire form.
//!
//! The model is separate from the wire form, so the two can change apart.

pub mod from_md;
pub mod render;

use super::shape::Counts;

/// How a part or a block was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// lmgw's own readers.
    Native,
    /// A PDF page's own text.
    TextLayer,
    /// Text recognition (not built yet).
    Ocr,
    /// A vision model (not built yet).
    Vision,
    /// A layout or table model on the CPU (not built yet).
    Onnx,
    /// A speech-to-text model's transcript (WP7, `media.rs`).
    Asr,
}

/// What a part is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Page,
    Slide,
    Sheet,
    Document,
}

/// A box in points from the page's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    /// The vertical centre.
    pub fn cy(&self) -> f32 {
        (self.y0 + self.y1) / 2.0
    }

    /// The smallest box holding both.
    pub fn union(&self, o: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    /// The union of `rects`; `None` for none.
    pub fn union_all<'a>(rects: impl IntoIterator<Item = &'a Rect>) -> Option<Rect> {
        rects.into_iter().fold(None, |acc, r| {
            Some(match acc {
                None => *r,
                Some(a) => a.union(r),
            })
        })
    }
}

/// A file's structure.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Doc {
    pub parts: Vec<Part>,
}

/// One part: a page, a slide, a sheet, or the whole document.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub kind: PartKind,
    /// 1-based.
    pub number: u32,
    /// A sheet's name.
    pub name: Option<String>,
    /// A page's width and height in points.
    pub size: Option<(f32, f32)>,
    /// How it was read; `None`: not read (a page without a text layer).
    pub source: Option<Source>,
    pub blocks: Vec<Block>,
}

/// One block of a part.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    /// Its text: a paragraph's lines joined, a heading's words; empty for a
    /// table.
    pub text: String,
    pub bbox: Option<Rect>,
    pub source: Source,
    /// A [`BlockKind::Figure`]'s exported picture (`super::pictures`);
    /// `None` on every other block, and on a figure no picture was made for
    /// yet.
    pub figure: Option<Box<Figure>>,
}

impl Block {
    pub fn new(kind: BlockKind, text: String, bbox: Option<Rect>, source: Source) -> Self {
        Self {
            kind,
            text,
            bbox,
            source,
            figure: None,
        }
    }
}

/// What a figure block knows of its picture: the picture's index in the
/// file's list (`p1` is 0) and the caption found beside it. The picture
/// itself (its bytes, its page) is in the file's list, so a renderer or a
/// describer finds it by `index`.
#[derive(Debug, Clone, PartialEq)]
pub struct Figure {
    pub index: u32,
    pub caption: Option<String>,
    /// The picture-description model's words (WP11).
    pub description: Option<String>,
}

/// What a block is.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    /// Level 1 to 6.
    Heading(u8),
    Paragraph,
    ListItem,
    /// A numbered list item, marked so by a layout model's region: its
    /// marker as the page has it (`1.`, `2)`, up to nine digits), the text
    /// after it in the block's `text`.
    NumberedItem(String),
    Table(Table),
    Figure,
    Caption,
    Code,
    Formula,
    /// Repeated at the top of the pages: kept in json, left out of markdown
    /// and text.
    PageHeader,
    /// Repeated at the bottom of the pages, likewise.
    PageFooter,
}

/// A table's grid; `cells` holds the cells that have text, row by row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    pub rows: u32,
    pub cols: u32,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub row: u32,
    pub col: u32,
    pub row_span: u32,
    pub col_span: u32,
    pub header: bool,
    pub text: String,
}

impl Table {
    /// Each row in order, as its cells' `(column, text)` sorted by column,
    /// for the renderers to write slot by slot. Read from the sparse cells:
    /// nothing the size of the grid (rows × columns) is ever held, so only
    /// what is written costs memory, and that is charged to the request's
    /// output budget first ([`Doc::grid_cost`]). A spanning cell's text is in
    /// its first column; cells outside the grid are left out (a table this
    /// module builds never has one).
    pub fn for_each_row(&self, mut row: impl FnMut(u32, &[(u32, &str)])) {
        let mut cells: Vec<&Cell> = self
            .cells
            .iter()
            .filter(|c| c.row < self.rows && c.col < self.cols)
            .collect();
        cells.sort_by_key(|c| (c.row, c.col));
        let mut at = 0;
        let mut this: Vec<(u32, &str)> = Vec::new();
        for r in 0..self.rows {
            this.clear();
            while let Some(c) = cells.get(at).filter(|c| c.row == r) {
                // Two cells in one slot: the first is kept, as a grid would.
                if this.last().is_none_or(|(col, _)| *col != c.col) {
                    this.push((c.col, c.text.as_str()));
                }
                at += 1;
            }
            row(r, &this);
        }
    }

    /// One row's `cols` slots through `slot` (`""` where no cell is), with
    /// `gap` between them.
    pub fn write_row(
        &self,
        cells: &[(u32, &str)],
        gap: &str,
        out: &mut String,
        mut slot: impl FnMut(&str, &mut String),
    ) {
        let mut next = cells.iter().peekable();
        for c in 0..self.cols {
            if c > 0 {
                out.push_str(gap);
            }
            match next.peek() {
                Some((col, text)) if *col == c => {
                    slot(text, out);
                    next.next();
                }
                _ => slot("", out),
            }
        }
    }

    /// What writing the grid costs at least: three bytes a slot (a header
    /// separator row included) plus the cells' text, as the office reader
    /// charges a table (`office.rs`).
    pub fn grid_cost(&self) -> u64 {
        let slots = (u64::from(self.rows) + 1).saturating_mul(u64::from(self.cols));
        let text: u64 = self.cells.iter().map(|c| c.text.len() as u64).sum();
        slots.saturating_mul(3).saturating_add(text)
    }

    /// Each row a line of its cells separated by tabs (`tables: false`).
    pub fn as_lines(&self) -> String {
        let mut out = String::new();
        self.for_each_row(|r, cells| {
            if r > 0 {
                out.push('\n');
            }
            self.write_row(cells, "\t", &mut out, |t, o| o.push_str(t));
        });
        out
    }
}

impl Doc {
    /// What `stats` counts: headings per level, tables and their cells
    /// (each grid cell, header cells included, as the markdown's table rows
    /// have them).
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for b in self.parts.iter().flat_map(|p| &p.blocks) {
            match &b.kind {
                BlockKind::Heading(n) => {
                    let i = usize::from((*n).clamp(1, 6)) - 1;
                    c.headings[i] += 1;
                }
                BlockKind::Table(t) => {
                    c.tables += 1;
                    c.table_cells = c.table_cells.saturating_add(t.rows.saturating_mul(t.cols));
                }
                _ => {}
            }
        }
        c
    }

    /// What writing every table's grid costs at least
    /// ([`Table::grid_cost`]), charged to the request's output budget before
    /// any is written.
    pub fn grid_cost(&self) -> u64 {
        self.parts
            .iter()
            .flat_map(|p| &p.blocks)
            .map(|b| match &b.kind {
                BlockKind::Table(t) => t.grid_cost(),
                _ => 0,
            })
            .fold(0u64, u64::saturating_add)
    }

    /// `tables: false`: every table becomes a paragraph of its rows, each a
    /// line of cells separated by tabs.
    pub fn tables_as_lines(&mut self) {
        for b in self.parts.iter_mut().flat_map(|p| p.blocks.iter_mut()) {
            if let BlockKind::Table(t) = &b.kind {
                b.text = t.as_lines();
                b.kind = BlockKind::Paragraph;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        let cell = |row, col, text: &str| Cell {
            row,
            col,
            row_span: 1,
            col_span: 1,
            header: row == 0,
            text: text.into(),
        };
        Table {
            rows: 2,
            cols: 3,
            cells: vec![cell(0, 0, "Item"), cell(0, 2, "Cost"), cell(1, 0, "Pens")],
        }
    }

    #[test]
    fn a_grid_fills_the_cells_without_text() {
        let t = table();
        assert_eq!(t.as_lines(), "Item\t\tCost\nPens\t\t");
        assert_eq!(t.grid_cost(), 3 * 3 * 3 + 12);
    }

    /// A table claiming a vast grid with a few cells: writing a row costs
    /// its slots, never the grid; the cost says how large the whole is.
    #[test]
    fn rows_are_read_from_the_sparse_cells() {
        let t = Table {
            rows: 3,
            cols: 4_000_000_000,
            cells: vec![Cell {
                row: 1,
                col: 7,
                row_span: 1,
                col_span: 1,
                header: false,
                text: "x".into(),
            }],
        };
        let mut seen = Vec::new();
        t.for_each_row(|r, cells| seen.push((r, cells.to_vec().len())));
        assert_eq!(seen, [(0, 0), (1, 1), (2, 0)]);
        assert!(t.grid_cost() > 40_000_000_000);
    }

    #[test]
    fn counts_and_tables_as_lines() {
        let mut d = Doc {
            parts: vec![Part {
                kind: PartKind::Page,
                number: 1,
                name: None,
                size: Some((595.0, 842.0)),
                source: Some(Source::TextLayer),
                blocks: vec![
                    Block::new(
                        BlockKind::Heading(1),
                        "Title".into(),
                        None,
                        Source::TextLayer,
                    ),
                    Block::new(
                        BlockKind::Heading(2),
                        "Part".into(),
                        None,
                        Source::TextLayer,
                    ),
                    Block::new(
                        BlockKind::Table(table()),
                        String::new(),
                        None,
                        Source::TextLayer,
                    ),
                    Block::new(
                        BlockKind::PageHeader,
                        "Head".into(),
                        None,
                        Source::TextLayer,
                    ),
                ],
            }],
        };
        let c = d.counts();
        assert_eq!(c.headings, [1, 1, 0, 0, 0, 0]);
        assert_eq!((c.tables, c.table_cells), (1, 6));
        d.tables_as_lines();
        assert_eq!(d.parts[0].blocks[2].kind, BlockKind::Paragraph);
        assert_eq!(d.parts[0].blocks[2].text, "Item\t\tCost\nPens\t\t");
    }
}
