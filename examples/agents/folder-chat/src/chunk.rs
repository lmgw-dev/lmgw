//! Deterministic chunking — ours, not a library's, so a re-sync of an
//! unchanged file always produces the same chunks (and quickdoc's
//! content-derived chunk ids stay stable).
//!
//! - **Markdown**: split on ATX headings (`#` … `######`, outside fenced code
//!   blocks); each chunk carries its heading path, `# A > ## B`. A section
//!   longer than the chunk size is split at paragraph boundaries. Setext
//!   headings (`Title` over `===`) are not recognised: `---` is also a rule and
//!   YAML front matter, and guessing between them is not deterministic in any
//!   useful sense.
//! - **Code and plain text**: split at blank-line blocks.
//! - **PDF**: per page (pdftotext's form feeds), heading `page N`.
//!
//! Whatever the kind, a piece that is still too large falls back to line
//! boundaries, and a single line longer than the chunk size (minified code, a
//! CSV with no newlines) is split inside the line — at whitespace where there
//! is some. **Nothing is ever dropped for size**: a huge file is many chunks.
//!
//! **Cost.** Every search for "the longest piece that fits" gallops (1, 2, 4,
//! … units or bytes) and then bisects, and never counts more than about twice
//! the piece it settles on, so cutting a file of `n` bytes into chunks of
//! about `m` bytes counts O(n log m) bytes of text, whatever its shape: one
//! multi-megabyte line, a million one-character lines, or prose. Lines and
//! paragraphs are walked lazily, never collected for a whole file. (This
//! relies on the counter never giving a longer span fewer tokens than a span
//! it contains, which the estimator's characters-divided-by-four does; with
//! such a counter the cuts are exactly those of a greedy left-to-right scan.)
//! The chunker runs in `spawn_blocking` (see `sync`), off the async workers.
//!
//! Spans are **byte offsets** into the text the chunk was cut from — the
//! file's own UTF-8 for text kinds, pdftotext's output for a PDF — and every
//! payload is the verbatim slice `text[start..end]`. [`line_range`] turns a
//! span into the line numbers a UI shows.

use std::sync::Arc;

use quickdoc_core::embed::{ApproxTokenCounter, TokenCounter};
use serde::Serialize;

/// Bumped whenever the chunking rules change; a different value in an existing
/// index re-chunks the folder (see `sync`), because chunks cut by two rule
/// sets side by side would rank inconsistently.
pub const CHUNKER_VERSION: &str = "1";

/// Page separator in `pdftotext` output.
pub const FORM_FEED: char = '\u{0c}';

/// The token estimator every size in this crate is measured with: quickdoc's
/// [`ApproxTokenCounter`] — characters divided by 4. It is an **estimate**
/// (a few percent off on English prose, more on code), which is why its name
/// is carried in the sync report and in every answer's budget. The same one
/// measures chunks and the chat budget, so the two agree with each other.
pub fn estimator() -> Arc<dyn TokenCounter> {
    Arc::new(ApproxTokenCounter::default())
}

/// One chunk: a byte range of the source text and where it sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChunkSpan {
    /// `# A > ## B` for markdown, `page N` for a PDF, empty otherwise.
    pub heading_path: String,
    pub start: usize,
    pub end: usize,
}

impl ChunkSpan {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// 1-based, inclusive line numbers of a byte span. Counts from the top of
/// `text`: for many spans of one text use [`line_ranges`], which is one pass.
pub fn line_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let start = start.min(text.len());
    let end = end.clamp(start, text.len());
    let first = text[..start].matches('\n').count() + 1;
    let body = &text[start..end];
    let last = first + body.trim_end_matches('\n').matches('\n').count();
    (first, last)
}

/// [`line_range`] of every span, in the spans' order, in one pass over `text`
/// — what the sync stores with each chunk (`folder_chat_chunk_lines`).
pub fn line_ranges(text: &str, spans: &[ChunkSpan]) -> Vec<(usize, usize)> {
    let newlines = |b: &[u8]| b.iter().filter(|c| **c == b'\n').count();
    let bytes = text.as_bytes();
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i].start);
    let mut out = vec![(0, 0); spans.len()];
    let (mut pos, mut line) = (0usize, 1usize);
    for i in order {
        let start = spans[i].start.min(text.len());
        let end = spans[i].end.clamp(start, text.len());
        if start >= pos {
            line += newlines(&bytes[pos..start]);
        } else {
            // Overlapping spans (the chunker never makes them): recount.
            line = 1 + newlines(&bytes[..start]);
        }
        pos = start;
        let body = text[start..end].trim_end_matches('\n');
        out[i] = (line, line + newlines(body.as_bytes()));
    }
    out
}

pub fn chunk_plain(text: &str, max_tokens: usize, tc: &dyn TokenCounter) -> Vec<ChunkSpan> {
    let mut out = Vec::new();
    split_region(text, 0, text.len(), max_tokens, tc, &mut out);
    out.into_iter()
        .map(|(start, end)| ChunkSpan {
            heading_path: String::new(),
            start,
            end,
        })
        .collect()
}

/// `text` is pdftotext's whole output; pages are separated by form feeds.
pub fn chunk_pdf(text: &str, max_tokens: usize, tc: &dyn TokenCounter) -> Vec<ChunkSpan> {
    let mut out = Vec::new();
    let mut page_start = 0usize;
    for (n, page) in text.split(FORM_FEED).enumerate() {
        let mut spans = Vec::new();
        split_region(
            text,
            page_start,
            page_start + page.len(),
            max_tokens,
            tc,
            &mut spans,
        );
        out.extend(spans.into_iter().map(|(start, end)| ChunkSpan {
            heading_path: format!("page {}", n + 1),
            start,
            end,
        }));
        page_start += page.len() + FORM_FEED.len_utf8();
    }
    out
}

/// How many pages pdftotext's output has, and the ones with no text on them —
/// numbered as [`chunk_pdf`] numbers them, and textless exactly when
/// [`chunk_pdf`] cuts nothing from them (a scanned page without an OCR layer).
/// pdftotext ends every page with a form feed, so a trailing feed closes the
/// last page rather than opening another.
pub fn pdf_pages(text: &str) -> (u32, Vec<u32>) {
    let body = text.strip_suffix(FORM_FEED).unwrap_or(text);
    let mut pages = 0u32;
    let mut textless = Vec::new();
    for page in body.split(FORM_FEED) {
        pages += 1;
        if page.trim().is_empty() {
            textless.push(pages);
        }
    }
    (pages, textless)
}

pub fn chunk_markdown(text: &str, max_tokens: usize, tc: &dyn TokenCounter) -> Vec<ChunkSpan> {
    struct Section {
        path: String,
        level: usize,
        start: usize,
        end: usize,
        /// Where the body starts: after the heading line, or `start` for the
        /// preamble before the first heading.
        body: usize,
    }
    let mut sections: Vec<Section> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut cur = Section {
        path: String::new(),
        level: 0,
        start: 0,
        end: 0,
        body: 0,
    };
    let mut fence: Option<(char, usize)> = None;
    for (ls, le) in line_spans(text, 0, text.len()) {
        let line = &text[ls..le];
        if let Some((ch, n)) = fence {
            if closes_fence(line, ch, n) {
                fence = None;
            }
            continue;
        }
        if let Some(f) = opens_fence(line) {
            fence = Some(f);
            continue;
        }
        if let Some((level, title)) = atx_heading(line) {
            cur.end = ls;
            sections.push(cur);
            stack.retain(|(l, _)| *l < level);
            stack.push((level, format!("{} {}", "#".repeat(level), title)));
            cur = Section {
                path: stack
                    .iter()
                    .map(|(_, h)| h.as_str())
                    .collect::<Vec<_>>()
                    .join(" > "),
                level,
                start: ls,
                end: 0,
                body: next_line_start(text, le),
            };
        }
    }
    cur.end = text.len();
    sections.push(cur);

    let mut out = Vec::new();
    for (i, s) in sections.iter().enumerate() {
        let body_blank = text[s.body.min(s.end)..s.end].trim().is_empty();
        // A heading with nothing under it before a deeper heading is carried
        // by that child's path already; as a chunk of its own it would only be
        // a one-line match that wastes an excerpt slot. A heading-only section
        // that nothing carries (a sibling follows, or it is last) is kept.
        if s.level > 0 && body_blank && sections.get(i + 1).is_some_and(|n| n.level > s.level) {
            continue;
        }
        let mut spans = Vec::new();
        split_region(text, s.start, s.end, max_tokens, tc, &mut spans);
        out.extend(spans.into_iter().map(|(start, end)| ChunkSpan {
            heading_path: s.path.clone(),
            start,
            end,
        }));
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown line classification
// ---------------------------------------------------------------------------

/// Up to three spaces of indentation, as CommonMark allows; four is an
/// indented code block and not a heading or fence.
fn dedent(line: &str) -> Option<&str> {
    let n = line.len() - line.trim_start_matches(' ').len();
    (n <= 3).then(|| &line[n..])
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    let l = dedent(line)?;
    let level = l.len() - l.trim_start_matches('#').len();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &l[level..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None; // `#hashtag`, not a heading
    }
    // An optional closing run of `#`s is decoration, not title.
    let title = rest.trim();
    let stripped = title.trim_end_matches('#');
    let title = if stripped.is_empty() || stripped.ends_with(' ') || stripped.ends_with('\t') {
        stripped.trim_end()
    } else {
        title
    };
    Some((level, title.to_string()))
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    let l = dedent(line)?;
    let ch = l.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = l.len() - l.trim_start_matches(ch).len();
    (n >= 3).then_some((ch, n))
}

fn closes_fence(line: &str, ch: char, n: usize) -> bool {
    let Some(l) = dedent(line) else {
        return false;
    };
    let run = l.len() - l.trim_start_matches(ch).len();
    run >= n && l[run..].trim().is_empty()
}

// ---------------------------------------------------------------------------
// The size-driven splitter shared by every kind
// ---------------------------------------------------------------------------

/// `(start, end)` of each line in `[start, end)`, without its `\n` (or
/// `\r\n`), lazily.
fn line_spans(text: &str, start: usize, end: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut s = start;
    std::iter::from_fn(move || {
        if s >= end {
            return None;
        }
        let e = text[s..end].find('\n').map_or(end, |i| s + i);
        let content_end = if e > s && text.as_bytes()[e - 1] == b'\r' {
            e - 1
        } else {
            e
        };
        let line = (s, content_end);
        s = e + 1;
        Some(line)
    })
}

fn next_line_start(text: &str, line_end: usize) -> usize {
    text[line_end..]
        .find('\n')
        .map_or(text.len(), |i| line_end + i + 1)
}

/// Drop blank lines at the front and whitespace at the back. Leading spaces on
/// the first kept line stay: indentation is part of code.
fn trim_region(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let first = line_spans(text, start, end).find(|(s, e)| !text[*s..*e].trim().is_empty())?;
    let e = start + text[start..end].trim_end().len();
    (e > first.0).then_some((first.0, e))
}

/// Maximal runs of non-blank lines, lazily.
fn paragraphs(text: &str, start: usize, end: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut lines = line_spans(text, start, end);
    std::iter::from_fn(move || {
        let mut cur: Option<(usize, usize)> = None;
        for (s, e) in lines.by_ref() {
            if text[s..e].trim().is_empty() {
                if cur.is_some() {
                    return cur;
                }
            } else {
                cur = Some((cur.map_or(s, |c| c.0), e));
            }
        }
        cur
    })
}

/// Cut `[start, end)` into spans of at most `max` tokens: whole if it fits,
/// else at paragraphs, else at lines, else inside the line.
fn split_region(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    tc: &dyn TokenCounter,
    out: &mut Vec<(usize, usize)>,
) {
    let Some((s, e)) = trim_region(text, start, end) else {
        return;
    };
    if tc.count(&text[s..e]) <= max {
        out.push((s, e));
        return;
    }
    let mut paras = paragraphs(text, s, e).peekable();
    let first = paras.next();
    if paras.peek().is_some() {
        pack(text, first.into_iter().chain(paras), max, tc, out);
        return;
    }
    let mut lines = line_spans(text, s, e)
        .filter(|(a, b)| !text[*a..*b].trim().is_empty())
        .peekable();
    let first = lines.next();
    if lines.peek().is_some() {
        pack(text, first.into_iter().chain(lines), max, tc, out);
        return;
    }
    hard_split(text, s, e, max, tc, out);
}

/// Join adjacent units into the longest runs that fit; a unit that does not
/// fit on its own goes back through [`split_region`] one level down.
///
/// The run starting at the front unit is found by galloping over how many
/// units it takes (1, 2, 4, …) and then bisecting, so a chunk of `k` units
/// costs O(log k) counts of at most about twice its own length — not one
/// count per unit of the whole run so far, which made a file of many short
/// lines cost its length times the chunk size. Units are pulled from the
/// iterator only as the gallop reaches them.
fn pack(
    text: &str,
    units: impl Iterator<Item = (usize, usize)>,
    max: usize,
    tc: &dyn TokenCounter,
    out: &mut Vec<(usize, usize)>,
) {
    use std::collections::VecDeque;
    let mut units = units.fuse();
    let mut buf: VecDeque<(usize, usize)> = VecDeque::new();
    // Make `buf[k]` exist if the iterator still has it.
    fn fill(
        buf: &mut VecDeque<(usize, usize)>,
        units: &mut impl Iterator<Item = (usize, usize)>,
        k: usize,
    ) -> bool {
        while buf.len() <= k {
            match units.next() {
                Some(u) => buf.push_back(u),
                None => return false,
            }
        }
        true
    }
    loop {
        if !fill(&mut buf, &mut units, 0) {
            return;
        }
        let (us, ue) = buf[0];
        if tc.count(&text[us..ue]) > max {
            buf.pop_front();
            split_region(text, us, ue, max, tc, out);
            continue;
        }
        let fits = |buf: &VecDeque<(usize, usize)>, k: usize| tc.count(&text[us..buf[k].1]) <= max;
        // `good`: the longest run known to fit; `bad`: the shortest known not
        // to. A run through a unit that does not fit alone cannot fit either.
        let mut good = 0usize;
        let mut bad = None;
        let mut step = 1usize;
        loop {
            if !fill(&mut buf, &mut units, step) {
                let last = buf.len() - 1;
                if last > good {
                    if fits(&buf, last) {
                        good = last;
                    } else {
                        bad = Some(last);
                    }
                }
                break;
            }
            if fits(&buf, step) {
                good = step;
                step *= 2;
            } else {
                bad = Some(step);
                break;
            }
        }
        if let Some(mut bad) = bad {
            while bad > good + 1 {
                let mid = good + (bad - good) / 2;
                if fits(&buf, mid) {
                    good = mid;
                } else {
                    bad = mid;
                }
            }
        }
        out.push((us, buf[good].1));
        buf.drain(..=good);
    }
}

/// Split one oversized line. Gallop (1, 2, 4, … bytes) and then bisect for
/// the longest prefix that fits; the rest of the line is counted only once
/// the gallop has reached its end, when it is at most about twice a prefix
/// that fitted — so each chunk costs O(m log m) for a chunk of `m` bytes and
/// the whole line O(n log m), never a recount of the remainder per chunk.
/// Then back off to the last whitespace in the window when there is one in
/// its second half, so words are not cut where it can be helped.
fn hard_split(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    tc: &dyn TokenCounter,
    out: &mut Vec<(usize, usize)>,
) {
    let fits = |a: usize, b: usize| tc.count(&text[a..b]) <= max;
    let boundary = |mut i: usize| {
        while i < end && !text.is_char_boundary(i) {
            i += 1;
        }
        i.min(end)
    };
    let mut s = start;
    while s < end {
        // At least one character per chunk, whatever the estimator says.
        let one = boundary(s + 1);
        let mut good = one;
        let mut step = 1usize;
        let bad;
        loop {
            let probe = boundary(s + step);
            if probe >= end {
                if fits(s, end) {
                    out.push((s, end));
                    return;
                }
                bad = end;
                break;
            }
            if fits(s, probe) {
                good = good.max(probe);
                step *= 2;
            } else {
                bad = probe;
                break;
            }
        }
        let mut bad = bad;
        while bad > good + 1 {
            let mid = boundary(good + (bad - good) / 2);
            if mid >= bad {
                break;
            }
            if fits(s, mid) {
                good = mid;
            } else {
                bad = mid;
            }
        }
        let mut cut = good;
        if let Some(ws) = text[s..good].rfind(char::is_whitespace) {
            let ws = s + ws;
            if ws > s + (good - s) / 2 {
                cut = ws + text[ws..].chars().next().map_or(1, char::len_utf8);
            }
        }
        let piece_end = s + text[s..cut].trim_end().len();
        if piece_end > s {
            out.push((s, piece_end));
        }
        s = cut;
        // Whitespace at the cut belongs to neither side.
        while s < end && text[s..].starts_with(char::is_whitespace) {
            s += text[s..].chars().next().map_or(1, char::len_utf8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc() -> Arc<dyn TokenCounter> {
        estimator()
    }

    fn texts<'a>(src: &'a str, spans: &[ChunkSpan]) -> Vec<&'a str> {
        spans.iter().map(|s| s.text(src)).collect()
    }

    #[test]
    fn markdown_carries_the_heading_path() {
        let md = "intro line\n\n# Setup\n\nInstall it.\n\n## Podman\n\nUse :Z.\n\n```sh\n# not a heading\n```\n\n## Docker\n\nNo.\n\n# Usage\n\nRun.\n";
        let spans = chunk_markdown(md, 400, tc().as_ref());
        let paths: Vec<&str> = spans.iter().map(|s| s.heading_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "",
                "# Setup",
                "# Setup > ## Podman",
                "# Setup > ## Docker",
                "# Usage"
            ]
        );
        let t = texts(md, &spans);
        assert_eq!(t[0], "intro line");
        assert!(t[1].starts_with("# Setup") && t[1].ends_with("Install it."));
        assert!(
            t[2].contains("# not a heading"),
            "a fenced line is content: {t:?}"
        );
    }

    #[test]
    fn a_heading_only_parent_is_carried_by_its_child() {
        let md = "# Book\n## Chapter\n\ntext\n";
        let spans = chunk_markdown(md, 400, tc().as_ref());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].heading_path, "# Book > ## Chapter");
    }

    #[test]
    fn long_sections_split_at_paragraphs_and_every_chunk_fits() {
        let para = "word ".repeat(60); // ~75 tokens each
        let md = format!("# Big\n\n{para}\n\n{para}\n\n{para}\n\n{para}\n");
        let spans = chunk_markdown(&md, 100, tc().as_ref());
        assert!(spans.len() >= 3, "{spans:?}");
        for s in &spans {
            assert_eq!(s.heading_path, "# Big");
            assert!(tc().count(s.text(&md)) <= 100);
        }
        // Deterministic: the same input cuts the same way.
        assert_eq!(spans, chunk_markdown(&md, 100, tc().as_ref()));
    }

    #[test]
    fn one_giant_line_is_split_not_dropped() {
        let line = "abcdefghij".repeat(10_000); // 100k chars, no whitespace
        let spans = chunk_plain(&line, 50, tc().as_ref());
        let rebuilt: String = texts(&line, &spans).concat();
        assert_eq!(rebuilt, line, "nothing is lost");
        assert!(spans.iter().all(|s| tc().count(s.text(&line)) <= 50));

        let words = "lorem ipsum dolor ".repeat(500);
        for s in chunk_plain(&words, 50, tc().as_ref()) {
            let t = s.text(&words);
            assert!(!t.starts_with(' ') && !t.ends_with(' '), "{t:?}");
        }
    }

    #[test]
    fn code_splits_at_blank_lines_and_keeps_indentation() {
        let code = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let spans = chunk_plain(code, 5, tc().as_ref());
        let t = texts(code, &spans);
        assert!(t.contains(&"fn a() {\n    1\n}"), "{t:?}");
        let spans = chunk_plain("\n\n    indented\n", 400, tc().as_ref());
        assert_eq!(spans[0].start, 2, "blank lines go, indentation stays");
    }

    #[test]
    fn pdf_pages_become_headings_with_offsets_into_the_output() {
        let out = "first page\n\u{0c}\u{0c}third page text\n\u{0c}";
        let spans = chunk_pdf(out, 400, tc().as_ref());
        assert_eq!(
            spans.len(),
            2,
            "the empty page and the trailing feed give nothing"
        );
        assert_eq!(spans[0].heading_path, "page 1");
        assert_eq!(spans[1].heading_path, "page 3");
        assert_eq!(spans[1].text(out), "third page text");
        assert_eq!(
            pdf_pages(out),
            (3, vec![2]),
            "the trailing feed closes page 3"
        );
        assert_eq!(pdf_pages("a\u{0c}b"), (2, vec![]), "no trailing feed");
        assert_eq!(pdf_pages(" \n\u{0c}"), (1, vec![1]));
    }

    #[test]
    fn line_ranges_single_and_batched() {
        let t = "a\nb\nc\nd\n";
        assert_eq!(line_range(t, 2, 5), (2, 3));
        assert_eq!(line_range(t, 0, 1), (1, 1));
        assert_eq!(line_range(t, 0, t.len()), (1, 4));

        let md = "intro\n\n# A\n\none\ntwo\n\n## B\n\nthree\n\n\nfour\n";
        for max in [2, 5, 400] {
            let spans = chunk_markdown(md, max, tc().as_ref());
            let batched = line_ranges(md, &spans);
            let single: Vec<_> = spans
                .iter()
                .map(|s| line_range(md, s.start, s.end))
                .collect();
            assert_eq!(batched, single, "max {max}");
        }
    }

    /// A counter that adds up how many bytes it was asked to count — the
    /// chunker's real cost, independent of the machine.
    struct Metered {
        inner: ApproxTokenCounter,
        bytes: std::sync::atomic::AtomicUsize,
    }

    impl TokenCounter for Metered {
        fn name(&self) -> String {
            self.inner.name()
        }
        fn count(&self, text: &str) -> usize {
            self.bytes
                .fetch_add(text.len(), std::sync::atomic::Ordering::Relaxed);
            self.inner.count(text)
        }
    }

    fn metered() -> Metered {
        Metered {
            inner: ApproxTokenCounter::default(),
            bytes: Default::default(),
        }
    }

    #[test]
    fn a_multi_megabyte_line_costs_n_log_m_not_n_squared() {
        // 4 MiB, no whitespace, no newline: the old splitter recounted the
        // whole remainder for every chunk, ~n²/(2·m) ≈ 2·10¹⁰ bytes here.
        let line = "abcdefghij".repeat(400_000);
        let n = line.len();
        let tc = metered();
        let spans = chunk_plain(&line, 100, &tc);
        let counted = tc.bytes.load(std::sync::atomic::Ordering::Relaxed);
        let rebuilt: String = texts(&line, &spans).concat();
        assert_eq!(rebuilt, line, "nothing is lost");
        // m ≈ 400 bytes, log2(m) ≈ 9: the gallop and the bisection count a
        // few multiples of that per byte. 64× is ample and still ~300× below
        // the quadratic cost.
        assert!(counted <= 64 * n, "counted {counted} bytes for {n}");
    }

    #[test]
    fn many_short_lines_cost_n_log_m_too() {
        // One paragraph of 400k two-byte lines: the old packer recounted the
        // growing chunk for every line (~chunk length × lines).
        let text = "x\n".repeat(400_000);
        let n = text.len();
        let tc = metered();
        let spans = chunk_plain(&text, 100, &tc);
        let counted = tc.bytes.load(std::sync::atomic::Ordering::Relaxed);
        assert!(spans
            .iter()
            .all(|s| estimator().count(s.text(&text)) <= 100));
        assert!(counted <= 64 * n, "counted {counted} bytes for {n}");
    }

    /// The galloping packer cuts exactly where a greedy left-to-right scan
    /// does (the old implementation, kept here as the reference).
    #[test]
    fn galloping_cuts_where_the_greedy_scan_cuts() {
        fn greedy(
            text: &str,
            units: &[(usize, usize)],
            max: usize,
            tc: &dyn TokenCounter,
        ) -> Vec<(usize, usize)> {
            let mut out = Vec::new();
            let mut cur: Option<(usize, usize)> = None;
            for &(us, ue) in units {
                if tc.count(&text[us..ue]) > max {
                    out.extend(cur.take());
                    out.push((us, ue)); // marker: split separately
                    continue;
                }
                cur = match cur {
                    None => Some((us, ue)),
                    Some((cs, _)) if tc.count(&text[cs..ue]) <= max => Some((cs, ue)),
                    Some(c) => {
                        out.push(c);
                        Some((us, ue))
                    }
                };
            }
            out.extend(cur);
            out
        }
        let mut text = String::new();
        let mut units = Vec::new();
        // Lines of varied length, some longer than the chunk on their own.
        for i in 0..500usize {
            let len = (i * 37) % 23 + if i % 97 == 0 { 90 } else { 1 };
            let s = text.len();
            text.push_str(&"w".repeat(len));
            units.push((s, text.len()));
            text.push('\n');
        }
        let tc = estimator();
        for max in [3, 7, 20] {
            let mut got = Vec::new();
            pack(&text, units.iter().copied(), max, tc.as_ref(), &mut got);
            let want = greedy(&text, &units, max, tc.as_ref());
            // Where the greedy scan marks an oversized unit, the packer has
            // split it further: compare the runs of whole units.
            let fits = |v: &Vec<(usize, usize)>| -> Vec<(usize, usize)> {
                v.iter()
                    .copied()
                    .filter(|(a, b)| {
                        tc.count(&text[*a..*b]) <= max
                            && units.iter().any(|u| u.0 == *a)
                            && units.iter().any(|u| u.1 == *b)
                    })
                    .collect()
            };
            assert_eq!(fits(&got), fits(&want), "max {max}");
        }
    }

    #[test]
    fn atx_edge_cases() {
        assert_eq!(atx_heading("## Title ##"), Some((2, "Title".into())));
        assert_eq!(atx_heading("#hashtag"), None);
        assert_eq!(atx_heading("    # code"), None);
        assert_eq!(atx_heading("####### seven"), None);
        assert_eq!(atx_heading("# C#"), Some((1, "C#".into())));
    }
}
