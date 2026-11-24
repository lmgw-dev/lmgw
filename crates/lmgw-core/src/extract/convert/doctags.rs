//! The DocTags reader (document-conversion design §3 tier 1, WP6): a page
//! as a docling model writes it (granite-docling) read into the blocks of
//! [`super::doc`], with their boxes.
//!
//! **The vocabulary** is docling-core's ([`vocab`], checked against
//! docling-core 2.104.0 on 2026-11-19): one `<doctag>…</doctag>` holding
//! elements (`<text>`, `<title>`, `<section_header_level_N>`, `<caption>`,
//! `<formula>`, `<list_item>`, `<code>` with a language token, `<picture>`
//! or `<chart>` with a class token, key-value cells, …) and tables written
//! in OTSL: a cell token before each cell's text (`<fcel>` filled, `<ecel>`
//! empty, `<ched>` column header, `<rhed>` row header, `<srow>` section
//! row, `<lcel>` joined to the cell on its left, `<ucel>` to the one above,
//! `<xcel>` to both) and `<nl>` after each row. An element starts with its
//! box as four `<loc_N>` tokens, x0 y0 x1 y1 on a grid of [`GRID`] steps
//! across the page.
//!
//! **What sits in what.**
//! - A picture (`<picture>`, `<chart>`) holds its class, and may hold a
//!   `<caption>`, a molecule's `<smiles>`, and a chart's data as OTSL cells
//!   (bare, as docling writes them, or in an `<otsl>` of their own). It
//!   becomes a `figure` block, followed by its data as a table, its
//!   molecule as a paragraph and its caption.
//! - A table (`<otsl>`, `<table>`, `<document_index>`) holds cells and may
//!   hold a `<caption>`.
//! - An `<inline>` group inside a text element (or on its own) holds
//!   elements whose text is one run: they are joined into one block.
//!
//! **The answer is untrusted text.** Nothing here follows it anywhere: it
//! only ever becomes text blocks. The reader is one pass without recursion,
//! so nesting costs nothing, and everything it allocates is bounded by the
//! answer's own length (a table's cells are the cell tokens it has; a span
//! is a number, never a grid).
//!
//! **No word is lost; what is not well-formed is said** ([`Read::issues`]).
//! An element left open ends at the next element. Text outside any element
//! is a paragraph. Text where an element has none (directly inside a
//! picture, before a table's first cell, after a joined cell) is kept as a
//! paragraph or in the cell it follows. A token of the vocabulary where
//! DocTags has none (a cell token outside a table, a picture class outside
//! a picture, a location that is no number of the grid) is dropped, and
//! counted. A `<…>` that is no token of the vocabulary stays the text it
//! is. Never a panic.

use super::doc::{Block, BlockKind, Source};

mod table;
mod vocab;

use vocab::{CellTok, Group, Leaf, Tok};

/// The steps of the location grid: `<loc_0>` to `<loc_499>` across the page
/// (docling-core's `get_location_token`, `rnorm = 500`).
pub const GRID: f32 = 500.0;

/// The longest tag the reader looks for, in bytes: longer than any token of
/// the vocabulary (`</chemistry_molecular_structure>` is 32), so a `<`
/// without its `>` nearby is text and costs no scan to the end.
pub(crate) const MAX_TAG_BYTES: usize = 48;

/// A page read from DocTags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Read {
    pub blocks: Vec<Block>,
    /// The answer carried DocTags structure: an element or a group tag.
    /// Without any, a model that answers in DocTags lost its tags on the
    /// way: they are special tokens, which llama-server leaves out unless
    /// it was started with `--special`.
    pub tagged: bool,
    /// The answer carried location tokens. They are ordinary tokens, so an
    /// answer whose tags were left out still has them, before each run of
    /// text (seen live on granite-docling, 2026-11-19).
    pub located: bool,
    /// What was not well-formed, each kind counted once per occurrence.
    pub issues: Issues,
}

/// What a DocTags answer got wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Issues {
    /// No `<doctag>` root, or it was not closed.
    pub root: bool,
    /// Elements that were not closed by their own closing tag.
    pub unclosed: u32,
    /// Runs of text outside any element (kept as paragraphs).
    pub loose_text: u32,
    /// Elements whose box was started and is not four location tokens
    /// (kept without one).
    pub boxes: u32,
    /// Runs of text where an element has none: directly inside a picture,
    /// before a table's first cell, after a joined cell (kept).
    pub misplaced: u32,
    /// Tokens where DocTags has none, or that say nothing readable: a cell
    /// token outside a table, a class outside a picture, a language outside
    /// code, a location that is no number of the grid (dropped).
    pub stray: u32,
}

impl Issues {
    pub fn any(&self) -> bool {
        self.root
            || [
                self.unclosed,
                self.loose_text,
                self.boxes,
                self.misplaced,
                self.stray,
            ]
            .iter()
            .any(|n| *n > 0)
    }

    /// In words, for a warning.
    pub fn said(&self) -> String {
        let mut said = Vec::new();
        if self.root {
            said.push("the <doctag> element was missing or left open".to_string());
        }
        if self.unclosed > 0 {
            said.push(format!("{} element(s) were left open", self.unclosed));
        }
        if self.loose_text > 0 {
            said.push(format!(
                "{} run(s) of text stood outside any element",
                self.loose_text
            ));
        }
        if self.boxes > 0 {
            said.push(format!(
                "{} element(s) had no complete position",
                self.boxes
            ));
        }
        if self.misplaced > 0 {
            said.push(format!(
                "{} run(s) of text stood where an element has none (inside a picture, before a \
                 table's first cell)",
                self.misplaced
            ));
        }
        if self.stray > 0 {
            said.push(format!(
                "{} token(s) stood where DocTags has none and were left out",
                self.stray
            ));
        }
        said.join(", ")
    }
}

/// Whether `answer` is written in DocTags: it starts with the `<doctag>`
/// root. Deliberately strict: a markdown answer that only mentions a tag is
/// markdown.
pub fn looks_like(answer: &str) -> bool {
    answer.trim_start().starts_with("<doctag>")
}

fn paragraph(text: &str) -> Block {
    Block::new(
        BlockKind::Paragraph,
        text.trim().to_string(),
        None,
        Source::Vision,
    )
}

/// An element being read.
struct Open<'a> {
    leaf: Leaf,
    name: &'a str,
    locs: Vec<f32>,
    text: String,
    /// A table's rows of cells, in OTSL order; a picture's chart data.
    rows: Vec<Vec<(CellTok, String)>>,
    /// Text that stood where the element has none, before its content.
    before: Vec<Block>,
    /// What was found inside a picture or a table, in order: a caption, a
    /// molecule, a table of its own, text that stood there.
    inner: Vec<Block>,
    /// `<inline>` groups open inside it, and elements open inside those:
    /// their text is this element's.
    inline: u32,
    nested: u32,
    /// Made for an `<inline>` group that stands on its own.
    synthetic: bool,
}

impl<'a> Open<'a> {
    fn new(leaf: Leaf, name: &'a str) -> Self {
        Self {
            leaf,
            name,
            locs: Vec::new(),
            text: String::new(),
            rows: Vec::new(),
            before: Vec::new(),
            inner: Vec::new(),
            inline: 0,
            nested: 0,
            synthetic: false,
        }
    }

    /// The cell text goes to now, in a table or a picture's chart data.
    fn cell(&mut self) -> Option<&mut String> {
        self.rows
            .last_mut()
            .and_then(|r| r.last_mut())
            .map(|c| &mut c.1)
    }

    /// Text inside the element; `true` when it stood where the element has
    /// none (and was kept as a paragraph).
    fn push_text(&mut self, t: &str) -> bool {
        if !self.leaf.floats() {
            self.text.push_str(t);
            return false;
        }
        if let Some(cell) = self.cell() {
            cell.push_str(t);
            return false;
        }
        if t.trim().is_empty() {
            return false;
        }
        match (self.leaf, self.rows.is_empty()) {
            (Leaf::Table, true) => self.before.push(paragraph(t)),
            _ => self.inner.push(paragraph(t)),
        }
        true
    }

    /// A cell token, or a label that starts a cell.
    fn push_cell(&mut self, c: CellTok) {
        if self.rows.is_empty() {
            self.rows.push(Vec::new());
        }
        if let Some(row) = self.rows.last_mut() {
            row.push((c, String::new()));
        }
    }

    /// A cell label: the kind of the cell it follows, or a cell of its own
    /// when it follows none (or one that already has its text).
    fn push_label(&mut self, l: CellTok) {
        let last = self.rows.last_mut().and_then(|r| r.last_mut());
        match last {
            Some((tok, text)) if text.trim().is_empty() => {
                if !tok.joined() && l != CellTok::Filled {
                    *tok = l;
                }
            }
            _ => self.push_cell(l),
        }
    }
}

/// Close `open` into its blocks.
fn close(open: Open<'_>, size: Option<(f32, f32)>, read: &mut Read) {
    let boxed = table::bbox(&open.locs, size);
    // A box that was started and not finished; an element written without
    // one (a model that gives no positions) is no fault.
    if !open.locs.is_empty() && open.locs.len() != 4 {
        read.issues.boxes += 1;
    }
    let text = open.text.trim().to_string();
    let block = |kind, text: String| Block::new(kind, text, boxed, Source::Vision);
    read.blocks.extend(open.before);
    match open.leaf {
        Leaf::Picture => {
            read.blocks.push(block(BlockKind::Figure, String::new()));
            // A chart's data, written as cells inside the picture.
            let (t, misplaced) = table::table(open.rows);
            read.issues.misplaced += misplaced;
            if t.rows > 0 {
                read.blocks.push(Block::new(
                    BlockKind::Table(t),
                    String::new(),
                    None,
                    Source::Vision,
                ));
            }
        }
        Leaf::Table => {
            let (t, misplaced) = table::table(open.rows);
            read.issues.misplaced += misplaced;
            if t.rows > 0 {
                read.blocks.push(block(BlockKind::Table(t), String::new()));
            }
        }
        _ if text.is_empty() => {}
        Leaf::Text => read.blocks.push(block(BlockKind::Paragraph, text)),
        Leaf::Heading(n) => read.blocks.push(block(BlockKind::Heading(n), text)),
        Leaf::Caption => read.blocks.push(block(BlockKind::Caption, text)),
        Leaf::Formula => read.blocks.push(block(BlockKind::Formula, text)),
        Leaf::ListItem => read.blocks.push(block(BlockKind::ListItem, text)),
        Leaf::PageHeader => read.blocks.push(block(BlockKind::PageHeader, text)),
        Leaf::PageFooter => read.blocks.push(block(BlockKind::PageFooter, text)),
        Leaf::Code => read.blocks.push(block(BlockKind::Code, text)),
        Leaf::Checkbox(on) => {
            let mark = if on { "[x]" } else { "[ ]" };
            read.blocks
                .push(block(BlockKind::Paragraph, format!("{mark} {text}")));
        }
    }
    read.blocks.extend(open.inner);
}

/// What a token ends.
enum Ends {
    Nothing,
    /// The element being read, by its own closing tag.
    Element,
    /// Everything open: something was left open.
    Everything,
}

/// The reader's place in the answer: the element being read, and the
/// picture or table it sits in.
#[derive(Default)]
struct At<'a> {
    open: Option<Open<'a>>,
    outer: Option<Open<'a>>,
}

impl<'a> At<'a> {
    /// Close the element being read: into the picture or table it sits in,
    /// which is then the element being read again, or into the page.
    fn shut(&mut self, size: Option<(f32, f32)>, read: &mut Read) {
        let Some(o) = self.open.take() else {
            return;
        };
        match self.outer.take() {
            Some(mut out) => {
                let mut inner = Read::default();
                close(o, size, &mut inner);
                read.issues.boxes += inner.issues.boxes;
                read.issues.misplaced += inner.issues.misplaced;
                out.inner.append(&mut inner.blocks);
                self.open = Some(out);
            }
            None => close(o, size, read),
        }
    }

    /// Close everything that is open: an element was left open. Each open
    /// element counts once.
    fn shut_all(&mut self, size: Option<(f32, f32)>, read: &mut Read) {
        while self.open.is_some() {
            read.issues.unclosed += 1;
            self.shut(size, read);
        }
    }

    fn end(&mut self, ends: Ends, size: Option<(f32, f32)>, read: &mut Read) {
        match ends {
            Ends::Nothing => {}
            Ends::Element => self.shut(size, read),
            Ends::Everything => self.shut_all(size, read),
        }
    }

    /// Whether `l` (named `name`) sits inside the element being read: a
    /// caption in a picture or a table, a molecule or a table of its data
    /// in a picture.
    fn nests(&self, l: Leaf, name: &str) -> bool {
        let Some(inside) = self.open.as_ref().map(|o| o.leaf) else {
            return false;
        };
        self.outer.is_none()
            && inside.floats()
            && (l == Leaf::Caption
                || (inside == Leaf::Picture && (name == "smiles" || l == Leaf::Table)))
    }
}

/// Read `answer` as DocTags; `size` is the page's width and height in
/// points, which the boxes are scaled to (`None`: no boxes).
pub fn read(answer: &str, size: Option<(f32, f32)>) -> Read {
    let mut read = Read::default();
    let mut at = At::default();
    let (mut root_open, mut root_seen) = (false, false);
    let mut rest = answer;
    while !rest.is_empty() {
        let inside = at.open.as_ref().map(|o| o.leaf);
        let (tok, left) = vocab::next(rest, inside == Some(Leaf::Picture));
        rest = left;
        match tok {
            Tok::Text(t) => match at.open.as_mut() {
                Some(o) => read.issues.misplaced += u32::from(o.push_text(t)),
                None if t.trim().is_empty() => {}
                None => {
                    read.issues.loose_text += 1;
                    read.blocks.push(paragraph(t));
                }
            },
            Tok::Open(l, name) => {
                read.tagged = true;
                // Inside an inline group the elements are one run of text.
                if let Some(o) = at.open.as_mut().filter(|o| o.inline > 0 && !l.floats()) {
                    o.nested += 1;
                    if !o.text.is_empty() && !o.text.ends_with(char::is_whitespace) {
                        o.text.push(' ');
                    }
                    continue;
                }
                if at.nests(l, name) {
                    at.outer = at.open.take();
                } else {
                    // What was open was left open.
                    at.shut_all(size, &mut read);
                }
                at.open = Some(Open::new(l, name));
            }
            Tok::Close(name) => {
                let ends = match at.open.as_mut() {
                    // An element of an inline group: its text stays.
                    Some(o) if o.nested > 0 => {
                        o.nested -= 1;
                        Ends::Nothing
                    }
                    Some(o) if o.name == name && !o.synthetic => Ends::Element,
                    // A closing tag for something else: what is open ends
                    // here.
                    Some(_) => Ends::Everything,
                    None => {
                        read.issues.unclosed += 1;
                        Ends::Nothing
                    }
                };
                at.end(ends, size, &mut read);
            }
            Tok::Group {
                group: Group::Inline,
                close,
            } => {
                read.tagged = true;
                let ends = match (at.open.as_mut(), close) {
                    (Some(o), false) if !o.leaf.floats() => {
                        o.inline += 1;
                        Ends::Nothing
                    }
                    (Some(o), true) if o.inline > 0 => {
                        o.inline -= 1;
                        o.nested = 0;
                        if o.inline == 0 && o.synthetic {
                            Ends::Element
                        } else {
                            Ends::Nothing
                        }
                    }
                    // A picture or a table holds no inline group.
                    (Some(_), false) => Ends::Everything,
                    (None, false) => {
                        let mut o = Open::new(Leaf::Text, "inline");
                        o.synthetic = true;
                        o.inline = 1;
                        at.open = Some(o);
                        Ends::Nothing
                    }
                    (_, true) => Ends::Nothing,
                };
                at.end(ends, size, &mut read);
            }
            Tok::Group { group, close } => {
                read.tagged = true;
                at.shut_all(size, &mut read);
                if group == Group::Root {
                    root_seen |= !close;
                    root_open = !close;
                }
            }
            Tok::Loc(v) => {
                read.located = true;
                if let Some(o) = at.open.as_mut() {
                    let first = o.text.trim().is_empty() && o.rows.is_empty() && o.inline == 0;
                    if o.locs.len() < 4 && first {
                        o.locs.push(v);
                    }
                }
            }
            Tok::BadLoc => {
                read.located = true;
                read.issues.stray += 1;
            }
            Tok::Cell(c) => match at.open.as_mut().filter(|o| o.leaf.floats()) {
                Some(o) => o.push_cell(c),
                None => read.issues.stray += 1,
            },
            Tok::Label(l) => match at.open.as_mut().filter(|o| o.leaf.floats()) {
                Some(o) => o.push_label(l),
                None => read.issues.stray += 1,
            },
            Tok::Nl => match at.open.as_mut() {
                Some(o) if o.leaf == Leaf::Table => o.rows.push(Vec::new()),
                // A picture's chart data; a row end before any cell is none.
                Some(o) if o.leaf == Leaf::Picture => {
                    if !o.rows.is_empty() {
                        o.rows.push(Vec::new());
                    }
                }
                Some(o) => o.text.push('\n'),
                None => {}
            },
            Tok::Class => read.issues.stray += u32::from(inside != Some(Leaf::Picture)),
            // A code block's language stands before its text (and inside
            // an inline group, before a code element's).
            Tok::Language => {
                let placed = at.open.as_ref().is_some_and(|o| {
                    o.inline > 0 || (o.leaf == Leaf::Code && o.text.trim().is_empty())
                });
                read.issues.stray += u32::from(!placed);
            }
            Tok::Dropped => {}
        }
    }
    at.shut_all(size, &mut read);
    read.issues.root = !root_seen || root_open;
    read
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vocab_tests;
