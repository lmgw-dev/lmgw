//! What a vision model is asked for a PDF page that text extraction could not
//! read, or read badly — ported from folder-chat's `vision.rs` so knowledge-base
//! ingestion (chat-complete design §9.2) reads pages with the same measured
//! words and the same page rule.
//!
//! A page **without text** is read from its image alone ([`Mode::Ocr`],
//! [`OCR_PROMPT`]); a page that **looks like a table** ([`table_like`]), or any
//! page when the caller asks for every page, is read from its image *and* its
//! extracted text ([`Mode::Structure`], [`STRUCTURE_PROMPT`]); [`pick`] decides.
//! Pages are rendered at [`super::pdf::PAGE_DPI`].
//!
//! The prompts are versioned ([`VISION_PROMPT_VERSION`]): a cache of readings
//! keys on it, so changing either prompt means bumping it.

use super::pdf::PdfText;

/// A page is table-like when at least this many of its lines split into
/// [`TABLE_MIN_COLUMNS`] cells or more.
///
/// 3: a header row and two rows of values. Measured on a mixed sample folder on
/// 2026-09-24, together with the two constants below: it picks the payroll-style
/// table, the timetable and the invoice pages and 2 of 13 brochure pages, and
/// none of the 37 pages of a two-column report.
pub const TABLE_MIN_ROWS: usize = 3;

/// The fewest cells a line must split into to count as a table row.
///
/// 4: a two-column page of prose with a sidebar comes out of `pdftotext
/// -layout` as three columns of words; a table has more.
pub const TABLE_MIN_COLUMNS: usize = 4;

/// Cells are separated by runs of at least this many spaces, counted after
/// the line is trimmed.
///
/// 3: `-layout` puts one or two spaces between the words of one cell, and
/// wider runs between columns.
pub const TABLE_GAP_SPACES: usize = 3;

/// The version of the two prompts below. Part of every cached reading's key:
/// changing either prompt means bumping it, and every page is read again
/// with the new words.
///
/// "2": the table sentence names what a row heading and a column heading
/// are, with a worked example. Under "1", gemma4-12b read a payroll-style table
/// on 2026-09-24 as `<row label> | <column label>: <row label>` followed by
/// `<column label> | <n>: <n>` — the row label taken for a column, the values
/// no longer naming their row; with the example it gave `<row label> |
/// <column label>: …` for every cell, and still all 88 cells of the
/// timetable right.
pub const VISION_PROMPT_VERSION: &str = "2";

/// The prompt for a page without text: the image alone. It read a scan
/// near-perfectly and a table 88 cells of 88 in the 2026-09-24 spike — the
/// one-line-per-cell form is what keeps a value with its column, and the
/// worked example ([`VISION_PROMPT_VERSION`]) what keeps it with its row.
pub const OCR_PROMPT: &str = "Transcribe this document page as plain text. Reproduce every \
piece of text exactly as printed, in reading order, without translating, summarising or adding \
anything. For every table, write one line per non-empty cell in the form '<row heading> | \
<column heading>: <value>'. The row heading is the label that starts the value's row, the \
column heading is the header printed above the value's column. For example, a row \
'Rent   800   750' under the column headers 'March' and 'April' becomes the two lines \
'Rent | March: 800' and 'Rent | April: 750'. Use the headings exactly as printed and skip empty \
cells. Output only the transcription.";

/// The prompt for a page with text, before the page's text is appended
/// ([`prompt`]). The image alone misread one digit of a payroll table in the spike;
/// with the text given and the model told to copy from it, every number came
/// out exact and labelled by its column — and, with the worked example, by
/// its row ([`VISION_PROMPT_VERSION`]).
pub const STRUCTURE_PROMPT: &str = "Below is the exact text of this page, extracted from the \
PDF, with its layout approximated by spaces. Rewrite it as plain text in reading order. Copy \
every word and number character for character from the extracted text — never from the image, \
never corrected, translated or summarised; use the image only to see which row and column each \
value belongs to. For every table, write one line per non-empty cell in the form '<row heading> \
| <column heading>: <value>'. The row heading is the label that starts the value's row, the \
column heading is the header printed above the value's column. For example, a row \
'Rent   800   750' under the column headers 'March' and 'April' becomes the two lines \
'Rent | March: 800' and 'Rent | April: 750'. Use the headings exactly as they appear and skip \
empty cells. Output only the rewritten text.\n\nExtracted text:\n";

/// How a page is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// No text on the page: the image alone ([`OCR_PROMPT`]).
    Ocr,
    /// The image and the page's text ([`STRUCTURE_PROMPT`]).
    Structure,
}

impl Mode {
    /// As a cache key or a log line spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ocr => "ocr",
            Self::Structure => "structure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ocr" => Some(Self::Ocr),
            "structure" => Some(Self::Structure),
            _ => None,
        }
    }
}

/// How many cells a line splits into at runs of [`TABLE_GAP_SPACES`] spaces
/// or more, after trimming it.
fn cells(line: &str) -> usize {
    let line = line.trim();
    if line.is_empty() {
        return 0;
    }
    let mut n = 1;
    let mut run = 0usize;
    for c in line.chars() {
        if c == ' ' {
            run += 1;
        } else {
            if run >= TABLE_GAP_SPACES {
                n += 1;
            }
            run = 0;
        }
    }
    n
}

/// Whether a page's text looks like a table: at least [`TABLE_MIN_ROWS`] of
/// its lines split into at least [`TABLE_MIN_COLUMNS`] cells at runs of
/// [`TABLE_GAP_SPACES`] spaces or more.
pub fn table_like(page: &str) -> bool {
    page.lines()
        .filter(|l| cells(l) >= TABLE_MIN_COLUMNS)
        .take(TABLE_MIN_ROWS)
        .count()
        >= TABLE_MIN_ROWS
}

/// How one page is read, or `None` when it is not.
pub fn pick(page: &str, every_page: bool) -> Option<Mode> {
    if page.trim().is_empty() {
        Some(Mode::Ocr)
    } else if every_page || table_like(page) {
        Some(Mode::Structure)
    } else {
        None
    }
}

/// Every page of `text` a vision model reads, in page order (1-based).
pub fn select(text: &PdfText, every_page: bool) -> Vec<(u32, Mode)> {
    text.pages
        .iter()
        .enumerate()
        .filter_map(|(i, p)| pick(p, every_page).map(|m| (i as u32 + 1, m)))
        .collect()
}

/// The prompt for one page: [`OCR_PROMPT`], or [`STRUCTURE_PROMPT`] followed
/// by the page's text (its surrounding blank lines and trailing spaces
/// dropped; the spaces that lay out its columns kept).
pub fn prompt(mode: Mode, page_text: &str) -> String {
    match mode {
        Mode::Ocr => OCR_PROMPT.to_string(),
        Mode::Structure => {
            let text = page_text.trim_start_matches(['\n', '\r']).trim_end();
            format!("{STRUCTURE_PROMPT}{text}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str =
        "Item      Mar    Apr    May\nRent      800    750    700\nFood      200    210    190\n";

    #[test]
    fn a_page_with_three_four_cell_lines_is_a_table() {
        assert!(table_like(TABLE));
        // Two rows are not enough, and neither are three-cell rows.
        assert!(!table_like("a   b   c   d\ne   f   g   h\n"));
        assert!(!table_like("a   b   c\nd   e   f\ng   h   i\n"));
        // Prose with the odd double space is not columns.
        assert!(!table_like(
            "just some prose  with a  double space\nand another line\n"
        ));
    }

    #[test]
    fn cells_split_at_runs_of_the_gap_after_trimming() {
        assert_eq!(cells("  a   b    c  "), 3);
        assert_eq!(cells("one two  three"), 1);
        assert_eq!(cells("   "), 0);
    }

    #[test]
    fn pick_reads_blank_pages_from_the_image_and_tables_with_their_text() {
        assert_eq!(pick("  \n", false), Some(Mode::Ocr));
        assert_eq!(pick(TABLE, false), Some(Mode::Structure));
        assert_eq!(pick("plain prose", false), None);
        assert_eq!(pick("plain prose", true), Some(Mode::Structure));
    }

    #[test]
    fn select_numbers_pages_from_one() {
        let t = PdfText::from_raw(&format!("prose\u{c}\u{c}{TABLE}\u{c}"));
        assert_eq!(select(&t, false), [(2, Mode::Ocr), (3, Mode::Structure)]);
        assert_eq!(select(&t, true).len(), 3);
    }

    #[test]
    fn the_prompt_appends_the_page_text_only_for_structure() {
        assert_eq!(prompt(Mode::Ocr, "ignored"), OCR_PROMPT);
        let p = prompt(Mode::Structure, "\n\n  A   B  \n");
        assert!(p.starts_with(STRUCTURE_PROMPT), "{p}");
        assert!(p.ends_with("Extracted text:\n  A   B"), "{p}");
        assert_eq!(Mode::parse(Mode::Ocr.as_str()), Some(Mode::Ocr));
        assert_eq!(Mode::parse("x"), None);
    }

    #[test]
    fn the_ported_constants_keep_their_measured_values() {
        assert_eq!(
            (TABLE_MIN_ROWS, TABLE_MIN_COLUMNS, TABLE_GAP_SPACES),
            (3, 4, 3)
        );
        assert_eq!(VISION_PROMPT_VERSION, "2");
        assert!(OCR_PROMPT.starts_with("Transcribe this document page as plain text."));
        assert!(STRUCTURE_PROMPT.ends_with("Extracted text:\n"));
    }
}
