//! A unit's lines as headings, paragraphs and list items (design §5.1).
//!
//! - The body size is the most common font size by characters across the
//!   document (a page alone can be all title, so the document's is used).
//! - A heading is a run of at most [`HEADING_MAX_LINES`] lines of one size
//!   at least [`HEADING_SIZE_RATIO`] times the body's, or a bold line of
//!   at most [`HEADING_BOLD_MAX_CHARS`] characters at body size that opens
//!   its block (or follows a heading) — unless the body itself is bold. A
//!   heading has at least two letters or digits, so a drop capital is none.
//! - Levels come from the size ranking within the document: the largest
//!   heading size is `#`, the next `##`, bold headings at body size come
//!   after all sizes; past six levels everything is `######`. Only sizes
//!   more than one heading has are ranked, and the first page's largest (the
//!   title): a one-off size (a label inside a figure) takes the level of the
//!   ranked size above it. Text set vertically (a margin note) is no heading.
//! - Side-by-side blocks read row by row (`order`) are one paragraph, a
//!   line a row.
//! - A line starting with a bullet opens a list item; the lines after it in
//!   the block go on with it.
//! - Lines are joined with a space; a word broken by a hyphen at a line's
//!   end is joined again when the next line goes on in lower case (what
//!   `pdftotext` does by default).
//! - A paragraph whose first line is a figure caption (`pictures::caption`)
//!   ends with the caption's last line, the first one shorter than
//!   [`CAPTION_LAST_LINE_SHARE`] of the run's widest: poppler often gives a
//!   caption and the paragraph under it as one block, and the paragraph
//!   stays a paragraph of its own.

use std::collections::{BTreeMap, HashMap};

use super::super::doc::{Block, BlockKind, Rect, Source};
use super::order::{Unit, What};
use super::table::Found;
use super::{Line, Page, Role};

/// How much larger than the body a heading's size is.
pub const HEADING_SIZE_RATIO: f32 = 1.15;

/// The most lines one heading has; a longer run of large text is a
/// paragraph (a pull quote, a slide's text).
pub const HEADING_MAX_LINES: usize = 3;

/// The longest bold line at body size that is a heading.
pub const HEADING_BOLD_MAX_CHARS: usize = 80;

/// A size in half points, the key sizes are compared by.
pub(super) fn key(size: f32) -> i64 {
    (size * 2.0).round() as i64
}

/// The document's body.
#[derive(Debug, Clone, Copy)]
pub(super) struct Style {
    body: f32,
    bold_body: bool,
}

impl Style {
    /// The body's font size in points; 0 when the document has no text.
    pub fn body(&self) -> f32 {
        self.body
    }

    pub fn of(pages: &[Page]) -> Self {
        let mut by_size: HashMap<i64, (u64, u64)> = HashMap::new();
        for l in pages.iter().flat_map(|p| &p.lines) {
            if matches!(l.role, Role::Header(_) | Role::Footer(_)) {
                continue;
            }
            let n = l.text.chars().count() as u64;
            let e = by_size.entry(key(l.size)).or_default();
            e.0 += n;
            if l.bold {
                e.1 += n;
            }
        }
        // The most characters; a tie goes to the smaller size.
        let body = by_size
            .iter()
            .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.0.cmp(a.0)))
            .map(|(k, v)| (*k, *v));
        match body {
            Some((k, (chars, bold))) => Self {
                body: k as f32 / 2.0,
                bold_body: bold * 2 > chars,
            },
            None => Self {
                body: 0.0,
                bold_body: false,
            },
        }
    }
}

/// What one line may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Large: its size key.
    Big(i64),
    /// Bold at body size and short.
    Bold,
    Body,
}

fn kind(l: &Line, s: &Style) -> Kind {
    let alnum = l
        .text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .count();
    // Text set vertically (a margin note) has a box taller than wide.
    let vertical = l.rect.height() > l.rect.width();
    if alnum < 2 || s.body <= 0.0 || vertical {
        return Kind::Body;
    }
    if l.size >= s.body * HEADING_SIZE_RATIO {
        return Kind::Big(key(l.size));
    }
    if l.bold
        && !s.bold_body
        && l.size >= s.body * 0.9
        && l.text.chars().count() <= HEADING_BOLD_MAX_CHARS
    {
        return Kind::Bold;
    }
    Kind::Body
}

/// One piece of a page.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seg {
    /// A heading: its size key, or `None` for bold at body size.
    Heading(Option<i64>, String, Rect),
    Para(String, Rect),
    Item(String, Rect),
    Table(Found),
    /// A block made elsewhere, as it is (a layout model's figure, caption,
    /// formula or code region: [`super::regions`]).
    Block(Block),
}

/// The heading sizes the document has, for the levels: how many headings
/// each size has, and the first page's largest (the title).
#[derive(Debug, Default)]
pub(super) struct Headings {
    sizes: BTreeMap<i64, u32>,
    title: Option<i64>,
    bold: bool,
}

/// A heading's level by its size key.
pub(super) struct Levels {
    by_key: HashMap<i64, u8>,
    bold: u8,
}

impl Headings {
    /// A heading found elsewhere (a layout model's region): of size `key`,
    /// or `None` for one at body size, ranked like a bold heading.
    pub fn record(&mut self, key: Option<i64>, first_page: bool) {
        match key {
            Some(k) => self.add(k, first_page),
            None => self.bold = true,
        }
    }

    fn add(&mut self, key: i64, first_page: bool) {
        *self.sizes.entry(key).or_default() += 1;
        if first_page {
            self.title = Some(self.title.map_or(key, |t| t.max(key)));
        }
    }

    /// The levels: the sizes that more than one heading has, and the title's,
    /// ranked largest first; a size only one heading has (a label in a
    /// figure, a margin note) takes the level of the next ranked size above
    /// it, or the first.
    pub fn levels(&self) -> Levels {
        let level = |i: usize| (i + 1).min(6) as u8;
        let ranked: Vec<i64> = self
            .sizes
            .iter()
            .rev()
            .filter(|(k, n)| **n >= 2 || Some(**k) == self.title)
            .map(|(k, _)| *k)
            .collect();
        let mut by_key = HashMap::new();
        for (i, k) in ranked.iter().enumerate() {
            by_key.insert(*k, level(i));
        }
        for k in self.sizes.keys() {
            if !by_key.contains_key(k) {
                // `ranked` is largest first: the last one above `k`.
                let above = ranked.partition_point(|r| *r > *k);
                by_key.insert(*k, level(above.saturating_sub(1)));
            }
        }
        Levels {
            by_key,
            bold: level(ranked.len()),
        }
    }
}

const BULLETS: &[char] = &[
    '•', '●', '○', '◦', '▪', '■', '□', '‣', '⁃', '·', '➢', '►', '▶', '✓',
];

/// A list item's text after its bullet.
pub(super) fn bullet(text: &str) -> Option<&str> {
    let mut chars = text.chars();
    let c = chars.next()?;
    let rest = chars.as_str();
    if BULLETS.contains(&c) {
        return Some(rest.trim_start());
    }
    if matches!(c, '-' | '*' | '–') && rest.starts_with(' ') {
        return Some(rest.trim_start());
    }
    None
}

/// Join `next` onto `text`: a hyphen at the end of a word is dropped when
/// `next` goes on in lower case, otherwise a space goes between.
pub(super) fn join(text: &mut String, next: &str) {
    if text.is_empty() {
        text.push_str(next);
        return;
    }
    let hyphen = text.ends_with('-')
        && text[..text.len() - 1]
            .chars()
            .next_back()
            .is_some_and(char::is_alphabetic)
        && next.chars().next().is_some_and(char::is_lowercase);
    if hyphen {
        text.pop();
    } else {
        text.push(' ');
    }
    text.push_str(next);
}

/// The segments of `page`'s `units`, recording heading sizes in `heads`.
pub(super) fn segments(
    page: &Page,
    units: Vec<Unit>,
    s: &Style,
    heads: &mut Headings,
    first_page: bool,
) -> Vec<Seg> {
    let mut out = Vec::new();
    for u in units {
        let lines = match u.what {
            What::Table(t) => {
                out.push(Seg::Table(t));
                continue;
            }
            // Side-by-side blocks read row by row: one paragraph, a line a
            // row, the row's lines left to right.
            What::Rows(rows) => {
                let text = rows
                    .iter()
                    .map(|r| {
                        r.iter()
                            .filter_map(|&i| page.lines.get(i))
                            .map(|l| l.text.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(Seg::Para(text, u.rect));
                continue;
            }
            // A layout model's region: laid out by `regions`, never here.
            What::Region(_) => continue,
            What::Lines(l) => l,
        };
        let lines: Vec<&Line> = lines.iter().filter_map(|&i| page.lines.get(i)).collect();
        let kinds: Vec<Kind> = lines.iter().map(|l| kind(l, s)).collect();
        let mut i = 0;
        let mut after_heading = true;
        while i < lines.len() {
            let k = kinds[i];
            let mut j = i + 1;
            while j < lines.len() && kinds[j] == k && k != Kind::Body {
                j += 1;
            }
            let heading = match k {
                Kind::Big(_) => j - i <= HEADING_MAX_LINES,
                Kind::Bold => after_heading && j - i <= HEADING_MAX_LINES,
                Kind::Body => false,
            };
            if heading {
                let mut text = String::new();
                for l in &lines[i..j] {
                    join(&mut text, &l.text);
                }
                let rect = Rect::union_all(lines[i..j].iter().map(|l| &l.rect)).unwrap_or_default();
                let size = match k {
                    Kind::Big(key) => {
                        heads.add(key, first_page);
                        Some(key)
                    }
                    _ => {
                        heads.bold = true;
                        None
                    }
                };
                out.push(Seg::Heading(size, text, rect));
                after_heading = true;
                i = j;
                continue;
            }
            // Body: a run of large lines too long for a heading as it is;
            // otherwise the lines up to the next large one. As paragraphs
            // and items.
            if !matches!(k, Kind::Big(_)) {
                while j < lines.len() && !matches!(kinds[j], Kind::Big(_)) {
                    j += 1;
                }
            }
            let mut cur: Option<(bool, String, Rect)> = None;
            // A paragraph that opens with a figure caption ends with the
            // caption's last line, so the text after it stays a paragraph.
            let wide = lines[i..j]
                .iter()
                .map(|l| l.rect.width())
                .fold(0.0, f32::max);
            let (mut caption, mut caption_ended) = (false, false);
            for l in &lines[i..j] {
                let item = bullet(&l.text);
                match (item, cur.as_mut()) {
                    (None, Some((_, text, rect))) if !caption_ended => {
                        join(text, &l.text);
                        *rect = rect.union(&l.rect);
                        caption_ended = caption && last_caption_line(&l.rect, wide);
                    }
                    (item, _) => {
                        if let Some(c) = cur.take() {
                            out.push(seg(c));
                        }
                        cur = Some(match item {
                            Some(rest) => (true, rest.to_string(), l.rect),
                            None => (false, l.text.clone(), l.rect),
                        });
                        caption =
                            item.is_none() && super::super::pictures::caption::is_caption(&l.text);
                        caption_ended = caption && last_caption_line(&l.rect, wide);
                    }
                }
            }
            if let Some(c) = cur.take() {
                out.push(seg(c));
            }
            after_heading = false;
            i = j;
        }
    }
    out
}

/// A caption's last line is shorter than the run's widest by this share.
pub const CAPTION_LAST_LINE_SHARE: f32 = 0.85;

/// Whether a caption's `line` is its last (shorter than
/// [`CAPTION_LAST_LINE_SHARE`] of `wide`, the run's widest line): the line
/// after it opens a paragraph of its own.
fn last_caption_line(line: &Rect, wide: f32) -> bool {
    line.width() < CAPTION_LAST_LINE_SHARE * wide
}

fn seg((item, text, rect): (bool, String, Rect)) -> Seg {
    if item {
        Seg::Item(text, rect)
    } else {
        Seg::Para(text, rect)
    }
}

/// The blocks of `segs`, headings at their levels.
pub(super) fn blocks(segs: Vec<Seg>, levels: &Levels) -> Vec<Block> {
    let tl = Source::TextLayer;
    segs.into_iter()
        .map(|s| match s {
            Seg::Heading(size, text, r) => {
                let level = size
                    .and_then(|k| levels.by_key.get(&k).copied())
                    .unwrap_or(levels.bold);
                Block::new(BlockKind::Heading(level), text, Some(r), tl)
            }
            Seg::Para(text, r) => Block::new(BlockKind::Paragraph, text, Some(r), tl),
            Seg::Item(text, r) => Block::new(BlockKind::ListItem, text, Some(r), tl),
            Seg::Table(t) => Block::new(BlockKind::Table(t.table), String::new(), Some(t.rect), tl),
            Seg::Block(b) => b,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyphens_join_broken_words_only() {
        let mut t = String::from("exam-");
        join(&mut t, "ple text");
        assert_eq!(t, "example text");
        let mut t = String::from("well-");
        join(&mut t, "Known");
        assert_eq!(t, "well- Known");
        let mut t = String::from("2026-");
        join(&mut t, "later");
        assert_eq!(t, "2026- later");
    }

    #[test]
    fn bullets_open_items() {
        assert_eq!(bullet("• one"), Some("one"));
        assert_eq!(bullet("•one"), Some("one"));
        assert_eq!(bullet("- two"), Some("two"));
        assert_eq!(bullet("-5 % less"), None);
        assert_eq!(bullet("plain"), None);
    }

    #[test]
    fn levels_rank_sizes_then_bold() {
        let mut h = Headings::default();
        // The title once on the first page, a figure label larger than it
        // once, sections and subsections many times, a one-off in between.
        h.add(40, true);
        h.add(28, true);
        h.add(60, false);
        for _ in 0..5 {
            h.add(28, false);
            h.add(24, false);
        }
        h.add(26, false);
        h.bold = true;
        let l = h.levels();
        assert_eq!(l.by_key[&40], 1);
        assert_eq!(l.by_key[&60], 1);
        assert_eq!(l.by_key[&28], 2);
        assert_eq!(l.by_key[&26], 2);
        assert_eq!(l.by_key[&24], 3);
        assert_eq!(l.bold, 4);
        // Many ranked sizes end at six.
        let mut h = Headings::default();
        for k in 20..30 {
            h.add(k, false);
            h.add(k, false);
        }
        assert_eq!(h.levels().by_key[&20], 6);
    }
}
