//! Reading PDF pages with a vision model — which pages, how they are asked
//! for, and how a reading joins the indexed text.
//!
//! Two gaps of `pdftotext` are what this is for. A **scanned page** has no
//! text layer, so nothing of it is indexed. A **table** comes out of
//! `-layout` as columns of spaces, and a small chat model reading that text
//! loses which column a value belongs to. A vision model shown the page image
//! reads both.
//!
//! **A reading never replaces the text layer.** The chunks of a PDF are
//! verbatim slices of `pdftotext`'s output, and a reading is model output, so
//! it is kept as text of its own: appended to the PDF's indexed text under a
//! header that names the page and the model ([`append_readings`]), cut into
//! chunks of its own headed `page N, read by <alias>` ([`heading`]), and shown
//! to the chat model and in citations as that model's reading. The chat's
//! system prompt says what such an excerpt is.
//!
//! **Which pages** ([`select`], a pure function over `pdftotext`'s output,
//! pages numbered as [`crate::chunk::pdf_pages`] numbers them):
//!
//! - a page with no text is read from its image alone ([`Mode::Ocr`]);
//! - a **table-like** page ([`table_like`]) is read from its image **and** its
//!   text ([`Mode::Structure`]): the model is told to copy every character
//!   from the text and use the image only to see which row and column a
//!   value is in, so no digit is taken from a misread image;
//! - with `vision_every_page` on, every other page with text is read in
//!   structure mode as well;
//! - any other page is not read.
//!
//! The rule's numbers are named constants, reported with every sync as one
//! sentence ([`rule`]), and part of the settings string ([`settings`]) whose
//! change sends every PDF through the sync's refresh pass.
//!
//! **The cache.** Each reading is stored in the index's page-reading cache
//! ([`crate::index`]) keyed by the bytes it was read from (the PDF's content
//! hash), the page, the alias, the mode, the prompt version
//! ([`VISION_PROMPT_VERSION`]) and the resolution ([`VISION_DPI`]), and
//! reused whenever all of them match — so a re-sync reads nothing again, and
//! neither does an index rebuilt for another embedding model. A failure that
//! would fail the same way again (an empty or cut-off answer, a refusal of
//! this page, a page that does not render) is cached too, and reported by
//! every sync until its key changes, or until the owner presses Retry failed
//! pages (`POST /api/vision/retry`) after fixing what failed. The key is the
//! alias, not the model behind it: pointing the alias at another model does
//! not re-read on its own; changing the alias field does.

use serde::{Deserialize, Serialize};

use crate::chunk::FORM_FEED;
use crate::gateway::{Gateway, PageReading};

/// The resolution a page is rendered at for the vision model (`pdftoppm -r`).
///
/// 150 dpi. On 2026-09-24 an A4 page at 150 dpi came to about 1000 image
/// tokens for Gemma 4 and read a scan near-perfectly and a table cell for
/// cell; more pixels cost tokens (and a llama-server `ubatch_size` at least
/// the image's tokens) without reading better. Reported with every sync
/// (`vision_dpi`).
pub const VISION_DPI: u32 = 150;

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

/// The side, in pixels, of the probe image when the refused page image's own
/// size cannot be read from its header ([`png_size`]) — which a PNG from
/// `pdftoppm` always can. The probe is otherwise the page's size: image
/// tokens grow with pixels (see [`VISION_DPI`]), so a smaller probe would
/// pass where the page's size is what the model refuses — a context too
/// small for one page image.
pub const PROBE_SIZE: u32 = 64;

/// What the probe image is sent with: a question any vision model answers,
/// about an image with nothing on it.
pub const PROBE_PROMPT: &str = "What colour is this image? Answer in one word.";

/// The settings ([`settings`]) of a PDF indexed without a vision model — and
/// what a PDF from before vision counts as, so it is not refreshed for
/// nothing.
pub const SETTINGS_OFF: &str = "off";

/// How a page is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No text on the page: the image alone ([`OCR_PROMPT`]).
    Ocr,
    /// The image and the page's text ([`STRUCTURE_PROMPT`]).
    Structure,
}

impl Mode {
    /// As stored in `folder_chat_vision.mode`.
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

/// The vision model a sync reads pages with.
#[derive(Debug, Clone)]
pub struct VisionOptions {
    /// The owner's `vision_model` alias.
    pub model: String,
    /// The owner's `vision_every_page`.
    pub every_page: bool,
    /// Where the readings are asked for.
    pub gateway: Gateway,
}

/// One stored reading of a page, ready to be appended ([`append_readings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub page: u32,
    /// The alias that read it.
    pub model: String,
    /// The reading as stored: one wrapping fence removed ([`clean`]).
    pub text: String,
}

/// Where one appended reading sits in the indexed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingSpan {
    pub page: u32,
    /// [`heading`] — the heading its chunks carry.
    pub heading: String,
    /// Byte offsets of the reading itself, header excluded.
    pub start: usize,
    pub end: usize,
}

/// `pdftotext`'s output as pages, numbered from 1 as
/// [`crate::chunk::pdf_pages`] numbers them: a trailing form feed closes the
/// last page rather than opening another.
pub fn pages(text: &str) -> Vec<&str> {
    text.strip_suffix(FORM_FEED)
        .unwrap_or(text)
        .split(FORM_FEED)
        .collect()
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

/// Every page of `pdftotext`'s output a vision model reads, in page order.
pub fn select(text: &str, every_page: bool) -> Vec<(u32, Mode)> {
    pages(text)
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

/// The page rule in one sentence, with its constants — the report's
/// `vision_rule`.
pub fn rule() -> String {
    format!(
        "a page without text is read from its image alone (ocr); a page on which at least \
         TABLE_MIN_ROWS ({TABLE_MIN_ROWS}) lines split into at least TABLE_MIN_COLUMNS \
         ({TABLE_MIN_COLUMNS}) cells at runs of at least TABLE_GAP_SPACES ({TABLE_GAP_SPACES}) \
         spaces is read from its image and its text (structure), and with vision_every_page on \
         so is every other page with text; pages are rendered at VISION_DPI ({VISION_DPI}) dpi"
    )
}

/// Everything that decides which pages are read and how, as one string: the
/// alias, `vision_every_page`, [`VISION_PROMPT_VERSION`], [`VISION_DPI`] and
/// the table rule's constants — or [`SETTINGS_OFF`]. Each PDF records the
/// string it was indexed with; a PDF whose string differs from this sync's
/// goes through the refresh pass.
pub fn settings(vision: Option<&VisionOptions>) -> String {
    match vision {
        None => SETTINGS_OFF.to_string(),
        Some(v) => format!(
            "model={} every_page={} prompt_version={VISION_PROMPT_VERSION} dpi={VISION_DPI} \
             table_min_rows={TABLE_MIN_ROWS} table_min_columns={TABLE_MIN_COLUMNS} \
             table_gap_spaces={TABLE_GAP_SPACES}",
            v.model, v.every_page
        ),
    }
}

/// Whether a chunk's heading is a reading's ([`heading`]) rather than a page
/// of the PDF's own text (`page N`).
pub fn is_reading(heading_path: &str) -> bool {
    heading_path
        .strip_prefix("page ")
        .map(|rest| rest.trim_start_matches(|c: char| c.is_ascii_digit()))
        .is_some_and(|rest| rest.starts_with(", read by "))
}

/// Width and height of a PNG, from its `IHDR` chunk; `None` when `png` is
/// not one.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") || png.get(12..16)? != b"IHDR" {
        return None;
    }
    let be = |at: usize| Some(u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?));
    Some((be(16)?, be(20)?))
}

/// The probe a vision model is sent when it refuses a page as an input
/// (`400`, `413`, `422`): a uniform grey PNG of `width` × `height` pixels —
/// the refused page image's own size, so it costs the model what the page
/// did, and carries nothing of the page. Built here byte by byte (grey
/// 8-bit, unfiltered rows, stored — uncompressed — deflate blocks), so it
/// needs neither a renderer nor an image crate. A model that refuses this
/// too refuses every image of that size, and the fault is the model's, not
/// the page's.
pub fn probe_png(width: u32, height: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in bytes {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn adler32(bytes: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for x in bytes {
            a = (a + u32::from(*x)) % 65_521;
            b = (b + a) % 65_521;
        }
        (b << 16) | a
    }
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend(u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
        let mut typed = kind.to_vec();
        typed.extend_from_slice(data);
        png.extend(&typed);
        png.extend(crc32(&typed).to_be_bytes());
    }
    // Mid grey: an image with nothing on it, of the page's size.
    const GREY: u8 = 0x80;
    let (w, h) = (width.max(1), height.max(1));
    let mut raw = Vec::with_capacity(h as usize * (w as usize + 1));
    for _ in 0..h {
        raw.push(0); // filter: none
        raw.extend(std::iter::repeat_n(GREY, w as usize));
    }
    // A stored deflate block holds at most 65 535 bytes: as many as it takes.
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(usize::from(u16::MAX)).collect();
    for (i, block) in blocks.iter().enumerate() {
        let len = u16::try_from(block.len()).expect("a block is at most u16::MAX bytes");
        zlib.push(u8::from(i + 1 == blocks.len())); // BFINAL on the last, BTYPE 00
        zlib.extend(len.to_le_bytes());
        zlib.extend((!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend(adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 0, 0, 0, 0]); // 8-bit, grey, deflate, adaptive, no interlace
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// The heading a reading's chunks carry: `page N, read by <alias>` — the page
/// first, so [`crate::chat::page_of`] finds it as it finds a text chunk's
/// `page N`.
pub fn heading(page: u32, model: &str) -> String {
    format!("page {page}, read by {model}")
}

/// The line above a reading in the indexed text.
pub fn header(page: u32, model: &str) -> String {
    format!("\n\n[page {page}, read by {model} from the page image]\n")
}

/// Append `readings` (in page order) to a PDF's `pdftotext` output: for each,
/// [`header`], the reading, and a newline. What the sync indexes and what the
/// MCP `read` tool returns are both built here, so a citation's lines resolve
/// in the tool's text. Returns where each reading landed.
pub fn append_readings(text: &mut String, readings: &[Reading]) -> Vec<ReadingSpan> {
    let mut out = Vec::with_capacity(readings.len());
    for r in readings {
        text.push_str(&header(r.page, &r.model));
        let start = text.len();
        text.push_str(&r.text);
        let end = text.len();
        text.push('\n');
        out.push(ReadingSpan {
            page: r.page,
            heading: heading(r.page, &r.model),
            start,
            end,
        });
    }
    out
}

/// A model's answer as it is stored: when the whole answer is one fenced
/// block (```` ``` ```` or ```` ```text ````, and no other fence inside), the
/// fence is removed; leading blank lines and trailing whitespace go. Nothing
/// else is touched.
pub fn clean(answer: &str) -> String {
    let t = answer.trim();
    let unfenced = t
        .strip_prefix("```")
        .and_then(|rest| rest.split_once('\n'))
        .and_then(|(lang, body)| {
            let body = body.strip_suffix("```")?;
            let inner_fence = body.lines().any(|l| l.trim_start().starts_with("```"));
            (!lang.contains('`') && !inner_fence).then_some(body)
        });
    unfenced
        .unwrap_or(t)
        .trim_start_matches(['\n', '\r'])
        .trim_end()
        .to_string()
}

/// A model's answer for one page as it is stored ([`clean`]), or why it
/// cannot be: a reading the model's context stopped (`finish_reason`
/// `length`) is partial, and storing it would cut the page off invisibly; an
/// answer with no text reads nothing.
pub fn usable(answer: &PageReading) -> Result<String, String> {
    if answer.finish_reason.as_deref() == Some("length") {
        return Err(
            "the reading stopped at the vision model's length limit (finish_reason length), so \
             only part of the page was read and nothing is stored; give the model a larger \
             context, or pick one that has it, then press Retry failed pages"
                .to_string(),
        );
    }
    let text = clean(&answer.text);
    if text.is_empty() {
        return Err("the vision model answered with no text".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_is_three_lines_of_four_cells_at_wide_gaps() {
        let table = "Wage slip September\n\
                     Item          Month        Year to date     Rate\n\
                     Gross         3.250,00     29.250,00        1,00\n\
                     Tax             715,00     6.435,00         0,22\n";
        assert!(table_like(table));
        assert_eq!(pick(table, false), Some(Mode::Structure));
        // Two rows are not a table.
        let two: String = table.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(!table_like(&two));
        // Prose with a sidebar: three columns at most.
        let prose = "The quick brown fox   jumps over the lazy   sidebar note\n".repeat(10);
        assert!(!table_like(&prose));
        assert_eq!(pick(&prose, false), None);
        assert_eq!(pick(&prose, true), Some(Mode::Structure));
        // Two spaces are one cell's words, not a column gap.
        assert_eq!(cells("a  b  c  d"), 1);
        assert_eq!(cells("   a   b   c   d   "), 4);
        assert_eq!(pick(" \n\n", false), Some(Mode::Ocr));
    }

    #[test]
    fn pages_are_numbered_like_pdf_pages() {
        let out = "first\n\u{0c}\u{0c}third\n\u{0c}";
        assert_eq!(pages(out), ["first\n", "", "third\n"]);
        assert_eq!(select(out, false), [(2, Mode::Ocr)]);
        assert_eq!(
            select(out, true),
            [(1, Mode::Structure), (2, Mode::Ocr), (3, Mode::Structure)]
        );
        assert_eq!(crate::chunk::pdf_pages(out), (3, vec![2]));
    }

    #[test]
    fn the_structure_prompt_carries_the_page_text() {
        let p = prompt(Mode::Structure, "\n\n  Item   Month\n  Gross  1,00  \n\n");
        assert!(p.starts_with(STRUCTURE_PROMPT), "{p}");
        assert!(
            p.ends_with("Extracted text:\n  Item   Month\n  Gross  1,00"),
            "{p}"
        );
        assert_eq!(prompt(Mode::Ocr, "ignored"), OCR_PROMPT);
    }

    #[test]
    fn one_wrapping_fence_is_removed_and_nothing_else() {
        assert_eq!(clean("```\nA | B: 1\n```"), "A | B: 1");
        assert_eq!(
            clean("  ```text\nline one\nline two\n```  \n"),
            "line one\nline two"
        );
        assert_eq!(clean("\n\nplain\n  indented  \n"), "plain\n  indented");
        // Two blocks, or a fence that does not wrap everything, stay.
        let two = "```\na\n```\nbetween\n```\nb\n```";
        assert_eq!(clean(two), two);
        assert_eq!(clean("intro\n```\ncode\n```"), "intro\n```\ncode\n```");
        assert_eq!(clean("```\n```"), "");
    }

    #[test]
    fn both_prompts_carry_the_worked_table_example() {
        let table = "For every table, write one line per non-empty cell in the form '<row \
                     heading> | <column heading>: <value>'. The row heading is the label that \
                     starts the value's row, the column heading is the header printed above the \
                     value's column. For example, a row 'Rent   800   750' under the column \
                     headers 'March' and 'April' becomes the two lines 'Rent | March: 800' and \
                     'Rent | April: 750'. Use the headings exactly as";
        for (p, tail) in [
            (
                OCR_PROMPT,
                " printed and skip empty cells. Output only the transcription.",
            ),
            (
                STRUCTURE_PROMPT,
                " they appear and skip empty cells. Output only the rewritten text.\n\nExtracted \
                 text:\n",
            ),
        ] {
            assert!(p.contains(&format!("{table}{tail}")), "{p}");
        }
        assert!(OCR_PROMPT.starts_with(
            "Transcribe this document page as plain text. Reproduce every piece of text exactly \
             as printed, in reading order"
        ));
        assert_eq!(VISION_PROMPT_VERSION, "2");
    }

    #[test]
    fn a_reading_heading_is_told_from_a_page_heading() {
        assert!(is_reading(&heading(12, "gemma4-12b")));
        assert!(!is_reading("page 12"));
        assert!(!is_reading("# Notes > ## page 3, read by x"));
    }

    #[test]
    fn the_probe_is_a_png_of_the_size_asked() {
        // An A4 page at VISION_DPI: many stored blocks.
        let png = probe_png(1240, 1754);
        assert_eq!(png_size(&png), Some((1240, 1754)));
        assert!(
            png.ends_with(&[0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]),
            "IEND"
        );
        assert_eq!(png_size(&probe_png(3, 2)), Some((3, 2)));
        assert_eq!(png_size(b"not a png at all, not at all"), None);
    }

    #[test]
    fn a_cut_off_or_empty_answer_is_not_a_reading() {
        let answer = |text: &str, finish: Option<&str>| PageReading {
            text: text.into(),
            finish_reason: finish.map(String::from),
        };
        assert_eq!(
            usable(&answer("```\nA | B: 1\n```", Some("stop"))).as_deref(),
            Ok("A | B: 1")
        );
        let e = usable(&answer("half a pa", Some("length"))).unwrap_err();
        assert!(e.contains("finish_reason length"), "{e}");
        assert!(usable(&answer("  \n ", None)).is_err());
        assert!(usable(&answer("```\n\n```", None)).is_err());
    }

    #[test]
    fn readings_are_appended_under_their_header() {
        let mut text = "page one text\n\u{0c}\u{0c}".to_string();
        let base = text.clone();
        let spans = append_readings(
            &mut text,
            &[Reading {
                page: 2,
                model: "vision-a".into(),
                text: "Scanned words".into(),
            }],
        );
        assert!(text.starts_with(&base), "the pdftotext part is untouched");
        assert_eq!(
            &text[base.len()..],
            "\n\n[page 2, read by vision-a from the page image]\nScanned words\n"
        );
        assert_eq!(spans[0].heading, "page 2, read by vision-a");
        assert_eq!(&text[spans[0].start..spans[0].end], "Scanned words");
        assert_eq!(crate::chat::page_of(&spans[0].heading), Some(2));
    }

    #[test]
    fn the_settings_name_every_input() {
        assert_eq!(settings(None), SETTINGS_OFF);
        let v = VisionOptions {
            model: "vision-a".into(),
            every_page: true,
            gateway: Gateway::new(crate::config::GatewayEnv::new("http://x/v1", None)),
        };
        let s = settings(Some(&v));
        for part in [
            "model=vision-a",
            "every_page=true",
            "prompt_version=2",
            "dpi=150",
            "table_min_rows=3",
            "table_min_columns=4",
            "table_gap_spaces=3",
        ] {
            assert!(s.contains(part), "{part} in {s}");
        }
        let r = rule();
        assert!(
            r.contains("TABLE_MIN_ROWS (3)") && r.contains("VISION_DPI (150)"),
            "{r}"
        );
    }
}
