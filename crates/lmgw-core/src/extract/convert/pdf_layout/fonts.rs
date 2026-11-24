//! Font size and weight for each line (design §5.1): `pdftohtml -xml -i
//! -zoom 1 -noroundcoord` (poppler 26.08, checked 2026-11-18) writes
//! `<page number>`, `<fontspec id size family>` (ids count across pages,
//! size in points rounded to a whole one) and `<text top left width height
//! font>` runs whose text may sit in `<b>`, `<i>` and `<a>`. At zoom 1 its
//! coordinates are the same points `pdftotext -bbox-layout` writes, so a
//! line is matched to the runs whose vertical centre lies inside it and that
//! overlap it sideways.
//!
//! A run is bold when most of its characters are inside `<b>`, or its font's
//! family names a bold face (`Bold`, `Black`, `Heavy`, `Demi`, `Semibold`, a
//! TeX-style `…TB`/`…-Bd`): poppler writes `<b>` only from the font's flags,
//! and many embedded fonts carry the weight only in their name (seen in the
//! WP0a corpus).
//!
//! poppler writes `<b>` and `<i>` overlapping where a line's weight and
//! slant change in different places (`<b>a <i>b </b>c</i>`, poppler 26.08),
//! so end tags are not matched to their start tags: `<b>` is a depth count.
//! Output that stops being XML anyway keeps the runs read up to there, and
//! the error is the file's `part_failed` warning.
//!
//! Without runs ([`from_boxes`]), a line's size is its box's height and no
//! line is bold.

use std::collections::{BTreeMap, HashMap};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::super::doc::Rect;
use super::{finite, Page};

/// How many runs (and rows of runs) one line is matched against at most:
/// the runs whose left edge is nearest the line's right edge. A line of ordinary text overlaps a
/// handful of runs; text stacked on itself (a hostile or broken PDF) could
/// make every line overlap every run, which would cost lines × runs. Only the
/// font facts are sampled this way: no text is left out.
pub const FONT_RUNS_PER_LINE: usize = 64;

/// One text run.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Run {
    pub rect: Rect,
    pub size: f32,
    pub bold: bool,
}

/// The runs of each page, by the page's number.
pub(super) type Runs = HashMap<u32, Vec<Run>>;

/// What the font output gave: the runs read, and where it stopped being
/// readable, if it did.
#[derive(Debug, Default)]
pub(super) struct Read {
    pub runs: Runs,
    pub error: Option<String>,
    /// Each page's size as displayed (`/Rotate` applied), by its number:
    /// `pdftotext -bbox-layout` gives the unturned one.
    pub sizes: HashMap<u32, (f32, f32)>,
}

fn attrs(e: &BytesStart<'_>) -> HashMap<Vec<u8>, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            (
                a.key.as_ref().to_vec(),
                String::from_utf8_lossy(&a.value).into_owned(),
            )
        })
        .collect()
}

/// Whether a font family names a bold face (the subset prefix `ABCDEF+`
/// dropped).
fn bold_family(family: &str) -> bool {
    let name = family.split_once('+').map_or(family, |(_, n)| n);
    let lower = name.to_ascii_lowercase();
    ["bold", "black", "heavy", "demi", "semibd", "-bd"]
        .iter()
        .any(|w| lower.contains(w))
        || name.ends_with("TB")
}

/// The text run being read.
struct Open {
    rect: Rect,
    size: f32,
    family_bold: bool,
    chars: u32,
    bold_chars: u32,
}

/// The runs of `xml`, up to where it stops being readable.
pub(super) fn runs(xml: &[u8]) -> Read {
    let mut reader = Reader::from_reader(xml);
    // poppler's `<b>`/`<i>` overlap (module doc).
    reader.config_mut().check_end_names = false;
    let mut fonts: HashMap<String, (f32, bool)> = HashMap::new();
    let mut out: Runs = HashMap::new();
    let mut sizes: HashMap<u32, (f32, f32)> = HashMap::new();
    let mut page = 0u32;
    let mut open: Option<Open> = None;
    // `<b>` depth inside the open run; saturating, so a hostile nesting is a
    // count, never a stack.
    let mut bold = 0u32;
    let mut buf = Vec::new();
    loop {
        let ev = match reader.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(e) => {
                let at = reader.buffer_position();
                return Read {
                    runs: out,
                    error: Some(format!("stops being readable XML at byte {at} ({e})")),
                    sizes,
                };
            }
        };
        match ev {
            ev @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(ev, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = ev else {
                    continue;
                };
                match e.local_name().as_ref() {
                    b"page" => {
                        let a = attrs(&e);
                        page = a
                            .get(b"number".as_slice())
                            .and_then(|n| n.trim().parse().ok())
                            .unwrap_or(page.saturating_add(1));
                        let n = |k: &[u8]| a.get(k).map_or(0.0, |v| finite(v));
                        let (w, h) = (n(b"width"), n(b"height"));
                        if w > 0.0 && h > 0.0 {
                            sizes.insert(page, (w, h));
                        }
                    }
                    b"fontspec" => {
                        let a = attrs(&e);
                        if let Some(id) = a.get(b"id".as_slice()) {
                            let size = a.get(b"size".as_slice()).map_or(0.0, |s| finite(s));
                            let fam = a.get(b"family".as_slice()).map(|f| bold_family(f));
                            fonts.insert(id.clone(), (size.max(0.0), fam.unwrap_or(false)));
                        }
                    }
                    b"text" if !empty => {
                        let a = attrs(&e);
                        let n = |k: &[u8]| a.get(k).map_or(0.0, |v| finite(v));
                        let (top, left) = (n(b"top"), n(b"left"));
                        let (w, h) = (n(b"width").max(0.0), n(b"height").max(0.0));
                        let (size, family_bold) = a
                            .get(b"font".as_slice())
                            .and_then(|f| fonts.get(f))
                            .copied()
                            .unwrap_or((0.0, false));
                        open = Some(Open {
                            rect: Rect {
                                x0: left,
                                y0: top,
                                x1: left + w,
                                y1: top + h,
                            },
                            size,
                            family_bold,
                            chars: 0,
                            bold_chars: 0,
                        });
                        bold = 0;
                    }
                    b"b" if !empty => bold = bold.saturating_add(1),
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"b" => bold = bold.saturating_sub(1),
                b"text" => {
                    if let Some(o) = open.take() {
                        if o.chars > 0 {
                            out.entry(page).or_default().push(Run {
                                rect: o.rect,
                                size: o.size,
                                bold: o.family_bold || o.bold_chars * 2 > o.chars,
                            });
                        }
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(o) = open.as_mut() {
                    let n = String::from_utf8_lossy(t.as_ref())
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .count() as u32;
                    o.chars = o.chars.saturating_add(n);
                    if bold > 0 {
                        o.bold_chars = o.bold_chars.saturating_add(n);
                    }
                }
            }
            Event::GeneralRef(_) => {
                if let Some(o) = open.as_mut() {
                    o.chars = o.chars.saturating_add(1);
                    if bold > 0 {
                        o.bold_chars = o.bold_chars.saturating_add(1);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Read {
        runs: out,
        error: None,
        sizes,
    }
}

/// A page's runs, for lookups by line: rows keyed by the run's vertical
/// centre (whole points), each sorted by its left edge.
struct Index<'a> {
    rows: BTreeMap<i64, Vec<&'a Run>>,
}

impl<'a> Index<'a> {
    fn new(runs: &'a [Run]) -> Self {
        let mut rows: BTreeMap<i64, Vec<&Run>> = BTreeMap::new();
        for r in runs {
            rows.entry(r.rect.cy().floor() as i64).or_default().push(r);
        }
        for row in rows.values_mut() {
            row.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0));
        }
        Self { rows }
    }

    /// The size and weight of the line at `r`: the size of the run that
    /// covers most of it, bold when bold runs cover nine tenths of what is
    /// covered. `None` when no run overlaps it.
    fn facts(&self, r: &Rect) -> Option<(f32, bool)> {
        let (lo, hi) = (r.y0.floor() as i64, r.y1.ceil() as i64);
        let mut best: Option<(f32, f32)> = None;
        let (mut covered, mut bold) = (0.0f32, 0.0f32);
        let mut seen = 0usize;
        for row in self.rows.range(lo..=hi).map(|(_, v)| v) {
            // A row counts too: a line as tall as the page must not visit
            // every row of it.
            seen += 1;
            if seen >= FONT_RUNS_PER_LINE {
                break;
            }
            let end = row.partition_point(|x| x.rect.x0 < r.x1);
            for run in row[..end].iter().rev() {
                if seen >= FONT_RUNS_PER_LINE {
                    break;
                }
                seen += 1;
                let cy = run.rect.cy();
                if cy < r.y0 || cy > r.y1 {
                    continue;
                }
                let over = run.rect.x1.min(r.x1) - run.rect.x0.max(r.x0);
                if over <= 0.0 {
                    continue;
                }
                covered += over;
                if run.bold {
                    bold += over;
                }
                if best.is_none_or(|(o, _)| over > o) {
                    best = Some((over, run.size));
                }
            }
        }
        best.map(|(_, size)| (size, covered > 0.0 && bold >= covered * 0.9))
    }
}

/// Each line's size and weight from `runs`; the pages are numbered from
/// `first`. A line no run overlaps gets its box's height scaled by the
/// document's own size-to-height ratio, and is not bold. Answers the pages
/// that have lines but no runs at all (the font run covered fewer pages, or
/// stopped early).
pub(super) fn apply(pages: &mut [Page], runs: &Runs, first: u32) -> Vec<u32> {
    let (mut sizes, mut heights) = (0.0f64, 0.0f64);
    let mut unmatched = Vec::new();
    let mut without = Vec::new();
    for (i, page) in pages.iter_mut().enumerate() {
        let number = first.saturating_add(i as u32);
        let Some(page_runs) = runs.get(&number) else {
            if !page.lines.is_empty() {
                without.push(number);
            }
            unmatched.extend((0..page.lines.len()).map(|l| (i, l)));
            continue;
        };
        let index = Index::new(page_runs);
        for (l, line) in page.lines.iter_mut().enumerate() {
            match index.facts(&line.rect) {
                Some((size, bold)) if size > 0.0 => {
                    line.size = size;
                    line.bold = bold;
                    sizes += f64::from(size);
                    heights += f64::from(line.rect.height().max(0.0));
                }
                _ => unmatched.push((i, l)),
            }
        }
    }
    let ratio = if heights > 0.0 {
        (sizes / heights) as f32
    } else {
        1.0
    };
    for (i, l) in unmatched {
        if let Some(line) = pages.get_mut(i).and_then(|p| p.lines.get_mut(l)) {
            line.size = line.rect.height().max(0.0) * ratio;
            line.bold = false;
        }
    }
    without
}

/// Without runs: each line's size is its box's height.
pub(super) fn from_boxes(pages: &mut [Page]) {
    for line in pages.iter_mut().flat_map(|p| p.lines.iter_mut()) {
        line.size = line.rect.height().max(0.0);
        line.bold = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_faces_are_told_by_name() {
        for f in [
            "SEKJLF+LinLibertineTB",
            "Helvetica-Bold",
            "ABCDEF+Arial,Bold",
            "X+Avenir-Heavy",
        ] {
            assert!(bold_family(f), "{f}");
        }
        for f in [
            "RWPIRK+LinLibertineT",
            "Helvetica",
            "TKQZJF+LinLibertineTI",
            "TB+Times",
        ] {
            assert!(!bold_family(f), "{f}");
        }
    }

    #[test]
    fn runs_carry_size_and_weight_and_bold_tags_nest_as_a_count() {
        let xml = br##"<pdf2xml><page number="3" width="595" height="842">
<fontspec id="0" size="10" family="A+Serif" color="#000000"/>
<fontspec id="1" size="20" family="B+Serif" color="#000000"/>
<text top="10" left="20" width="100" height="22" font="1"><b>Example Report</b></text>
<text top="40" left="20" width="300" height="11" font="0">plain &amp; <b><b>x</b></b> words</text>
<text top="60" left="20" width="0" height="11" font="9"></text>
</page></pdf2xml>"##;
        let r = runs(xml);
        assert!(r.error.is_none());
        let p = &r.runs[&3];
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].size, p[0].bold), (20.0, true));
        assert_eq!((p[1].size, p[1].bold), (10.0, false));
        assert_eq!(p[1].rect.x1, 320.0);
    }

    /// What poppler 26.08 writes for a line set Bold, BoldOblique, Oblique,
    /// BoldOblique, plain: `<b>` and `<i>` overlap. The runs are read, bold
    /// where most characters are inside `<b>`.
    #[test]
    fn overlapping_bold_and_italic_tags_are_read() {
        let xml = br##"<pdf2xml><page number="1" width="595" height="842">
<fontspec id="0" size="10" family="A+Serif" color="#000000"/>
<text top="10" left="20" width="200" height="11" font="0"><b>Bold <i>BoldItalic </b>Italic <b>BI2 </b></i>plain</text>
<text top="30" left="20" width="200" height="11" font="0"><i>mostly plain <b>x</i></b> text</text>
</page></pdf2xml>"##;
        let r = runs(xml);
        assert!(r.error.is_none(), "{:?}", r.error);
        let p = &r.runs[&1];
        assert_eq!(p.len(), 2);
        // 17 of 28 characters inside `<b>`: bold; 1 of 17: not.
        assert!(p[0].bold);
        assert!(!p[1].bold);
    }

    /// Output that stops being XML keeps what was read before.
    #[test]
    fn broken_output_keeps_the_runs_before_it() {
        let xml = br##"<pdf2xml><page number="1" width="595" height="842">
<fontspec id="0" size="20" family="A+Serif" color="#000000"/>
<text top="10" left="20" width="200" height="22" font="0">Title</text>
</page><page number="2"><text top="10" left="20" width="9" height="9" font="0">x</text><<"##;
        let r = runs(xml);
        assert!(
            r.error.as_deref().is_some_and(|e| e.contains("byte")),
            "{:?}",
            r.error
        );
        assert_eq!(r.runs[&1].len(), 1);
    }
}
