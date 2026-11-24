//! A long result shown a page at a time (WP12): an 800-page PDF's markdown
//! runs to megabytes and its structure to tens of thousands of blocks, so the
//! viewer never builds the whole document's DOM at once. The text is cut into
//! pages of about [`PAGE_BYTES`] at block boundaries (a blank line outside a
//! code fence), the structure into pages of [`PAGE_BLOCKS`] blocks; the pager
//! says which page of how many is shown and steps through all of them. A page
//! is a unit of display, not a bound: every byte and every block is reachable.
//!
//! Pure and linear in the text: one pass over its lines.

use std::ops::Range;

use lmgw_api_types::documents::structure::DocPart;

/// The text a page of the rendered or raw view holds, about: cut at the
/// first block boundary past it.
pub(super) const PAGE_BYTES: usize = 48 * 1024;
/// A page past this many bytes without a block boundary (one long
/// paragraph, a fenced block of a whole log) is cut at a line instead, and a
/// line this long (the structure's compact JSON is one line) is cut inside
/// itself, at a character boundary.
const FORCE_BYTES: usize = 4 * PAGE_BYTES;
/// The blocks a page of the structure view holds.
pub(super) const PAGE_BLOCKS: usize = 200;

/// The fence a line opens or closes: its marker and run length.
fn fence(line: &str) -> Option<(u8, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = *t.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = t.bytes().take_while(|b| *b == c).count();
    (n >= 3).then_some((c, n))
}

/// `text` cut into pages: byte ranges that cover it in order, each ending at
/// a line end, except a page forced by a line longer than [`FORCE_BYTES`],
/// which ends at a character boundary inside it. `markdown`: cut only at a
/// blank line outside a fence (until [`FORCE_BYTES`]); otherwise at any line
/// end once a page is full.
pub(super) fn pages(text: &str, markdown: bool) -> Vec<Range<usize>> {
    pages_of(text, markdown, PAGE_BYTES)
}

fn pages_of(text: &str, markdown: bool, size: usize) -> Vec<Range<usize>> {
    let force = size.saturating_mul(FORCE_BYTES / PAGE_BYTES).max(size);
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    let mut open: Option<(u8, usize)> = None;
    for line in text.split_inclusive('\n') {
        let end = at + line.len();
        if markdown {
            match (open, fence(line)) {
                (None, Some(f)) => open = Some(f),
                (Some((c, n)), Some((c2, n2))) if c == c2 && n2 >= n => open = None,
                _ => {}
            }
        }
        // A page the line alone would take past the forced size is cut
        // inside the line, at a character boundary, as often as needed.
        while end - start > force {
            let mut cut = start + force;
            while cut < end && !text.is_char_boundary(cut) {
                cut += 1;
            }
            out.push(start..cut);
            start = cut;
        }
        let full = end - start >= size;
        let boundary = !markdown || (open.is_none() && line.trim().is_empty());
        if (full && boundary) || end - start >= force {
            out.push(start..end);
            start = end;
        }
        at = end;
    }
    if start < text.len() || out.is_empty() {
        out.push(start..text.len());
    }
    out
}

/// Where block `n` of the whole structure (counted over every part's
/// blocks, in order) is: its part and its place in that part.
pub(super) fn locate(starts: &[usize], n: usize) -> Option<(usize, usize)> {
    // `starts[i]` is part i's first block's number.
    let part = starts.partition_point(|s| *s <= n).checked_sub(1)?;
    Some((part, n - starts[part]))
}

/// Each part's first block's number, and the blocks in all.
pub(super) fn block_starts(parts: &[DocPart]) -> (Vec<usize>, usize) {
    let mut starts = Vec::with_capacity(parts.len());
    let mut n = 0;
    for p in parts {
        starts.push(n);
        n += p.blocks.len();
    }
    (starts, n)
}

/// The pages `total` items take at `per` a page (at least one).
pub(super) fn page_count(total: usize, per: usize) -> usize {
    total.div_ceil(per.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pages_cover_the_text_in_order_and_cut_at_blank_lines() {
        let para = "word ".repeat(10) + "\n\n";
        let text = para.repeat(100);
        let p = pages_of(&text, true, 200);
        assert!(p.len() > 5);
        assert_eq!(p.first().unwrap().start, 0);
        assert_eq!(p.last().unwrap().end, text.len());
        for w in p.windows(2) {
            assert_eq!(w[0].end, w[1].start, "no gap, no overlap");
            assert!(
                text[..w[0].end].ends_with("\n\n"),
                "a blank line ends a page"
            );
        }
    }

    #[test]
    fn a_fence_is_never_cut_until_it_is_forced() {
        let mut text = String::from("intro\n\n```\n");
        for _ in 0..50 {
            text.push_str("code line\n\n");
        }
        text.push_str("```\n\nafter\n");
        let p = pages_of(&text, true, 100);
        // The fence's blank lines are no boundary: the first page ends at
        // the forced size or after the fence, never at a blank line in it.
        let first = &text[p[0].clone()];
        assert!(first.len() >= 100);
        assert!(
            first.len() >= 400 || first.ends_with("```\n\n"),
            "{first:?}"
        );
        assert_eq!(p.last().unwrap().end, text.len());
    }

    #[test]
    fn raw_text_cuts_at_any_line_and_a_line_never_splits() {
        let text = "a\n".repeat(1000);
        let p = pages_of(&text, false, 100);
        assert_eq!(p.len(), 20);
        assert!(p.iter().all(|r| text[..r.end].ends_with('\n')));
        assert_eq!(pages_of("", false, 100), vec![0..0]);
        assert_eq!(pages_of("no newline", true, 100), vec![0..10]);
    }

    #[test]
    fn a_block_is_found_by_its_number_across_parts() {
        let starts = [0, 3, 3, 10];
        assert_eq!(locate(&starts, 0), Some((0, 0)));
        assert_eq!(locate(&starts, 2), Some((0, 2)));
        assert_eq!(locate(&starts, 3), Some((2, 0)), "an empty part is skipped");
        assert_eq!(locate(&starts, 12), Some((3, 2)));
        assert_eq!(locate(&[], 0), None);
        assert_eq!(page_count(0, 200), 1);
        assert_eq!(page_count(401, 200), 3);
    }

    #[test]
    fn one_very_long_line_is_cut_inside_at_character_boundaries() {
        // The structure's compact JSON: no line end for megabytes.
        let text = "é{\"k\":1},".repeat(300_000);
        assert!(!text.contains('\n'));
        for markdown in [false, true] {
            let p = pages_of(&text, markdown, 1000);
            assert!(p.len() > 100, "{}", p.len());
            assert_eq!(p.first().unwrap().start, 0);
            assert_eq!(p.last().unwrap().end, text.len());
            for w in p.windows(2) {
                assert_eq!(w[0].end, w[1].start);
                assert!(text.is_char_boundary(w[0].end));
                assert!(w[0].len() <= 4000 + 3, "a page stays near the forced size");
            }
        }
    }

    #[test]
    fn a_million_lines_page_in_one_pass() {
        let text = "line of text\n".repeat(1_000_000);
        let p = pages(&text, true);
        assert_eq!(p.last().unwrap().end, text.len());
    }
}
