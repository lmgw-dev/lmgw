//! Text out of office files, as markdown (chat-complete design §8, §9.2).
//!
//! | format | how | output |
//! |---|---|---|
//! | docx | `word/document.xml` through `quick-xml` | paragraphs; headings (`Heading N`, `Title`) as `#`; list items as `- `; tables as markdown tables; the main document only (no headers, footers, comments or notes) |
//! | pptx | `ppt/slides/*.xml` in the presentation's own slide order | one `## Slide N` part per slide; tables as markdown tables; no speaker notes |
//! | odt / odp | `content.xml` | as docx / pptx; `text:h` headings, `text:list`, tables; comments, tracked deletions and speaker notes skipped |
//! | xlsx / ods | [`super::office_xlsx`], [`super::office_ods`] (own sparse readers; no spreadsheet library) | one `## Sheet: name` part per sheet, a markdown table of the rows that exist and the columns that are used, headed by the column letters, first column the row number |
//!
//! Legacy binary Office (`.doc` / `.xls` / `.ppt`, OLE2) is not read; the
//! sniffer refuses it.
//!
//! **Zip bombs.** Every ZIP-based format is checked before anything is read:
//! the archive's declared uncompressed sizes may add up to at most
//! [`MAX_UNCOMPRESSED_BYTES`] (a cheap early refusal), and then one shared
//! [`Budget`] of that size counts what is *actually* produced: every
//! decompressed byte read from any entry (a header can lie, the decompressor
//! cannot) plus every byte of extracted text, cell or table (a slide listed
//! twice, or a string cell referenced a million times, would otherwise
//! amplify). Over it is a visible [`OfficeError::TooLarge`] that names the
//! constant. The input itself is already bounded by the caller's
//! `max_body_mb`; this bounds what it may *expand* to.
//!
//! These functions are synchronous and CPU-bound; async callers run them in
//! `spawn_blocking` (as [`super::extract`] does).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Cursor, Read, Seek};

use quick_xml::events::Event;
use quick_xml::Reader;

/// The most an office archive may expand to, in bytes: the sum of its entries'
/// declared uncompressed sizes, and the [`Budget`] of decompressed bytes read
/// plus text produced, per archive.
///
/// 1 GiB. A real document's XML is megabytes; even a very large workbook is
/// a few hundred MiB unpacked. A ZIP bomb is gigabytes to petabytes. The line
/// sits far above anything real and far below anything that would take the
/// gateway down, and it is the one number to change if a real file ever hits
/// it — the refusal names it.
pub const MAX_UNCOMPRESSED_BYTES: u64 = 1 << 30;

/// The office formats [`extract`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    Docx,
    Xlsx,
    Pptx,
    Odt,
    Ods,
    Odp,
}

impl OfficeFormat {
    /// The spelling [`super::sniff::Sniffed::sub`] uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
        }
    }

    pub fn from_sub(sub: &str) -> Option<Self> {
        Some(match sub {
            "docx" => Self::Docx,
            "xlsx" => Self::Xlsx,
            "pptx" => Self::Pptx,
            "odt" => Self::Odt,
            "ods" => Self::Ods,
            "odp" => Self::Odp,
            _ => return None,
        })
    }

    /// Whether the file is a spreadsheet (one part per sheet).
    pub fn is_spreadsheet(self) -> bool {
        matches!(self, Self::Xlsx | Self::Ods)
    }
}

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfficeError {
    #[error("the file is not a readable {0} archive: {1}")]
    Archive(&'static str, String),
    #[error("the {0} file has no {1}")]
    Missing(&'static str, &'static str),
    #[error("{0} could not be read as XML: {1}")]
    Xml(String, String),
    #[error(
        "the archive expands to more than MAX_UNCOMPRESSED_BYTES ({} MiB) of unpacked bytes and \
         extracted text, so it was refused instead of unpacked ({what})",
        MAX_UNCOMPRESSED_BYTES >> 20
    )]
    TooLarge { what: String },
}

/// One unit of the output: a whole document, a slide, or a sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// `Slide 3` or `Sheet: Budget`; `None` for a word-processing document.
    pub heading: Option<String>,
    /// Markdown.
    pub body: String,
}

/// The text of one office file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficeText {
    pub format: OfficeFormat,
    pub parts: Vec<Part>,
}

impl OfficeText {
    /// Everything as one markdown text: each headed part under `## heading`.
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            if let Some(h) = &p.heading {
                let _ = write!(out, "## {h}");
                if !p.body.is_empty() {
                    out.push_str("\n\n");
                }
            }
            out.push_str(&p.body);
        }
        out
    }

    /// The names of the sheets (spreadsheets) — `Sheet: ` stripped.
    pub fn sheet_names(&self) -> Vec<String> {
        if !self.format.is_spreadsheet() {
            return Vec::new();
        }
        self.parts
            .iter()
            .filter_map(|p| p.heading.as_deref())
            .map(|h| h.strip_prefix("Sheet: ").unwrap_or(h).to_string())
            .collect()
    }
}

/// Read `bytes` as `format`, within a fresh [`Budget`] of
/// [`MAX_UNCOMPRESSED_BYTES`].
pub fn extract(format: OfficeFormat, bytes: &[u8]) -> Result<OfficeText, OfficeError> {
    extract_within(format, bytes, &mut Budget::new())
}

/// [`extract`] against a given budget (tests use a small one).
pub(crate) fn extract_within(
    format: OfficeFormat,
    bytes: &[u8],
    budget: &mut Budget,
) -> Result<OfficeText, OfficeError> {
    let parts = match format {
        OfficeFormat::Docx => vec![Part {
            heading: None,
            body: docx(bytes, budget)?,
        }],
        OfficeFormat::Pptx => pptx(bytes, budget)?,
        OfficeFormat::Odt => vec![Part {
            heading: None,
            body: odf(bytes, "odt", false, budget)?
                .into_iter()
                .next()
                .unwrap_or_default(),
        }],
        OfficeFormat::Odp => odf(bytes, "odp", true, budget)?
            .into_iter()
            .enumerate()
            .map(|(i, body)| Part {
                heading: Some(format!("Slide {}", i + 1)),
                body,
            })
            .collect(),
        OfficeFormat::Xlsx => {
            let mut zip = open_zip(bytes, "xlsx")?;
            check_archive(&mut zip)?;
            super::office_xlsx::sheets(&mut zip, budget)?
        }
        OfficeFormat::Ods => {
            let mut zip = open_zip(bytes, "ods")?;
            check_archive(&mut zip)?;
            super::office_ods::sheets(&mut zip, budget)?
        }
    };
    Ok(OfficeText { format, parts })
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

pub(super) type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// The one budget of actual bytes an archive may cost: decompressed bytes
/// read from any entry, and text produced from them. It is passed through
/// every read of one archive, so no path (a lying header, an entry named
/// twice, a repeated reference) can get around it.
#[derive(Debug)]
pub struct Budget {
    limit: u64,
    used: u64,
}

impl Budget {
    /// A budget of [`MAX_UNCOMPRESSED_BYTES`].
    pub fn new() -> Self {
        Self::with_limit(MAX_UNCOMPRESSED_BYTES)
    }

    pub(crate) fn with_limit(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    fn left(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// Account for `n` more bytes, or refuse by name.
    pub(super) fn spend(&mut self, n: u64, what: &str) -> Result<(), OfficeError> {
        if n > self.left() {
            self.used = self.limit;
            return Err(OfficeError::TooLarge {
                what: what.to_string(),
            });
        }
        self.used += n;
        Ok(())
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

fn open_zip<'a>(bytes: &'a [u8], what: &'static str) -> Result<Zip<'a>, OfficeError> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| OfficeError::Archive(what, e.to_string()))
}

/// Refuse an archive whose declared uncompressed sizes add up to more than
/// [`MAX_UNCOMPRESSED_BYTES`]. Only an early, cheap refusal: the [`Budget`]
/// is what bounds the real work, since a header can lie.
fn check_archive<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Result<(), OfficeError> {
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let size = zip
            .by_index_raw(i)
            .map_err(|e| OfficeError::Archive("zip", e.to_string()))?
            .size();
        total = total.saturating_add(size);
        if total > MAX_UNCOMPRESSED_BYTES {
            return Err(OfficeError::TooLarge {
                what: format!("{} entries declare more than that in total", zip.len()),
            });
        }
    }
    Ok(())
}

/// One entry, read to at most `limit` decompressed bytes, or `None` when the
/// archive has no such entry. More than `limit` is [`OfficeError::TooLarge`]
/// (the sniffer maps it to its own message).
pub(crate) fn read_entry_limited<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, OfficeError> {
    let file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(OfficeError::Archive("zip", e.to_string())),
    };
    let mut out = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| OfficeError::Archive("zip", format!("{name}: {e}")))?;
    if out.len() as u64 > limit {
        return Err(OfficeError::TooLarge {
            what: format!("the entry '{name}'"),
        });
    }
    Ok(Some(out))
}

/// One entry, or `None`: read against the archive's [`Budget`] — what is
/// really decompressed is what is counted.
pub(super) fn read_entry_if_present<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>, OfficeError> {
    let Some(out) = read_entry_limited(zip, name, budget.left())? else {
        return Ok(None);
    };
    budget.spend(out.len() as u64, &format!("the entry '{name}'"))?;
    Ok(Some(out))
}

fn read_entry(
    zip: &mut Zip<'_>,
    what: &'static str,
    name: &'static str,
    budget: &mut Budget,
) -> Result<Vec<u8>, OfficeError> {
    read_entry_if_present(zip, name, budget)?.ok_or(OfficeError::Missing(what, name))
}

// ---------------------------------------------------------------------------
// XML tokens
// ---------------------------------------------------------------------------

/// One element start: its local name and its attributes by qualified name.
pub(super) struct Elem {
    pub(super) name: String,
    attrs: Vec<(String, String)>,
}

impl Elem {
    /// An attribute by qualified name (`w:val`); failing that, by local name.
    pub(super) fn attr(&self, name: &str) -> Option<&str> {
        let local = |s: &str| s.rsplit(':').next().unwrap_or(s).to_string();
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .or_else(|| self.attrs.iter().find(|(k, _)| local(k) == local(name)))
            .map(|(_, v)| v.as_str())
    }
}

pub(super) enum Tok {
    Start(Elem),
    End(String),
    Text(String),
}

fn local(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

/// Walk `xml`, calling `f` for each start, end and run of text. Empty
/// elements arrive as a start and an end. Entities are resolved (the five
/// predefined ones and character references; any other is dropped).
pub(super) fn walk_xml(
    xml: &[u8],
    part: &str,
    mut f: impl FnMut(Tok) -> Result<(), OfficeError>,
) -> Result<(), OfficeError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let bad = |e: &dyn std::fmt::Display| OfficeError::Xml(part.to_string(), e.to_string());
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| bad(&e))? {
            Event::Start(e) => {
                let mut attrs = Vec::new();
                for a in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                    let val = a
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    attrs.push((key, val));
                }
                f(Tok::Start(Elem {
                    name: local(e.name().as_ref()),
                    attrs,
                }))?;
            }
            Event::End(e) => f(Tok::End(local(e.name().as_ref())))?,
            Event::Text(t) => f(Tok::Text(t.decode().map_err(|e| bad(&e))?.into_owned()))?,
            Event::CData(t) => f(Tok::Text(t.decode().map_err(|e| bad(&e))?.into_owned()))?,
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(|e| bad(&e))?;
                let text = match name.as_ref() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "apos" => Some('\''),
                    "quot" => Some('"'),
                    _ => r.resolve_char_ref().ok().flatten(),
                };
                if let Some(c) = text {
                    f(Tok::Text(c.to_string()))?;
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
        buf.clear();
    }
}

// ---------------------------------------------------------------------------
// The document builder shared by docx, pptx, odt and odp
// ---------------------------------------------------------------------------

enum Block {
    Para(String),
    Heading(u8, String),
    Item(usize, String),
    Table(String),
}

#[derive(Default)]
struct Tab {
    /// Finished rows: (column index, text) of the non-empty cells only.
    rows: Vec<Vec<(usize, String)>>,
    row: Vec<(usize, String)>,
    cell: Option<String>,
    /// Column of the open cell, and of the next one.
    cell_col: usize,
    next_col: usize,
}

/// Collects paragraphs and tables as the walkers meet them.
#[derive(Default)]
struct Doc {
    blocks: Vec<Block>,
    tables: Vec<Tab>,
    para: String,
    depth: usize,
    heading: Option<u8>,
    list: Option<usize>,
    /// Bytes of finished tables already spent against the budget when each
    /// closed, so the text total that is spent at the end leaves them out.
    tables_spent: u64,
}

impl Doc {
    /// A paragraph opens. Paragraphs nest (text boxes hold paragraphs): the
    /// outermost one owns the kind, and an inner one joins with a space.
    fn para_start(&mut self) {
        if self.depth == 0 {
            self.para.clear();
            self.heading = None;
            self.list = None;
        } else if !self.para.is_empty() && !self.para.ends_with([' ', '\n']) {
            self.para.push(' ');
        }
        self.depth += 1;
    }

    fn in_para(&self) -> bool {
        self.depth > 0
    }

    fn push(&mut self, s: &str) {
        if self.depth > 0 {
            self.para.push_str(s);
        }
    }

    /// Mark the open outermost paragraph as a heading of `level`.
    fn set_heading(&mut self, level: u8) {
        if self.depth == 1 {
            self.heading = Some(level.clamp(1, 6));
        }
    }

    /// Mark the open outermost paragraph as a list item at `level`.
    fn set_list(&mut self, level: usize) {
        if self.depth == 1 {
            self.list = Some(level);
        }
    }

    fn para_end(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth > 0 {
            return;
        }
        let text = std::mem::take(&mut self.para);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.cell.as_mut()) {
            if !cell.is_empty() {
                cell.push(' ');
            }
            cell.push_str(text);
            return;
        }
        self.blocks.push(match (self.heading, self.list) {
            (Some(l), _) => Block::Heading(l, text.to_string()),
            (None, Some(l)) => Block::Item(l, text.to_string()),
            _ => Block::Para(text.to_string()),
        });
    }

    fn table_start(&mut self) {
        self.tables.push(Tab::default());
    }

    fn row_start(&mut self) {
        if let Some(t) = self.tables.last_mut() {
            t.row.clear();
            t.next_col = 0;
        }
    }

    /// A cell opens, covering `repeat` columns (ODF's
    /// `number-columns-repeated`; 1 elsewhere).
    fn cell_start(&mut self, repeat: usize) {
        if let Some(t) = self.tables.last_mut() {
            t.cell = Some(String::new());
            t.cell_col = t.next_col;
            t.next_col = t.next_col.saturating_add(repeat.max(1));
        }
    }

    fn cell_end(&mut self) {
        if let Some(t) = self.tables.last_mut() {
            if let Some(text) = t.cell.take() {
                if !text.trim().is_empty() {
                    let col = t.cell_col;
                    t.row.push((col, text));
                }
            }
        }
    }

    fn row_end(&mut self) {
        if let Some(t) = self.tables.last_mut() {
            let row = std::mem::take(&mut t.row);
            if !row.is_empty() {
                t.rows.push(row);
            }
        }
    }

    /// A table closes. Nothing is allocated from its shape until the shape is
    /// spent against the `budget`: rows times used columns is what the
    /// markdown table costs, and a hostile file makes that quadratic in its
    /// own size (row `i` one cell at column `i`).
    fn table_end(&mut self, budget: &mut Budget) -> Result<(), OfficeError> {
        let Some(t) = self.tables.pop() else {
            return Ok(());
        };
        // Columns are ranked by the columns that hold anything: a repeated
        // empty cell in the source costs nothing here.
        let used: BTreeSet<usize> = t.rows.iter().flatten().map(|(c, _)| *c).collect();
        let rank: HashMap<usize, usize> = used.iter().enumerate().map(|(i, c)| (*c, i)).collect();
        if let Some(cell) = self.tables.last_mut().and_then(|p| p.cell.as_mut()) {
            // A table inside a cell: flattened into the cell's text (only the
            // cells that hold something — no grid).
            let flat = t
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|(_, c)| c.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !flat.is_empty() {
                if !cell.is_empty() {
                    cell.push(' ');
                }
                cell.push_str(&flat);
            }
        } else if !t.rows.is_empty() {
            let slots = (t.rows.len() as u64 + 1).saturating_mul(used.len() as u64);
            // The table's own size: three bytes a slot (` | `, or the two
            // spaces and bar of an empty one) plus its cells' text.
            let text: u64 = t.rows.iter().flatten().map(|(_, c)| c.len() as u64).sum();
            let cost = slots.saturating_mul(3).saturating_add(text);
            budget.spend(cost, "a table (rows that exist times the columns used)")?;
            self.tables_spent = self.tables_spent.saturating_add(cost);
            let md = sparse_table(&t.rows, &rank);
            if !md.is_empty() {
                self.blocks.push(Block::Table(md));
            }
        }
        Ok(())
    }

    /// The blocks as markdown, paragraphs `sep` apart (list items always one
    /// line apart from each other).
    fn finish(self, sep: &str) -> String {
        let mut out = String::new();
        let mut prev_item = false;
        for b in self.blocks {
            let (text, item) = match b {
                Block::Para(t) | Block::Table(t) => (t, false),
                Block::Heading(l, t) => (format!("{} {t}", "#".repeat(l as usize)), false),
                Block::Item(l, t) => (format!("{}- {t}", "  ".repeat(l.min(8))), true),
            };
            if !out.is_empty() {
                out.push_str(if item && prev_item { "\n" } else { sep });
            }
            out.push_str(&text);
            prev_item = item;
        }
        out
    }
}

/// One cell's text for a markdown table: no newlines, `|` escaped, trimmed.
pub(super) fn md_cell(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}

/// The markdown table of sparse rows (`(column, text)` pairs, each row
/// non-empty, the first the header), `rank` giving every used column its
/// place. The grid is only ever written out, never held.
fn sparse_table(rows: &[Vec<(usize, String)>], rank: &HashMap<usize, usize>) -> String {
    let width = rank.len();
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let mut line: Vec<(usize, String)> = r.iter().map(|(c, t)| (rank[c], md_cell(t))).collect();
        line.sort_by_key(|(c, _)| *c);
        out.push('|');
        let mut it = line.into_iter().peekable();
        for c in 0..width {
            match it.next_if(|(k, _)| *k == c) {
                Some((_, t)) => {
                    let _ = write!(out, " {t} |");
                }
                None => out.push_str("  |"),
            }
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..width {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// A markdown table from `rows` (the first is the header). Trailing empty
/// rows and columns are dropped; `None` when nothing is left. Cells lose their
/// newlines and escape `|`.
pub fn markdown_table(rows: &[Vec<String>]) -> String {
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| md_cell(c)).collect())
        .collect();
    let last_row = match rows.iter().rposition(|r| r.iter().any(|c| !c.is_empty())) {
        Some(i) => i,
        None => return String::new(),
    };
    let width = rows[..=last_row]
        .iter()
        .map(|r| r.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1))
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, r) in rows[..=last_row].iter().enumerate() {
        out.push('|');
        for c in 0..width {
            let _ = write!(out, " {} |", r.get(c).map_or("", String::as_str));
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..width {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

// ---------------------------------------------------------------------------
// docx
// ---------------------------------------------------------------------------

/// Heading level of a paragraph style, from `word/styles.xml`: the built-in
/// style names (`heading 1`, `Title`) are stored in English whatever the UI
/// language, so the map works for a German or French document too.
fn docx_heading_styles(styles: &[u8]) -> HashMap<String, u8> {
    let mut map = HashMap::new();
    let mut id: Option<String> = None;
    let _ = walk_xml(styles, "word/styles.xml", |t| {
        match t {
            Tok::Start(e) if e.name == "style" => id = e.attr("w:styleId").map(String::from),
            Tok::Start(e) if e.name == "name" => {
                if let (Some(id), Some(name)) = (&id, e.attr("w:val")) {
                    if let Some(l) = heading_level(name) {
                        map.insert(id.clone(), l);
                    }
                }
            }
            Tok::End(n) if n == "style" => id = None,
            _ => {}
        }
        Ok(())
    });
    map
}

/// `heading 2` / `Heading2` / `Title` → a level.
fn heading_level(name: &str) -> Option<u8> {
    let n = name.trim().to_lowercase();
    if n == "title" {
        return Some(1);
    }
    let rest = n.strip_prefix("heading")?.trim();
    rest.parse::<u8>().ok().filter(|l| (1..=9).contains(l))
}

fn docx(bytes: &[u8], budget: &mut Budget) -> Result<String, OfficeError> {
    let mut zip = open_zip(bytes, "docx")?;
    check_archive(&mut zip)?;
    let doc_xml = read_entry(&mut zip, "docx", "word/document.xml", budget)?;
    let styles = read_entry_if_present(&mut zip, "word/styles.xml", budget)?
        .map(|s| docx_heading_styles(&s))
        .unwrap_or_default();

    let mut doc = Doc::default();
    let (mut in_t, mut in_ppr, mut skip) = (false, false, 0usize);
    walk_xml(&doc_xml, "word/document.xml", |t| {
        if skip > 0 {
            match t {
                Tok::Start(_) => skip += 1,
                Tok::End(_) => skip -= 1,
                Tok::Text(_) => {}
            }
            return Ok(());
        }
        match t {
            // The VML fallback of a DrawingML choice repeats its text.
            Tok::Start(e) if e.name == "Fallback" => skip = 1,
            Tok::Start(e) => match e.name.as_str() {
                "p" => doc.para_start(),
                "pPr" => in_ppr = true,
                "pStyle" => {
                    if let Some(v) = e.attr("w:val") {
                        let level = styles.get(v).copied().or_else(|| heading_level(v));
                        if let Some(l) = level {
                            doc.set_heading(l);
                        }
                    }
                }
                "numPr" => doc.set_list(0),
                "ilvl" => {
                    if let Some(l) = e.attr("w:val").and_then(|v| v.parse().ok()) {
                        doc.set_list(l);
                    }
                }
                "t" => in_t = true,
                "tab" if !in_ppr => doc.push("\t"),
                "br" | "cr" if !in_ppr => doc.push("\n"),
                "noBreakHyphen" => doc.push("-"),
                "tbl" => doc.table_start(),
                "tr" => doc.row_start(),
                "tc" => doc.cell_start(1),
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "p" => doc.para_end(),
                "pPr" => in_ppr = false,
                "t" => in_t = false,
                "tbl" => doc.table_end(budget)?,
                "tr" => doc.row_end(),
                "tc" => doc.cell_end(),
                _ => {}
            },
            Tok::Text(s) if in_t => doc.push(&s),
            Tok::Text(_) => {}
        }
        Ok(())
    })?;
    let spent = doc.tables_spent;
    let out = doc.finish("\n\n");
    budget.spend(
        (out.len() as u64).saturating_sub(spent),
        "the extracted text of word/document.xml",
    )?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// pptx
// ---------------------------------------------------------------------------

fn pptx(bytes: &[u8], budget: &mut Budget) -> Result<Vec<Part>, OfficeError> {
    let mut zip = open_zip(bytes, "pptx")?;
    check_archive(&mut zip)?;
    let slides = pptx_slide_paths(&mut zip, budget)?;
    let mut parts = Vec::new();
    for (i, path) in slides.iter().enumerate() {
        let xml = read_entry_if_present(&mut zip, path, budget)?.ok_or(OfficeError::Missing(
            "pptx",
            "slide part named by the presentation",
        ))?;
        let (body, spent) = pptx_slide(&xml, path, budget)?;
        budget.spend(
            (body.len() as u64).saturating_sub(spent),
            &format!("the extracted text of '{path}'"),
        )?;
        parts.push(Part {
            heading: Some(format!("Slide {}", i + 1)),
            body,
        });
    }
    Ok(parts)
}

/// The slide parts in the presentation's own order (`p:sldIdLst` resolved
/// through the relationships — file numbers do not follow a reordering), or
/// by file number when the presentation part does not say.
fn pptx_slide_paths(zip: &mut Zip<'_>, budget: &mut Budget) -> Result<Vec<String>, OfficeError> {
    let mut ordered: Vec<String> = Vec::new();
    // A slide listed twice is read once: each slide part, in presentation order.
    let mut seen: HashSet<String> = HashSet::new();
    if let (Some(pres), Some(rels)) = (
        read_entry_if_present(zip, "ppt/presentation.xml", budget)?,
        read_entry_if_present(zip, "ppt/_rels/presentation.xml.rels", budget)?,
    ) {
        let mut targets: HashMap<String, String> = HashMap::new();
        walk_xml(&rels, "ppt/_rels/presentation.xml.rels", |t| {
            if let Tok::Start(e) = t {
                if e.name == "Relationship" {
                    if let (Some(id), Some(target)) = (e.attr("Id"), e.attr("Target")) {
                        targets.insert(id.to_string(), target.to_string());
                    }
                }
            }
            Ok(())
        })?;
        walk_xml(&pres, "ppt/presentation.xml", |t| {
            if let Tok::Start(e) = t {
                if e.name == "sldId" {
                    if let Some(target) = e.attr("r:id").and_then(|id| targets.get(id)) {
                        let path = match target.strip_prefix('/') {
                            Some(abs) => abs.to_string(),
                            None => format!("ppt/{target}"),
                        };
                        if seen.insert(path.clone()) {
                            ordered.push(path);
                        }
                    }
                }
            }
            Ok(())
        })?;
    }
    if !ordered.is_empty() {
        return Ok(ordered);
    }
    let number = |n: &str| -> Option<u64> {
        n.strip_prefix("ppt/slides/slide")?
            .strip_suffix(".xml")?
            .parse()
            .ok()
    };
    let mut numbered: Vec<(u64, String)> = zip
        .file_names()
        .filter_map(|n| number(n).map(|k| (k, n.to_string())))
        .collect();
    numbered.sort();
    Ok(numbered.into_iter().map(|(_, n)| n).collect())
}

fn pptx_slide(xml: &[u8], part: &str, budget: &mut Budget) -> Result<(String, u64), OfficeError> {
    let mut doc = Doc::default();
    let (mut in_t, mut skip) = (false, 0usize);
    walk_xml(xml, part, |t| {
        if skip > 0 {
            match t {
                Tok::Start(_) => skip += 1,
                Tok::End(_) => skip -= 1,
                Tok::Text(_) => {}
            }
            return Ok(());
        }
        match t {
            Tok::Start(e) if e.name == "Fallback" => skip = 1,
            Tok::Start(e) => match e.name.as_str() {
                "p" => doc.para_start(),
                "t" => in_t = true,
                "br" => doc.push("\n"),
                "tbl" => doc.table_start(),
                "tr" => doc.row_start(),
                "tc" => doc.cell_start(1),
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "p" => doc.para_end(),
                "t" => in_t = false,
                "tbl" => doc.table_end(budget)?,
                "tr" => doc.row_end(),
                "tc" => doc.cell_end(),
                _ => {}
            },
            Tok::Text(s) if in_t => doc.push(&s),
            Tok::Text(_) => {}
        }
        Ok(())
    })?;
    let spent = doc.tables_spent;
    Ok((doc.finish("\n"), spent))
}

// ---------------------------------------------------------------------------
// odt / odp
// ---------------------------------------------------------------------------

/// `content.xml` of an OpenDocument text or presentation: one body per slide
/// when `by_page` (each `draw:page`), else exactly one.
fn odf(
    bytes: &[u8],
    what: &'static str,
    by_page: bool,
    budget: &mut Budget,
) -> Result<Vec<String>, OfficeError> {
    let mut zip = open_zip(bytes, what)?;
    check_archive(&mut zip)?;
    let xml = read_entry(&mut zip, what, "content.xml", budget)?;

    let mut docs: Vec<Doc> = vec![Doc::default()];
    let (mut list_depth, mut skip, mut pages_seen) = (0usize, 0usize, 0usize);
    walk_xml(&xml, "content.xml", |t| {
        if skip > 0 {
            match t {
                Tok::Start(_) => skip += 1,
                Tok::End(_) => skip -= 1,
                Tok::Text(_) => {}
            }
            return Ok(());
        }
        let doc = docs.last_mut().expect("one doc at least");
        match t {
            // Comments, tracked deletions and speaker notes are not the text.
            Tok::Start(e)
                if matches!(e.name.as_str(), "annotation" | "tracked-changes" | "notes") =>
            {
                skip = 1
            }
            Tok::Start(e) => match e.name.as_str() {
                "page" if by_page => {
                    // The first page fills the doc that is already there.
                    pages_seen += 1;
                    if pages_seen > 1 {
                        docs.push(Doc::default());
                    }
                }
                "h" => {
                    doc.para_start();
                    let level = e.attr("text:outline-level").and_then(|v| v.parse().ok());
                    doc.set_heading(level.unwrap_or(1));
                }
                "p" => {
                    doc.para_start();
                    if list_depth > 0 {
                        doc.set_list(list_depth - 1);
                    }
                }
                "list" => list_depth += 1,
                "s" => doc.push(" "),
                "tab" => doc.push("\t"),
                "line-break" => doc.push("\n"),
                "table" => doc.table_start(),
                "table-row" => doc.row_start(),
                "table-cell" | "covered-table-cell" => {
                    let repeat = e
                        .attr("table:number-columns-repeated")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                    doc.cell_start(repeat);
                }
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "h" | "p" => doc.para_end(),
                "list" => list_depth = list_depth.saturating_sub(1),
                "table" => doc.table_end(budget)?,
                "table-row" => doc.row_end(),
                "table-cell" | "covered-table-cell" => doc.cell_end(),
                _ => {}
            },
            Tok::Text(s) if doc.in_para() => doc.push(&s),
            Tok::Text(_) => {}
        }
        Ok(())
    })?;
    let sep = if by_page { "\n" } else { "\n\n" };
    let spent: u64 = docs.iter().map(|d| d.tables_spent).sum();
    let bodies: Vec<String> = docs.into_iter().map(|d| d.finish(sep)).collect();
    let total: usize = bodies.iter().map(String::len).sum();
    budget.spend(
        (total as u64).saturating_sub(spent),
        "the extracted text of content.xml",
    )?;
    Ok(bodies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::test_files as tf;

    fn md(format: OfficeFormat, bytes: &[u8]) -> String {
        extract(format, bytes).unwrap().markdown()
    }

    #[test]
    fn markdown_tables_drop_empty_edges_and_escape() {
        let rows = vec![
            vec!["Name".into(), "Note".into(), String::new()],
            vec!["a|b".into(), "line1\nline2".into(), String::new()],
            vec![String::new(), String::new(), String::new()],
        ];
        assert_eq!(
            markdown_table(&rows),
            "| Name | Note |\n| --- | --- |\n| a\\|b | line1 line2 |"
        );
        assert_eq!(markdown_table(&[vec![String::new()]]), "");
        assert_eq!(markdown_table(&[]), "");
        // Ragged rows pad.
        let ragged = vec![vec!["h1".into(), "h2".into()], vec!["x".into()]];
        assert_eq!(
            markdown_table(&ragged),
            "| h1 | h2 |\n| --- | --- |\n| x |  |"
        );
    }

    #[test]
    fn docx_paragraphs_headings_lists_tabs_breaks_and_entities() {
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Intro &amp; scope</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Plain</w:t></w:r><w:r><w:tab/><w:t xml:space="preserve"> text</w:t></w:r><w:r><w:br/><w:t>next line</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t>tab stops are not tabs</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>one</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>nested</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:delText>deleted</w:delText></w:r></w:p>"#,
            r#"<w:p/>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Sub</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>caf&#233;</w:t></w:r></w:p>"#,
        );
        let out = md(OfficeFormat::Docx, &tf::docx_xml(body, None));
        assert_eq!(
            out,
            "# Intro & scope\n\nPlain\t text\nnext line\n\ntab stops are not tabs\n\n- one\n  - nested\n\n## Sub\n\ncafé"
        );
    }

    #[test]
    fn docx_heading_styles_come_from_styles_xml_in_any_language() {
        let styles = concat!(
            r#"<w:styles><w:style w:type="paragraph" w:styleId="berschrift1"><w:name w:val="heading 1"/></w:style>"#,
            r#"<w:style w:type="paragraph" w:styleId="Titel"><w:name w:val="Title"/></w:style>"#,
            r#"<w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Normal"/></w:style></w:styles>"#,
        );
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Titel"/></w:pPr><w:r><w:t>Bericht</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="berschrift1"/></w:pPr><w:r><w:t>Kapitel</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Body"/></w:pPr><w:r><w:t>Text</w:t></w:r></w:p>"#,
        );
        let out = md(OfficeFormat::Docx, &tf::docx_xml(body, Some(styles)));
        assert_eq!(out, "# Bericht\n\n# Kapitel\n\nText");
    }

    #[test]
    fn docx_tables_become_markdown_and_nested_ones_flatten() {
        let cell = |t: &str| format!(r#"<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>"#);
        let nested = format!(
            r#"<w:tc><w:p><w:r><w:t>outer</w:t></w:r></w:p><w:tbl><w:tr>{}{}</w:tr></w:tbl></w:tc>"#,
            cell("i1"),
            cell("i2")
        );
        let body = format!(
            "<w:p><w:r><w:t>before</w:t></w:r></w:p><w:tbl><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl><w:p><w:r><w:t>after</w:t></w:r></w:p>",
            cell("Item"),
            cell("Qty"),
            cell("bolt"),
            nested,
        );
        let out = md(OfficeFormat::Docx, &tf::docx_xml(&body, None));
        assert_eq!(
            out,
            "before\n\n| Item | Qty |\n| --- | --- |\n| bolt | outer i1 \\| i2 |\n\nafter"
        );
    }

    #[test]
    fn docx_does_not_repeat_the_vml_fallback_of_a_text_box() {
        let body = concat!(
            r#"<w:p><w:r><mc:AlternateContent>"#,
            r#"<mc:Choice Requires="wps"><w:drawing><wps:txbx><w:txbxContent><w:p><w:r><w:t>boxed</w:t></w:r></w:p></w:txbxContent></wps:txbx></w:drawing></mc:Choice>"#,
            r#"<mc:Fallback><w:pict><w:txbxContent><w:p><w:r><w:t>boxed</w:t></w:r></w:p></w:txbxContent></w:pict></mc:Fallback>"#,
            r#"</mc:AlternateContent></w:r></w:p>"#,
        );
        assert_eq!(md(OfficeFormat::Docx, &tf::docx_xml(body, None)), "boxed");
    }

    #[test]
    fn docx_missing_document_part_and_garbage_are_errors() {
        let empty = tf::zip(&[("[Content_Types].xml", b"<Types/>".as_slice())]);
        assert_eq!(
            extract(OfficeFormat::Docx, &empty).unwrap_err(),
            OfficeError::Missing("docx", "word/document.xml")
        );
        assert!(matches!(
            extract(OfficeFormat::Docx, b"not a zip"),
            Err(OfficeError::Archive("docx", _))
        ));
        let broken = tf::docx_xml("<w:p><w:r><w:t>x</w:r></w:p>", None);
        assert!(matches!(
            extract(OfficeFormat::Docx, &broken),
            Err(OfficeError::Xml(..))
        ));
    }

    #[test]
    fn pptx_slides_follow_the_presentation_order_not_the_file_numbers() {
        // Slide file 2 comes first in the presentation.
        let bytes = tf::pptx_ordered(
            &[("slide1.xml", &["second"]), ("slide2.xml", &["first"])],
            &["slide2.xml", "slide1.xml"],
        );
        let t = extract(OfficeFormat::Pptx, &bytes).unwrap();
        assert_eq!(t.parts.len(), 2);
        assert_eq!(t.parts[0].heading.as_deref(), Some("Slide 1"));
        assert_eq!(t.parts[0].body, "first");
        assert_eq!(t.parts[1].body, "second");
        assert_eq!(t.markdown(), "## Slide 1\n\nfirst\n\n## Slide 2\n\nsecond");
    }

    #[test]
    fn pptx_without_a_presentation_part_falls_back_to_numeric_order() {
        let bytes = tf::zip(&[
            ("ppt/slides/slide10.xml", tf::slide_xml(&["ten"]).as_bytes()),
            (
                "ppt/slides/slide2.xml",
                tf::slide_xml(&["two", "more"]).as_bytes(),
            ),
        ]);
        let t = extract(OfficeFormat::Pptx, &bytes).unwrap();
        assert_eq!(t.parts[0].body, "two\nmore");
        assert_eq!(t.parts[1].body, "ten");
    }

    #[test]
    fn pptx_tables_and_line_breaks() {
        let xml = concat!(
            r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree>"#,
            r#"<p:sp><p:txBody><a:p><a:r><a:t>Title</a:t></a:r><a:br/><a:r><a:t>sub</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl>"#,
            r#"<a:tr><a:tc><a:txBody><a:p><a:r><a:t>A</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>B</a:t></a:r></a:p></a:txBody></a:tc></a:tr>"#,
            r#"<a:tr><a:tc><a:txBody><a:p><a:r><a:t>1</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>2</a:t></a:r></a:p></a:txBody></a:tc></a:tr>"#,
            r#"</a:tbl></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#,
        );
        let bytes = tf::zip(&[("ppt/slides/slide1.xml", xml.as_bytes())]);
        assert_eq!(
            md(OfficeFormat::Pptx, &bytes),
            "## Slide 1\n\nTitle\nsub\n| A | B |\n| --- | --- |\n| 1 | 2 |"
        );
    }

    #[test]
    fn odt_headings_lists_tables_and_skipped_parts() {
        let body = concat!(
            r#"<office:text>"#,
            r#"<text:tracked-changes><text:changed-region><text:deletion><text:p>gone</text:p></text:deletion></text:changed-region></text:tracked-changes>"#,
            r#"<text:h text:outline-level="2">Chapter</text:h>"#,
            r#"<text:p>Some<text:s/>text<text:tab/>tab<text:line-break/>break <text:span>span</text:span><office:annotation><text:p>comment</text:p></office:annotation></text:p>"#,
            r#"<text:list><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:list><text:list-item><text:p>inner</text:p></text:list-item></text:list></text:list-item></text:list>"#,
            r#"<table:table><table:table-row><table:table-cell><text:p>K</text:p></table:table-cell><table:table-cell><text:p>V</text:p></table:table-cell></table:table-row>"#,
            r#"<table:table-row><table:table-cell table:number-columns-repeated="1000000000"/><table:table-cell><text:p>only-v</text:p></table:table-cell></table:table-row></table:table>"#,
            r#"</office:text>"#,
        );
        let out = md(OfficeFormat::Odt, &tf::odf("text", body));
        // The repeated empty cell keeps `only-v` out of V's column: the table is
        // as wide as the columns that hold anything, however many the file
        // claims to repeat.
        assert_eq!(
            out,
            "## Chapter\n\nSome text\ttab\nbreak span\n\n- one\n  - inner\n\n| K | V |  |\n| --- | --- | --- |\n|  |  | only-v |"
        );
    }

    #[test]
    fn odp_pages_become_slides_and_notes_are_skipped() {
        let body = concat!(
            r#"<office:presentation>"#,
            r#"<draw:page><draw:frame><draw:text-box><text:p>Welcome</text:p></draw:text-box></draw:frame>"#,
            r#"<presentation:notes><draw:frame><draw:text-box><text:p>speaker note</text:p></draw:text-box></draw:frame></presentation:notes></draw:page>"#,
            r#"<draw:page><draw:frame><draw:text-box><text:p>Second</text:p><text:p>slide</text:p></draw:text-box></draw:frame></draw:page>"#,
            r#"</office:presentation>"#,
        );
        let t = extract(OfficeFormat::Odp, &tf::odf("presentation", body)).unwrap();
        assert_eq!(
            t.markdown(),
            "## Slide 1\n\nWelcome\n\n## Slide 2\n\nSecond\nslide"
        );
    }

    #[test]
    fn spreadsheets_one_part_per_sheet_with_letters_and_row_numbers() {
        let bytes = tf::xlsx(&[
            (
                "Budget",
                &[
                    &["Item", "Cost"],
                    &["Rent", "800"],
                    &["Food", "n:12.5"],
                    &["", ""],
                ],
            ),
            ("Empty", &[]),
            ("Notes", &[&["only one cell"]]),
        ]);
        let t = extract(OfficeFormat::Xlsx, &bytes).unwrap();
        assert_eq!(t.sheet_names(), ["Budget", "Empty", "Notes"]);
        assert_eq!(
            t.parts[0].body,
            "| row | A | B |\n| --- | --- | --- |\n| 1 | Item | Cost |\n| 2 | Rent | 800 |\n| 3 | Food | 12.5 |"
        );
        assert_eq!(t.parts[1].body, "");
        assert_eq!(
            t.parts[2].body,
            "| row | A |\n| --- | --- |\n| 1 | only one cell |"
        );
        let all = t.markdown();
        assert!(
            all.starts_with("## Sheet: Budget\n\n| row | A | B |"),
            "{all}"
        );
        assert!(
            all.contains("\n\n## Sheet: Empty\n\n## Sheet: Notes\n\n| row"),
            "{all}"
        );
    }

    #[test]
    fn xlsx_reads_shared_strings_rich_text_booleans_errors_and_missing_refs() {
        let sst = concat!(
            r#"<sst xmlns="x" count="9" uniqueCount="2">"#,
            r#"<si><t>plain</t></si>"#,
            r#"<si><r><t>rich </t></r><r><rPr/><t>text</t></r><rPh><t>PHONETIC</t></rPh></si>"#,
            r#"</sst>"#,
        );
        let sheet = concat!(
            r#"<row r="2"><c r="B2" t="s"><v>0</v></c><c t="s"><v>1</v></c><c t="b"><v>1</v></c><c t="b"><v>0</v></c></row>"#,
            r#"<row><c t="e"><v>#DIV/0!</v></c><c t="str"><v>a &amp; b</v></c><c t="s"><v>99</v></c></row>"#,
        );
        let out = md(OfficeFormat::Xlsx, &tf::xlsx_raw(sheet, Some(sst)));
        assert_eq!(
            out,
            "## Sheet: Sheet1\n\n| row | A | B | C | D | E |\n| --- | --- | --- | --- | --- | --- |\n\
             | 2 |  | plain | rich text | TRUE | FALSE |\n| 3 | #DIV/0! | a & b |  |  |  |"
        );
    }

    #[test]
    fn a_lying_unique_count_and_a_corner_to_corner_sheet_cost_nothing() {
        // 1.5 KB files that made calamine reserve 99999999999 strings and a
        // 17-billion-cell rectangle. Both are read in-process, at once.
        let started = std::time::Instant::now();
        let sst = r#"<sst xmlns="x" count="1" uniqueCount="99999999999"><si><t>x</t></si></sst>"#;
        let out = md(
            OfficeFormat::Xlsx,
            &tf::xlsx_raw(
                r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#,
                Some(sst),
            ),
        );
        assert_eq!(
            out,
            "## Sheet: Sheet1\n\n| row | A |\n| --- | --- |\n| 1 | x |"
        );
        let wide = concat!(
            r#"<row r="1"><c r="A1"><v>1</v></c></row>"#,
            r#"<row r="1048576"><c r="XFD1048576"><v>2</v></c></row>"#,
        );
        let out = md(OfficeFormat::Xlsx, &tf::xlsx_raw(wide, None));
        assert_eq!(
            out,
            "## Sheet: Sheet1\n\n| row | A | XFD |\n| --- | --- | --- |\n| 1 | 1 |  |\n| 1048576 |  | 2 |"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_shared_string_referenced_many_times_is_counted_each_time() {
        // One 100 KB string referenced by 200 cells is 20 MB of output.
        let sst = format!(
            r#"<sst xmlns="x"><si><t>{}</t></si></sst>"#,
            "y".repeat(100_000)
        );
        let cells: String = (0..200)
            .map(|i| format!(r#"<c r="A{}" t="s"><v>0</v></c>"#, i + 1))
            .collect();
        let bytes = tf::xlsx_raw(&format!("<row>{cells}</row>"), Some(&sst));
        let e = extract_within(OfficeFormat::Xlsx, &bytes, &mut Budget::with_limit(5 << 20))
            .unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
    }

    #[test]
    fn a_lying_header_cannot_get_past_the_budget() {
        // The header says 100 bytes; the entry inflates to 4 MiB.
        let data = vec![b' '; 4 << 20];
        let bytes = tf::zip_lying_about_size("word/document.xml", &data, 100);
        let e = extract_within(OfficeFormat::Docx, &bytes, &mut Budget::with_limit(1 << 20))
            .unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
    }

    #[test]
    fn the_budget_is_shared_across_the_entries_of_one_archive() {
        // Two entries of 600 KiB each pass a 1 MiB budget one at a time.
        let slide = tf::slide_xml(&[&"z".repeat(600 << 10)]);
        let two = tf::zip(&[
            ("ppt/slides/slide1.xml", slide.as_bytes()),
            ("ppt/slides/slide2.xml", slide.as_bytes()),
        ]);
        assert!(extract_within(OfficeFormat::Pptx, &two, &mut Budget::with_limit(4 << 20)).is_ok());
        let e =
            extract_within(OfficeFormat::Pptx, &two, &mut Budget::with_limit(1 << 20)).unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
    }

    #[test]
    fn a_slide_listed_many_times_is_read_and_counted_once() {
        // 2 KB of `<p:sldId>`s all pointing at one 1 MB slide made 1.5 GB.
        let big = "s".repeat(1 << 20);
        let order = vec!["slide1.xml"; 300];
        let bytes = tf::pptx_ordered(&[("slide1.xml", &[big.as_str()])], &order);
        let t =
            extract_within(OfficeFormat::Pptx, &bytes, &mut Budget::with_limit(8 << 20)).unwrap();
        assert_eq!(t.parts.len(), 1);
        assert_eq!(t.parts[0].heading.as_deref(), Some("Slide 1"));
    }

    #[test]
    fn ods_is_read_sparsely() {
        let body = concat!(
            r#"<office:spreadsheet><table:table table:name="Sheet1">"#,
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p>Name</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Score</text:p></table:table-cell></table:table-row>"#,
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p>Ann</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="7"><text:p>7.00</text:p></table:table-cell><table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>TRUE</text:p></table:table-cell></table:table-row>"#,
            r#"</table:table></office:spreadsheet>"#,
        );
        let out = md(OfficeFormat::Ods, &tf::odf("spreadsheet", body));
        assert_eq!(
            out,
            "## Sheet: Sheet1\n\n| row | A | B | C |\n| --- | --- | --- | --- |\n\
             | 1 | Name | Score |  |\n| 2 | Ann | 7 | TRUE |"
        );
    }

    #[test]
    fn ods_repeated_empty_columns_and_rows_are_skipped_not_built() {
        let body = concat!(
            r#"<office:spreadsheet><table:table table:name="Big">"#,
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p>a</text:p></table:table-cell>"#,
            r#"<table:table-cell table:number-columns-repeated="1000000000"/>"#,
            r#"<table:table-cell office:value-type="string"><text:p>z</text:p></table:table-cell></table:table-row>"#,
            r#"<table:table-row table:number-rows-repeated="1048570"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>"#,
            r#"<table:table-row><table:table-cell><text:p>late</text:p></table:table-cell></table:table-row>"#,
            r#"</table:table></office:spreadsheet>"#,
        );
        let started = std::time::Instant::now();
        let out = md(OfficeFormat::Ods, &tf::odf("spreadsheet", body));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // `z` sits in column 1_000_000_001 (0-based), and the empty rows
        // between are not there at all.
        assert_eq!(
            out,
            "## Sheet: Big\n\n| row | A | CFDGSXN |\n| --- | --- | --- |\n| 1 | a | z |\n\
             | 1048572 | late |  |"
        );
    }

    #[test]
    fn ods_repeated_non_empty_cells_are_counted_against_the_budget() {
        let body = concat!(
            r#"<office:spreadsheet><table:table table:name="Bomb"><table:table-row>"#,
            r#"<table:table-cell table:number-columns-repeated="1000000000" office:value-type="string"><text:p>x</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table></office:spreadsheet>"#,
        );
        let e = extract(OfficeFormat::Ods, &tf::odf("spreadsheet", body)).unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
        // Repeated rows of a non-empty row count the same way.
        let rows = concat!(
            r#"<office:spreadsheet><table:table table:name="Bomb">"#,
            r#"<table:table-row table:number-rows-repeated="1000000000"><table:table-cell office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row>"#,
            r#"</table:table></office:spreadsheet>"#,
        );
        assert!(matches!(
            extract(OfficeFormat::Ods, &tf::odf("spreadsheet", rows)),
            Err(OfficeError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_broken_workbook_is_a_visible_error() {
        assert!(matches!(
            extract(OfficeFormat::Xlsx, b"nope"),
            Err(OfficeError::Archive("xlsx", _))
        ));
        assert!(matches!(
            extract(OfficeFormat::Ods, b"nope"),
            Err(OfficeError::Archive("ods", _))
        ));
        let empty = tf::zip(&[("[Content_Types].xml", b"<Types/>".as_slice())]);
        assert_eq!(
            extract(OfficeFormat::Xlsx, &empty).unwrap_err(),
            OfficeError::Missing("xlsx", "xl/workbook.xml")
        );
    }

    #[test]
    fn an_archive_that_expands_past_the_limit_is_refused_by_name() {
        // The check reads the declared sizes, so an archive whose header claims
        // more than the limit is refused before anything is inflated.
        let big = tf::zip_with_declared_size("word/document.xml", MAX_UNCOMPRESSED_BYTES + 1);
        let e = extract(OfficeFormat::Docx, &big).unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
        assert!(e.to_string().contains("1024 MiB"), "{e}");
        let big = tf::zip_with_declared_size("xl/workbook.xml", MAX_UNCOMPRESSED_BYTES + 1);
        assert!(matches!(
            extract(OfficeFormat::Xlsx, &big),
            Err(OfficeError::TooLarge { .. })
        ));
    }

    /// Row `i` holds one cell at column `i`: n rows x n used columns.
    fn diag_odt(n: usize) -> Vec<u8> {
        let rows: String = (0..n)
            .map(|i| {
                let lead = if i == 0 {
                    String::new()
                } else {
                    format!(r#"<table:table-cell table:number-columns-repeated="{i}"/>"#)
                };
                format!(
                    "<table:table-row>{lead}<table:table-cell><text:p>x</text:p></table:table-cell></table:table-row>"
                )
            })
            .collect();
        tf::odf("text", &format!("<table:table>{rows}</table:table>"))
    }

    fn diag_docx(n: usize) -> Vec<u8> {
        let rows: String = (0..n)
            .map(|i| {
                format!(
                    "<w:tr>{}<w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr>",
                    "<w:tc><w:p/></w:tc>".repeat(i)
                )
            })
            .collect();
        tf::docx_xml(&format!("<w:tbl>{rows}</w:tbl>"), None)
    }

    #[test]
    fn a_diagonal_table_is_spent_before_its_grid_is_built() {
        // 6000 x 6000 slots: 15 KB of odt used to allocate 1.25 GB.
        let e = extract_within(
            OfficeFormat::Odt,
            &diag_odt(6000),
            &mut Budget::with_limit(16 << 20),
        )
        .unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
        let e = extract_within(
            OfficeFormat::Docx,
            &diag_docx(3000),
            &mut Budget::with_limit(8 << 20),
        )
        .unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
    }

    #[test]
    fn a_small_diagonal_table_renders_sparsely_and_exactly() {
        let want = "| x |  |  |\n| --- | --- | --- |\n|  | x |  |\n|  |  | x |";
        let t = extract(OfficeFormat::Odt, &diag_odt(3)).unwrap().markdown();
        assert_eq!(t, want);
        let t = extract(OfficeFormat::Docx, &diag_docx(3))
            .unwrap()
            .markdown();
        assert_eq!(t, want);
    }

    #[test]
    fn xlsx_date_cells_are_iso_text_and_other_numbers_stay_numbers() {
        // xf 0 general, 1 built-in 14, 2 custom 164 datetime, 3 custom 165
        // elapsed hours, 4 custom 166 plain number (its code says "d" only
        // inside quotes). cellStyleXfs must not be counted.
        let styles = concat!(
            r#"<numFmts count="3"><numFmt numFmtId="164" formatCode="yyyy\-mm\-dd\ hh:mm:ss"/>"#,
            r#"<numFmt numFmtId="165" formatCode="[h]:mm"/>"#,
            r#"<numFmt numFmtId="166" formatCode="0.00&quot; days&quot;"/></numFmts>"#,
            r#"<cellStyleXfs count="1"><xf numFmtId="14"/></cellStyleXfs>"#,
            r#"<cellXfs count="5"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/>"#,
            r#"<xf numFmtId="165"/><xf numFmtId="166"/></cellXfs>"#,
        );
        let row = concat!(
            r#"<row r="1"><c r="A1"><v>46295</v></c><c r="B1" s="1"><v>46295</v></c>"#,
            r#"<c r="C1" s="2"><v>46295.75</v></c><c r="D1" s="3"><v>1.5</v></c>"#,
            r#"<c r="E1" s="4"><v>46295</v></c><c r="F1" s="9"><v>46295</v></c>"#,
            r#"<c r="G1" s="1" t="str"><v>46295</v></c></row>"#,
        );
        let t = extract(OfficeFormat::Xlsx, &tf::xlsx_styled(row, styles, false))
            .unwrap()
            .markdown();
        assert!(
            t.ends_with(
                "| 1 | 46295 | 2026-09-30 | 2026-09-30 18:00:00 | 36:00 | 46295 | 46295 | 46295 |"
            ),
            "{t}"
        );
        // The 1904 system: the same serial is 1462 days later.
        let row = r#"<row r="1"><c r="A1" s="1"><v>0</v></c></row>"#;
        let t = extract(OfficeFormat::Xlsx, &tf::xlsx_styled(row, styles, true))
            .unwrap()
            .markdown();
        assert!(t.ends_with("| 1 | 1904-01-01 |"), "{t}");
    }

    #[test]
    fn empty_shared_strings_are_spent_like_cells() {
        // 2 MiB of `<si/>` used to be 24-byte Strings without a refusal.
        let sst = format!("<sst>{}</sst>", "<si/>".repeat(400_000));
        let bytes = tf::xlsx_raw("", Some(&sst));
        let e = extract_within(OfficeFormat::Xlsx, &bytes, &mut Budget::with_limit(8 << 20))
            .unwrap_err();
        assert!(matches!(e, OfficeError::TooLarge { .. }), "{e:?}");
        assert!(e.to_string().contains("MAX_UNCOMPRESSED_BYTES"), "{e}");
    }

    #[test]
    fn heading_level_reads_the_built_in_names() {
        assert_eq!(heading_level("heading 3"), Some(3));
        assert_eq!(heading_level("Heading2"), Some(2));
        assert_eq!(heading_level("Title"), Some(1));
        assert_eq!(heading_level("Heading Strong"), None);
        assert_eq!(heading_level("Normal"), None);
    }
}
