//! Repeated page headers and footers (design §5.1): a line in the top or
//! bottom band of a page that appears in the same band on enough of the
//! pages is marked a header or footer. They stay in `json` as
//! `page_header`/`page_footer` blocks, markdown and text leave them out, and
//! the file's warnings name every one (`header_removed`).
//!
//! Two lines are the same when their text is the same (whitespace
//! collapsed), or when they differ only in a page number: the first or last
//! run of at most [`PAGE_NUMBER_MAX_DIGITS`] digits, whose value minus the
//! page's index is the same on each page ("Page 3 of 9" on the third page,
//! "Page 4 of 9" on the fourth). Any other difference keeps them apart, so a
//! line that changes in another way (an amount carried forward, a slide's
//! "Exercise 2") is never removed as a variant of another, and each text
//! removed is named in the warning. Lines a table holds are never headers
//! (a long table's header row repeated at the top of each page stays).
//!
//! The band and the threshold were measured on the WP0a corpus (docling's
//! `tests/data` at `5e600e8d`, 2026-11-18, poppler 26.08): running headers
//! sit between 4 % and 13 % of the page height from the top (Elsevier 4 %,
//! ACM 8 %, LNCS 11.5 % with the page number), footers between 89 % and 97 %.
//! Papers alternate their headers between odd and even pages (author line /
//! title line), so each variant is on just under half the pages, and an IBM
//! Redbook's chapter footers on a third: hence a third, not half. Re-check
//! against the WP0b numbers.

use std::collections::HashMap;

use super::{Page, Role};

/// The top band: a line starting within this share of the page's height
/// from the top; the bottom band: a line ending within it from the bottom.
pub const HEADER_BAND: f32 = 0.13;

/// The least share of the pages a line must repeat on (and at least
/// [`HEADER_MIN_PAGES`] pages).
pub const HEADER_MIN_SHARE: f32 = 1.0 / 3.0;

/// A line on one page only is never "repeated".
pub const HEADER_MIN_PAGES: u32 = 2;

/// The space that sets a header (footer) apart from the text, in heights
/// of its tallest line.
pub const HEADER_GAP_LINES: f32 = 1.0;

/// The longest run of digits read as a page number.
pub const PAGE_NUMBER_MAX_DIGITS: usize = 5;

/// Which band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Band {
    Top,
    Bottom,
}

/// One repeated line, as the warning names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repeated {
    pub band: Band,
    /// The text as it is on the first page it was found on.
    pub text: String,
    /// It differs from page to page only in its page number.
    pub numbered: bool,
}

/// A line's identity across pages: its band, its text (with the page
/// number as `#` when `offset` is set), and the page number's value minus
/// the page's index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    band: Band,
    text: String,
    offset: Option<i64>,
}

/// `text` with whitespace collapsed.
fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The page-number keys of `text` (collapsed) on the page at `index`: its
/// first and last runs of at most [`PAGE_NUMBER_MAX_DIGITS`] digits, each
/// as `#` with its offset.
fn numbered(band: Band, text: &str, index: usize) -> Vec<Key> {
    let b = text.as_bytes();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            // Only the first and the last run are kept.
            if runs.len() == 2 {
                runs[1] = (start, i);
            } else {
                runs.push((start, i));
            }
        } else {
            i += 1;
        }
    }
    runs.into_iter()
        .filter(|(a, z)| z - a <= PAGE_NUMBER_MAX_DIGITS)
        .filter_map(|(a, z)| {
            let v: i64 = text[a..z].parse().ok()?;
            Some(Key {
                band,
                text: format!("{}#{}", &text[..a], &text[z..]),
                offset: Some(v - index as i64),
            })
        })
        .collect()
}

/// Each line's band, if it is a candidate: the lines at the top of the
/// page down to the first gap of at least [`HEADER_GAP_LINES`] line heights,
/// all of them starting in the top band (and likewise from the bottom). A
/// running header is set apart from the text by space; a body line that
/// happens to sit in the band and repeat is not.
fn candidates(page: &Page) -> Vec<Option<Band>> {
    let n = page.lines.len();
    let mut out = vec![None; n];
    let h = page.height;
    if h <= 0.0 || n == 0 {
        return out;
    }
    let r = |i: usize| page.lines[i].rect;
    let mut by_top: Vec<usize> = (0..n).collect();
    by_top.sort_by(|&a, &b| r(a).y0.total_cmp(&r(b).y0));
    // Top: the run before the first gap, if it stays in the band.
    let region = |order: &[usize],
                  inside: &dyn Fn(usize) -> bool,
                  gap: &dyn Fn(f32, usize) -> f32,
                  edge: &dyn Fn(usize) -> f32|
     -> usize {
        let Some(&first) = order.first() else {
            return 0;
        };
        if !inside(first) {
            return 0;
        }
        let (mut reach, mut tall) = (edge(first), r(first).height());
        for (k, &i) in order.iter().enumerate().skip(1) {
            if gap(reach, i) >= HEADER_GAP_LINES * tall {
                return k;
            }
            if !inside(i) {
                return 0;
            }
            reach = if edge(i) > reach { edge(i) } else { reach };
            tall = tall.max(r(i).height());
        }
        // The whole page is in the band and close together: no header.
        0
    };
    // A table's line is never a header, and ends the run.
    let body = |i: usize| page.lines[i].role == Role::Body;
    let top = region(
        &by_top,
        &|i| body(i) && r(i).y0 <= h * HEADER_BAND,
        &|reach, i| r(i).y0 - reach,
        &|i| r(i).y1,
    );
    for &i in &by_top[..top] {
        out[i] = Some(Band::Top);
    }
    let mut by_bottom: Vec<usize> = (0..n).collect();
    by_bottom.sort_by(|&a, &b| r(b).y1.total_cmp(&r(a).y1));
    // From the bottom the reach is the smallest top, kept negated so the
    // same "further" test applies.
    let bottom = region(
        &by_bottom,
        &|i| body(i) && r(i).y1 >= h * (1.0 - HEADER_BAND),
        &|reach, i| -reach - r(i).y1,
        &|i| -r(i).y0,
    );
    for &i in &by_bottom[..bottom] {
        if out[i].is_none() {
            out[i] = Some(Band::Bottom);
        }
    }
    out
}

/// Mark the repeated lines of `pages` ([`Role::Header`]/[`Role::Footer`]
/// with the index of their [`Repeated`]); answers them in the order first
/// seen.
pub(super) fn mark(pages: &mut [Page]) -> Vec<Repeated> {
    let with_text = pages.iter().filter(|p| !p.lines.is_empty()).count() as u32;
    if with_text < HEADER_MIN_PAGES {
        return Vec::new();
    }
    let need = ((with_text as f32 * HEADER_MIN_SHARE).ceil() as u32).max(HEADER_MIN_PAGES);
    // Each candidate's keys: its exact text first, then its page numbers.
    let keys: Vec<Vec<Option<Vec<Key>>>> = pages
        .iter()
        .enumerate()
        .map(|(p, page)| {
            candidates(page)
                .into_iter()
                .zip(&page.lines)
                .map(|(mark, line)| {
                    let band = mark?;
                    let text = collapsed(&line.text);
                    let mut k = numbered(band, &text, p);
                    k.insert(
                        0,
                        Key {
                            band,
                            text,
                            offset: None,
                        },
                    );
                    Some(k)
                })
                .collect()
        })
        .collect();
    // Per key: pages seen, the last page counted, the first text, its order.
    let mut seen: HashMap<&Key, (u32, usize, &str, usize)> = HashMap::new();
    for (p, (page, marks)) in pages.iter().zip(&keys).enumerate() {
        for (line, k) in page.lines.iter().zip(marks) {
            for key in k.iter().flatten() {
                let order = seen.len();
                let e = seen
                    .entry(key)
                    .or_insert_with(|| (0, usize::MAX, line.text.as_str(), order));
                if e.1 != p {
                    e.0 += 1;
                    e.1 = p;
                }
            }
        }
    }
    // The repeated keys, numbered in the order first seen.
    let mut found: Vec<(usize, &Key, &str)> = seen
        .iter()
        .filter(|(_, e)| e.0 >= need)
        .map(|(k, e)| (e.3, *k, e.2))
        .collect();
    found.sort_by_key(|(o, _, _)| *o);
    let index: HashMap<&Key, u32> = found
        .iter()
        .enumerate()
        .map(|(i, (_, k, _))| (*k, i as u32))
        .collect();
    let mut roles: Vec<Vec<(usize, Role)>> = Vec::with_capacity(pages.len());
    for marks in &keys {
        let mut page_roles = Vec::new();
        for (l, k) in marks.iter().enumerate() {
            let Some(k) = k else {
                continue;
            };
            // The exact text first: a line both repeated as it is and
            // numbered is named as it is.
            if let Some((key, id)) = k.iter().find_map(|key| Some((key, *index.get(key)?))) {
                page_roles.push((
                    l,
                    match key.band {
                        Band::Top => Role::Header(id),
                        Band::Bottom => Role::Footer(id),
                    },
                ));
            }
        }
        roles.push(page_roles);
    }
    let out: Vec<Repeated> = found
        .iter()
        .map(|(_, k, text)| Repeated {
            band: k.band,
            text: (*text).to_string(),
            numbered: k.offset.is_some(),
        })
        .collect();
    for (page, marks) in pages.iter_mut().zip(roles) {
        for (l, role) in marks {
            if let Some(line) = page.lines.get_mut(l) {
                line.role = role;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::super::doc::Rect;
    use super::super::Line;
    use super::*;

    fn line(y: f32, text: &str) -> Line {
        Line {
            rect: Rect {
                x0: 50.0,
                y0: y,
                x1: 300.0,
                y1: y + 10.0,
            },
            text: text.into(),
            size: 10.0,
            bold: false,
            block: 0,
            role: Role::Body,
        }
    }

    fn page(n: u32) -> Page {
        let head = if n.is_multiple_of(2) {
            "Example Corp, even"
        } else {
            "Example Report, odd"
        };
        Page {
            width: 600.0,
            height: 800.0,
            lines: vec![
                line(30.0, head),
                line(70.0, "Body text near the top"),
                line(400.0, "Body"),
                line(760.0, &format!("Page {n} of 9")),
            ],
        }
    }

    fn names(r: &[Repeated]) -> Vec<(Band, &str, bool)> {
        r.iter()
            .map(|r| (r.band, r.text.as_str(), r.numbered))
            .collect()
    }

    #[test]
    fn alternating_headers_and_numbered_footers_are_marked() {
        let mut pages: Vec<Page> = (1..=9).map(page).collect();
        let r = mark(&mut pages);
        assert_eq!(
            names(&r),
            [
                (Band::Top, "Example Report, odd", false),
                (Band::Bottom, "Page 1 of 9", true),
                (Band::Top, "Example Corp, even", false),
            ]
        );
        assert_eq!(pages[0].lines[0].role, Role::Header(0));
        assert_eq!(pages[3].lines[3].role, Role::Footer(1));
        assert_eq!(pages[1].lines[0].role, Role::Header(2));
        // A body line in the band below the header's gap is the text's
        // start, even though it repeats on every page.
        assert_eq!(pages[0].lines[1].role, Role::Body);
        assert_eq!(pages[0].lines[2].role, Role::Body);
    }

    #[test]
    fn one_page_has_no_repeats_and_rare_lines_stay() {
        let mut one = vec![page(1)];
        assert!(mark(&mut one).is_empty());
        assert!(one[0].lines.iter().all(|l| l.role == Role::Body));
        // A top line on 2 of 9 pages is under a third.
        let mut pages: Vec<Page> = (1..=9).map(page).collect();
        for p in pages.iter_mut().skip(2) {
            p.lines[1].text = "different".into();
        }
        mark(&mut pages);
        assert_eq!(pages[0].lines[1].role, Role::Body);
    }

    /// A line whose numbers change in another way than the page's
    /// (amounts carried forward, numbered slide titles not in step with the
    /// pages, quarters) is no repeat of the others: each text stays unless
    /// it repeats as it is, and then it is named.
    #[test]
    fn numbers_that_are_no_page_numbers_keep_lines_apart() {
        let amounts = [
            "1,234.56", "5,001.10", "977.00", "3,500.25", "12.00", "640.80", "88.10",
        ];
        let titles = [1, 1, 2, 2, 9, 9, 13];
        let quarters = [
            "Q3 2025", "Q4 2025", "Q3 2025", "Q4 2025", "Q1 2026", "Q1 2026", "Q4 2026",
        ];
        let mut pages: Vec<Page> = (0..7)
            .map(|i| Page {
                width: 600.0,
                height: 800.0,
                lines: vec![
                    line(30.0, &format!("Balance brought forward {}", amounts[i])),
                    line(60.0, &format!("Exercise {}", titles[i])),
                    line(300.0, "Body"),
                    line(770.0, &format!("{} Results", quarters[i])),
                ],
            })
            .collect();
        let r = mark(&mut pages);
        // Each title and quarter is on two pages of seven at most, under a
        // third.
        assert!(r.is_empty(), "{r:?}");
        assert!(pages
            .iter()
            .all(|p| p.lines.iter().all(|l| l.role == Role::Body)));
        // On five of seven pages, one quarter repeats as it is and is named.
        for p in pages.iter_mut().take(5) {
            p.lines[3].text = "Q3 2025 Results".into();
        }
        let r = mark(&mut pages);
        assert_eq!(names(&r), [(Band::Bottom, "Q3 2025 Results", false)]);
        assert_eq!(pages[5].lines[3].role, Role::Body);
    }

    /// A long table's repeated header row at the top of each page is the
    /// table's, never a page header.
    #[test]
    fn a_table_s_lines_are_never_headers() {
        let mut pages: Vec<Page> = (1..=4).map(page).collect();
        for p in &mut pages {
            p.lines[0].role = Role::Cell(0);
        }
        let r = mark(&mut pages);
        assert_eq!(names(&r), [(Band::Bottom, "Page 1 of 9", true)]);
        assert!(pages.iter().all(|p| p.lines[0].role == Role::Cell(0)));
    }
}
