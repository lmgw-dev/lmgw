//! The deterministic chunker (chat-complete design §9.2). quickdoc's own
//! chunking is an LLM tool loop and is not reused: a knowledge base is the
//! owner's documents, chunked the same way every time, with no model in the
//! loop and nothing to pay for.
//!
//! **Input** is a file's [`Section`]s — a PDF page, a slide, a sheet, or the
//! whole of a text or word-processing file. A chunk never crosses a section,
//! so a citation always names one page.
//!
//! **Markdown-aware** inside a section:
//! - ATX headings start a new chunk and build the heading path
//!   (`Sheet: Budget > Income`), which is embedded and shown with the chunk;
//! - paragraphs pack line by line, a line too long for a chunk by sentences,
//!   then words, then characters;
//! - a fenced code block stays whole when it fits a chunk; split, every piece
//!   is re-opened and closed with the block's own fence;
//! - a table stays whole when it fits; split, every piece after the first
//!   repeats the header row, so each one still says what its columns are.
//!
//! **Sizes** are the base's `chunk_tokens` / `chunk_overlap`, counted with the
//! gateway's tiktoken counter over what is embedded: the heading path and the
//! payload. Overlap carries whole trailing lines (or sentences) of a chunk into
//! the next one when a size limit — not a heading or a section — ended it.
//!
//! **Spans** are byte offsets into the file's assembled text ([`assemble`]),
//! which is what the source viewer shows; the payload is that region verbatim,
//! plus a repeated table header or a re-opened fence where one was split.

use quickdoc_core::embed::TokenCounter;

/// One part of a file that no chunk crosses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// 1-based PDF page.
    pub page: Option<u32>,
    /// The part's own heading (`Slide 3`, `Sheet: Budget`): the root of every
    /// heading path inside it.
    pub heading: Option<String>,
    /// Where [`Self::text`] starts in the assembled file text.
    pub offset: usize,
    pub text: String,
}

/// A file's text as the source viewer shows it, and the sections the chunks
/// point into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    pub text: String,
    pub sections: Vec<Section>,
}

/// One part before assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub page: Option<u32>,
    pub heading: Option<String>,
    pub body: String,
}

/// Lay the parts out as one text — a page under `--- page N ---`, a headed
/// part under `## heading` (the shapes the Chat's `<file>` block and the
/// office extractor already use) — and remember where each body starts.
pub fn assemble(parts: Vec<Part>) -> Assembled {
    let mut text = String::new();
    let mut sections = Vec::with_capacity(parts.len());
    for p in parts {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        if let Some(n) = p.page {
            text.push_str(&format!("--- page {n} ---\n"));
        } else if let Some(h) = &p.heading {
            text.push_str(&format!("## {h}\n\n"));
        }
        let body = p.body.trim_end().to_string();
        let offset = text.len();
        text.push_str(&body);
        sections.push(Section {
            page: p.page,
            heading: p.heading,
            offset,
            text: body,
        });
    }
    Assembled { text, sections }
}

/// A base's chunk settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub chunk_tokens: usize,
    pub overlap: usize,
}

/// One chunk, before it has an id or a vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub page: Option<u32>,
    pub heading_path: String,
    /// Byte range in the assembled text.
    pub span: (usize, usize),
    pub payload: String,
    /// Counted over [`embed_text`] of this chunk.
    pub tokens: usize,
}

/// What is embedded for a chunk, and what `chunk_tokens` bounds: the heading
/// path over the payload.
pub fn embed_text(heading_path: &str, payload: &str) -> String {
    if heading_path.is_empty() {
        payload.to_string()
    } else {
        format!("{heading_path}\n{payload}")
    }
}

/// Chunk every section of a file, in reading order.
pub fn chunk(sections: &[Section], sizes: Sizes, counter: &dyn TokenCounter) -> Vec<Draft> {
    chunk_until(sections, sizes, counter, &|| false).unwrap_or_default()
}

/// [`chunk`], interruptible: `stop` is looked at between sections, between
/// lines and between the pieces a long line is cut into, and a `true` ends
/// the work with `None`. The ingest job runs this on a blocking thread and
/// hands it the job's cancel flag, so Cancel lands within a piece's work
/// instead of after the whole file.
pub fn chunk_until(
    sections: &[Section],
    sizes: Sizes,
    counter: &dyn TokenCounter,
    stop: &dyn Fn() -> bool,
) -> Option<Vec<Draft>> {
    let mut out = Vec::new();
    for s in sections {
        if stop() {
            return None;
        }
        let atoms = atomize(s, sizes, counter, stop);
        if stop() {
            return None;
        }
        pack(s, &atoms, sizes, counter, &mut out);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Atoms: the smallest pieces the packer moves
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An ATX heading line.
    Heading,
    /// A line (or a sentence/word piece of one) of a paragraph.
    Prose,
    /// A table or code block that fits a chunk on its own: never split.
    Whole,
    /// The header and separator lines of a split table.
    TableHeader,
    TableRow,
    /// The opening fence line of a split code block.
    CodeOpen,
    CodeLine,
    CodeClose,
}

#[derive(Debug, Clone)]
struct Atom {
    /// Byte range in the section's text.
    start: usize,
    end: usize,
    tokens: usize,
    kind: Kind,
    /// Index into the section's groups, for a split table or code block.
    group: Option<usize>,
    heading_path: String,
}

enum Group {
    Table { header: String },
    Code { open: String, close: String },
}

struct Atoms {
    list: Vec<Atom>,
    groups: Vec<Group>,
}

struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        out.push(Line {
            start: pos,
            end: pos + body.len(),
            text: body,
        });
        pos += raw.len();
    }
    out
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((hashes, rest.trim().trim_end_matches('#').trim()))
}

/// The fence a line opens: its character and length.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

fn fence_closes(line: &str, c: char, n: usize) -> bool {
    let t = line.trim();
    t.chars().take_while(|x| *x == c).count() >= n && t.chars().all(|x| x == c)
}

fn is_table_line(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn path_of(root: &Option<String>, stack: &[(usize, String)]) -> String {
    root.iter()
        .cloned()
        .chain(stack.iter().map(|(_, h)| h.clone()))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" > ")
}

fn atomize(
    s: &Section,
    sizes: Sizes,
    counter: &dyn TokenCounter,
    stop: &dyn Fn() -> bool,
) -> Atoms {
    let text = s.text.as_str();
    let ls = lines(text);
    let mut list: Vec<Atom> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut path = path_of(&s.heading, &stack);
    // Room for content in a chunk once its heading path is embedded with it.
    let room = |path: &str| {
        let h = if path.is_empty() {
            0
        } else {
            counter.count(path) + 1
        };
        sizes.chunk_tokens.saturating_sub(h).max(1)
    };

    let mut i = 0;
    while i < ls.len() {
        if stop() {
            break;
        }
        let l = &ls[i];
        if l.text.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some((level, title)) = heading_level(l.text) {
            stack.retain(|(lv, _)| *lv < level);
            stack.push((level, title.to_string()));
            path = path_of(&s.heading, &stack);
            list.push(Atom {
                start: l.start,
                end: l.end,
                tokens: counter.count(l.text),
                kind: Kind::Heading,
                group: None,
                heading_path: path.clone(),
            });
            i += 1;
            continue;
        }
        if let Some((c, n)) = fence_open(l.text) {
            let mut j = i + 1;
            while j < ls.len() && !fence_closes(ls[j].text, c, n) {
                j += 1;
            }
            let closed = j < ls.len();
            let last = if closed { j } else { ls.len() - 1 };
            let (start, end) = (l.start, ls[last].end);
            let tokens = counter.count(&text[start..end]);
            if tokens <= room(&path) {
                list.push(Atom {
                    start,
                    end,
                    tokens,
                    kind: Kind::Whole,
                    group: None,
                    heading_path: path.clone(),
                });
            } else {
                let g = groups.len();
                let close = if closed {
                    ls[j].text.trim().to_string()
                } else {
                    std::iter::repeat_n(c, n).collect()
                };
                groups.push(Group::Code {
                    open: l.text.trim().to_string(),
                    close: close.clone(),
                });
                let fence_tokens = counter.count(l.text) + counter.count(&close) + 2;
                list.push(Atom {
                    start: l.start,
                    end: l.end,
                    tokens: counter.count(l.text),
                    kind: Kind::CodeOpen,
                    group: Some(g),
                    heading_path: path.clone(),
                });
                for cl in &ls[i + 1..(if closed { j } else { ls.len() })] {
                    for (a, b) in split_range(
                        text,
                        cl.start,
                        cl.end,
                        room(&path).saturating_sub(fence_tokens).max(1),
                        counter,
                        stop,
                    ) {
                        list.push(Atom {
                            start: a,
                            end: b,
                            tokens: counter.count(&text[a..b]),
                            kind: Kind::CodeLine,
                            group: Some(g),
                            heading_path: path.clone(),
                        });
                    }
                }
                if closed {
                    list.push(Atom {
                        start: ls[j].start,
                        end: ls[j].end,
                        tokens: counter.count(ls[j].text),
                        kind: Kind::CodeClose,
                        group: Some(g),
                        heading_path: path.clone(),
                    });
                }
            }
            i = last + 1;
            continue;
        }
        if is_table_line(l.text) && ls.get(i + 1).is_some_and(|n| is_table_separator(n.text)) {
            let mut j = i + 2;
            while j < ls.len() && is_table_line(ls[j].text) {
                j += 1;
            }
            let (start, end) = (l.start, ls[j - 1].end);
            let tokens = counter.count(&text[start..end]);
            if tokens <= room(&path) {
                list.push(Atom {
                    start,
                    end,
                    tokens,
                    kind: Kind::Whole,
                    group: None,
                    heading_path: path.clone(),
                });
            } else {
                let header_end = ls[i + 1].end;
                let header = text[l.start..header_end].to_string();
                let header_tokens = counter.count(&header);
                let g = groups.len();
                groups.push(Group::Table {
                    header: header.clone(),
                });
                list.push(Atom {
                    start: l.start,
                    end: header_end,
                    tokens: header_tokens,
                    kind: Kind::TableHeader,
                    group: Some(g),
                    heading_path: path.clone(),
                });
                for row in &ls[i + 2..j] {
                    for (a, b) in split_range(
                        text,
                        row.start,
                        row.end,
                        room(&path).saturating_sub(header_tokens + 1).max(1),
                        counter,
                        stop,
                    ) {
                        list.push(Atom {
                            start: a,
                            end: b,
                            tokens: counter.count(&text[a..b]),
                            kind: Kind::TableRow,
                            group: Some(g),
                            heading_path: path.clone(),
                        });
                    }
                }
            }
            i = j;
            continue;
        }
        // A paragraph line: until a blank line or the start of something else.
        for (a, b) in split_range(text, l.start, l.end, room(&path), counter, stop) {
            list.push(Atom {
                start: a,
                end: b,
                tokens: counter.count(&text[a..b]),
                kind: Kind::Prose,
                group: None,
                heading_path: path.clone(),
            });
        }
        i += 1;
    }
    Atoms { list, groups }
}

/// Split `text[start..end]` into pieces of at most `max` tokens: at sentence
/// ends first, then at whitespace, then between characters. Pieces are
/// contiguous and cover the whole range.
fn split_range(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    counter: &dyn TokenCounter,
    stop: &dyn Fn() -> bool,
) -> Vec<(usize, usize)> {
    split_level(text, start, end, max, counter, stop, Level::Sentence)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Sentence,
    Word,
    Char,
}

impl Level {
    fn finer(self) -> Option<Self> {
        match self {
            Self::Sentence => Some(Self::Word),
            Self::Word => Some(Self::Char),
            Self::Char => None,
        }
    }
}

fn split_level(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    counter: &dyn TokenCounter,
    stop: &dyn Fn() -> bool,
    level: Level,
) -> Vec<(usize, usize)> {
    if counter.count(&text[start..end]) <= max {
        return vec![(start, end)];
    }
    let s = &text[start..end];
    let b = s.as_bytes();
    let cuts: Vec<usize> = match level {
        // After `.`, `!` or `?` followed by whitespace.
        Level::Sentence => (0..b.len().saturating_sub(1))
            .filter(|k| matches!(b[*k], b'.' | b'!' | b'?') && b[k + 1].is_ascii_whitespace())
            .map(|k| start + k + 1)
            .collect(),
        Level::Word => s
            .char_indices()
            .filter(|(k, c)| *k > 0 && c.is_whitespace())
            .map(|(k, _)| start + k)
            .collect(),
        Level::Char => s.char_indices().skip(1).map(|(k, _)| start + k).collect(),
    };
    if cuts.is_empty() {
        return match level.finer() {
            Some(l) => split_level(text, start, end, max, counter, stop, l),
            // One character over the budget: it goes out as it is.
            None => vec![(start, end)],
        };
    }
    // Unit boundaries: start, every cut, end.
    let mut bounds = Vec::with_capacity(cuts.len() + 2);
    bounds.push(start);
    bounds.extend(cuts);
    bounds.push(end);

    let mut out = Vec::new();
    let mut p = 0;
    let last = bounds.len() - 1;
    while p + 1 < bounds.len() {
        if stop() {
            // The caller discards a stopped run; the range stays covered.
            out.push((bounds[p], end));
            break;
        }
        let fits = |m: usize| counter.count(&text[bounds[p]..bounds[m]]) <= max;
        if !fits(p + 1) {
            // One unit alone is too big: one level finer, for that unit only.
            let (ua, ub) = (bounds[p], bounds[p + 1]);
            match level.finer() {
                Some(l) => out.extend(split_level(text, ua, ub, max, counter, stop, l)),
                None => out.push((ua, ub)),
            }
            p += 1;
            continue;
        }
        // The furthest boundary whose piece still fits. A token count grows
        // with its text, so the fit is monotone; the search gallops out from
        // the piece's start (1, 2, 4, ... units) before it bisects, so what
        // it counts is a few pieces' worth of text, never the whole
        // remaining line — that made a single long line quadratic.
        let (mut lo, mut hi) = (p + 1, last);
        let mut step = 1;
        while lo < last {
            let probe = (lo + step).min(last);
            if fits(probe) {
                lo = probe;
                step *= 2;
            } else {
                hi = probe - 1;
                break;
            }
        }
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        out.push((bounds[p], bounds[lo]));
        p = lo;
    }
    out
}

// ---------------------------------------------------------------------------
// Packing
// ---------------------------------------------------------------------------

fn pack(
    s: &Section,
    atoms: &Atoms,
    sizes: Sizes,
    counter: &dyn TokenCounter,
    out: &mut Vec<Draft>,
) {
    let list = &atoms.list;
    let mut p = Packer {
        cur: Vec::new(),
        carried: 0,
        i: 0,
    };
    loop {
        if p.i >= list.len() {
            if p.cur.is_empty() {
                break;
            }
            // The last chunk of the section: a correction may hand atoms
            // back, in which case the loop goes round again.
            p.close(s, atoms, sizes, counter, out, false);
            continue;
        }
        let a = &list[p.i];
        // A heading ends the chunk before it, unless that chunk is headings
        // only (a heading directly under its parent stays with it).
        if a.kind == Kind::Heading && p.cur.iter().any(|k| list[*k].kind != Kind::Heading) {
            p.close(s, atoms, sizes, counter, out, false);
            continue;
        }
        if !p.cur.is_empty() {
            let mut next = p.cur.clone();
            next.push(p.i);
            if estimate(atoms, &next, counter) > sizes.chunk_tokens {
                p.close(s, atoms, sizes, counter, out, true);
                continue;
            }
        }
        p.cur.push(p.i);
        p.i += 1;
    }
}

/// The chunk being filled: atom indices, how many of them at its head were
/// carried over as overlap, and the next atom to look at.
struct Packer {
    cur: Vec<usize>,
    carried: usize,
    i: usize,
}

impl Packer {
    /// Emit the current chunk. The estimate that filled it sums atom counts;
    /// here the real payload is counted, and atoms that turn out not to fit
    /// go back to the queue — never below the first atom this chunk added,
    /// so a correction cannot re-emit the previous chunk's overlap on its
    /// own. A chunk a size limit ended (`by_size`) does not end on a table
    /// header, an opening fence or a heading: those move on with what they
    /// introduce, and the next chunk opens with overlap.
    fn close(
        &mut self,
        s: &Section,
        atoms: &Atoms,
        sizes: Sizes,
        counter: &dyn TokenCounter,
        out: &mut Vec<Draft>,
        by_size: bool,
    ) {
        let list = &atoms.list;
        while self.cur.len() > self.carried + 1
            && exact(s, atoms, &self.cur, counter).1 > sizes.chunk_tokens
        {
            self.cur.pop();
            self.i -= 1;
        }
        while self.carried > 0 && exact(s, atoms, &self.cur, counter).1 > sizes.chunk_tokens {
            self.cur.remove(0);
            self.carried -= 1;
        }
        while by_size
            && self.cur.len() > self.carried + 1
            && self.i < list.len()
            && self.cur.last().is_some_and(|k| {
                matches!(
                    list[*k].kind,
                    Kind::TableHeader | Kind::CodeOpen | Kind::Heading
                )
            })
        {
            self.cur.pop();
            self.i -= 1;
        }
        flush(s, atoms, &self.cur, counter, out);
        let next = if by_size && self.i < list.len() {
            overlap(atoms, &self.cur, self.i, sizes, counter)
        } else {
            Vec::new()
        };
        self.carried = next.len();
        self.cur = next;
    }
}

/// The trailing prose of a chunk a size limit ended, to open the next one
/// with — whole atoms, at most `overlap` tokens, and only when the next atom
/// still fits beside it and continues the same heading.
fn overlap(
    atoms: &Atoms,
    cur: &[usize],
    next: usize,
    sizes: Sizes,
    counter: &dyn TokenCounter,
) -> Vec<usize> {
    let list = &atoms.list;
    let n = &list[next];
    if sizes.overlap == 0 || n.kind != Kind::Prose {
        return Vec::new();
    }
    let mut carried: Vec<usize> = Vec::new();
    let mut total = 0usize;
    for k in cur.iter().rev() {
        let a = &list[*k];
        if a.kind != Kind::Prose || a.heading_path != n.heading_path {
            break;
        }
        if total + a.tokens > sizes.overlap {
            break;
        }
        total += a.tokens;
        carried.insert(0, *k);
    }
    // Never carry the whole chunk: the next one must move forward.
    if carried.len() == cur.len() {
        carried.remove(0);
    }
    while !carried.is_empty() {
        let mut probe = carried.clone();
        probe.push(next);
        if estimate(atoms, &probe, counter) <= sizes.chunk_tokens {
            break;
        }
        carried.remove(0);
    }
    carried
}

/// Summed atom counts plus the heading path and any repeated header or fence
/// — cheap, and close; [`exact`] is the check.
fn estimate(atoms: &Atoms, idx: &[usize], counter: &dyn TokenCounter) -> usize {
    let list = &atoms.list;
    let path = chunk_path(atoms, idx);
    let mut t = if path.is_empty() {
        0
    } else {
        counter.count(&path) + 1
    };
    t += idx.iter().map(|k| list[*k].tokens + 1).sum::<usize>();
    let (prefix, suffix) = wrap(atoms, idx);
    t += prefix.map_or(0, |p| counter.count(&p) + 1);
    t += suffix.map_or(0, |p| counter.count(&p) + 1);
    t
}

/// The heading path a chunk carries: that of its first non-heading atom, or
/// of its last atom when it is headings only.
fn chunk_path(atoms: &Atoms, idx: &[usize]) -> String {
    let list = &atoms.list;
    idx.iter()
        .map(|k| &list[*k])
        .find(|a| a.kind != Kind::Heading)
        .or_else(|| idx.last().map(|k| &list[*k]))
        .map(|a| a.heading_path.clone())
        .unwrap_or_default()
}

/// The repeated table header or re-opened fence in front of a chunk, and the
/// closing fence behind it, when a split group runs across its edges.
fn wrap(atoms: &Atoms, idx: &[usize]) -> (Option<String>, Option<String>) {
    let list = &atoms.list;
    let (Some(first), Some(last)) = (idx.first(), idx.last()) else {
        return (None, None);
    };
    let (f, l) = (&list[*first], &list[*last]);
    let prefix = match (f.kind, f.group.map(|g| &atoms.groups[g])) {
        (Kind::TableRow, Some(Group::Table { header })) => Some(header.clone()),
        (Kind::CodeLine | Kind::CodeClose, Some(Group::Code { open, .. })) => Some(open.clone()),
        _ => None,
    };
    let suffix = match (l.kind, l.group.map(|g| &atoms.groups[g])) {
        (Kind::CodeOpen | Kind::CodeLine, Some(Group::Code { close, .. })) => Some(close.clone()),
        _ => None,
    };
    (prefix, suffix)
}

/// The chunk's payload and its real token count.
fn exact(s: &Section, atoms: &Atoms, idx: &[usize], counter: &dyn TokenCounter) -> (String, usize) {
    let list = &atoms.list;
    let start = list[idx[0]].start;
    let end = list[*idx.last().unwrap_or(&idx[0])].end;
    let (prefix, suffix) = wrap(atoms, idx);
    let mut payload = String::new();
    if let Some(p) = prefix {
        payload.push_str(p.trim_end_matches(['\n', '\r']));
        payload.push('\n');
    }
    payload.push_str(&s.text[start..end]);
    if let Some(x) = suffix {
        payload.push('\n');
        payload.push_str(&x);
    }
    let tokens = counter.count(&embed_text(&chunk_path(atoms, idx), &payload));
    (payload, tokens)
}

fn flush(
    s: &Section,
    atoms: &Atoms,
    idx: &[usize],
    counter: &dyn TokenCounter,
    out: &mut Vec<Draft>,
) {
    if idx.is_empty() {
        return;
    }
    let list = &atoms.list;
    let (payload, tokens) = exact(s, atoms, idx, counter);
    let start = list[idx[0]].start;
    let end = list[*idx.last().unwrap_or(&idx[0])].end;
    out.push(Draft {
        page: s.page,
        heading_path: chunk_path(atoms, idx),
        span: (s.offset + start, s.offset + end),
        payload,
        tokens,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One token per whitespace-separated word, plus one per newline: easy to
    /// reason about in assertions, and deterministic like the real counter.
    struct Words;

    impl TokenCounter for Words {
        fn name(&self) -> String {
            "words".into()
        }
        fn count(&self, text: &str) -> usize {
            text.split_whitespace().count() + text.matches('\n').count()
        }
    }

    fn one(text: &str) -> Vec<Section> {
        assemble(vec![Part {
            page: None,
            heading: None,
            body: text.into(),
        }])
        .sections
    }

    fn sizes(chunk_tokens: usize, overlap: usize) -> Sizes {
        Sizes {
            chunk_tokens,
            overlap,
        }
    }

    #[test]
    fn headings_start_chunks_and_build_the_path() {
        let text = "# Taxes\n\nIntro line here.\n\n## 2025\n\nPaid in March.\n\n### Refund\n\nCame in May.\n\n## 2026\n\nNot yet.";
        let out = chunk(&one(text), sizes(100, 0), &Words);
        let paths: Vec<&str> = out.iter().map(|d| d.heading_path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Taxes",
                "Taxes > 2025",
                "Taxes > 2025 > Refund",
                "Taxes > 2026"
            ]
        );
        assert!(out[1].payload.starts_with("## 2025"));
        assert!(out[1].payload.contains("Paid in March."));
        // Spans are verbatim regions of the assembled text.
        let full = &assemble(vec![Part {
            page: None,
            heading: None,
            body: text.into(),
        }])
        .text;
        for d in &out {
            assert_eq!(&full[d.span.0..d.span.1], d.payload);
        }
    }

    #[test]
    fn a_heading_directly_under_its_parent_stays_with_it() {
        let out = chunk(&one("# A\n## B\ntext under b"), sizes(100, 0), &Words);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].heading_path, "A > B");
    }

    #[test]
    fn no_chunk_is_bigger_than_the_budget_and_nothing_is_lost() {
        let lines: Vec<String> = (0..60)
            .map(|i| format!("line {i} has five words"))
            .collect();
        let text = lines.join("\n");
        let out = chunk(&one(&text), sizes(40, 0), &Words);
        assert!(out.len() > 5);
        for d in &out {
            assert!(d.tokens <= 40, "{} > 40: {:?}", d.tokens, d.payload);
        }
        // Without overlap the chunks tile the text exactly.
        let joined: Vec<&str> = out.iter().flat_map(|d| d.payload.lines()).collect();
        assert_eq!(joined, text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn overlap_carries_trailing_lines_into_the_next_chunk() {
        let lines: Vec<String> = (0..30).map(|i| format!("row {i} a b c")).collect();
        let text = lines.join("\n");
        let out = chunk(&one(&text), sizes(30, 12), &Words);
        assert!(out.len() > 2);
        for w in out.windows(2) {
            // The previous chunk's last two lines (10 of the 12 overlap
            // tokens) open the next one.
            let prev: Vec<&str> = w[0].payload.lines().collect();
            let tail = prev[prev.len() - 2..].join("\n");
            assert!(
                w[1].payload.starts_with(&tail),
                "{:?} should open with {tail:?}",
                w[1].payload
            );
            assert!(w[1].span.0 < w[0].span.1, "overlapping spans");
        }
        for d in &out {
            assert!(d.tokens <= 30);
        }
    }

    #[test]
    fn a_fence_that_fits_stays_whole() {
        let text = "Before.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nAfter.";
        let out = chunk(&one(text), sizes(100, 0), &Words);
        assert_eq!(out.len(), 1);
        assert!(out[0].payload.contains("fn a() {}\n\nfn b() {}\n```"));
        // At a budget where the fence only fits alone, it is still one piece.
        let out = chunk(&one(text), sizes(16, 0), &Words);
        let fence: Vec<&Draft> = out.iter().filter(|d| d.payload.contains("fn a")).collect();
        assert_eq!(fence.len(), 1);
        assert!(fence[0].payload.contains("fn b"));
    }

    #[test]
    fn a_split_fence_is_reopened_and_closed_in_every_piece() {
        let body: Vec<String> = (0..40).map(|i| format!("let x{i} = {i};")).collect();
        let text = format!("```rust\n{}\n```", body.join("\n"));
        let out = chunk(&one(&text), sizes(30, 0), &Words);
        assert!(out.len() > 2);
        for d in &out {
            assert!(d.payload.starts_with("```rust"), "{:?}", d.payload);
            assert!(d.payload.trim_end().ends_with("```"), "{:?}", d.payload);
            assert!(d.tokens <= 30);
        }
    }

    #[test]
    fn a_split_table_repeats_its_header() {
        let mut rows = vec!["| Month | Rent |".to_string(), "| --- | --- |".to_string()];
        rows.extend((0..40).map(|i| format!("| m{i} | {i} |")));
        let text = rows.join("\n");
        let out = chunk(&one(&text), sizes(30, 5), &Words);
        assert!(out.len() > 2);
        for d in &out {
            assert!(
                d.payload.starts_with("| Month | Rent |\n| --- | --- |"),
                "{:?}",
                d.payload
            );
            assert!(d.tokens <= 30);
        }
        // Every row is in exactly one chunk: tables are not overlapped.
        let rows_seen: usize = out
            .iter()
            .map(|d| d.payload.lines().filter(|l| l.starts_with("| m")).count())
            .sum();
        assert_eq!(rows_seen, 40);
    }

    #[test]
    fn a_table_that_fits_stays_whole() {
        let text = "| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |";
        let out = chunk(&one(text), sizes(100, 0), &Words);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, text);
    }

    #[test]
    fn chunks_never_cross_pages_or_sheets() {
        let a = assemble(vec![
            Part {
                page: Some(1),
                heading: None,
                body: "first page text".into(),
            },
            Part {
                page: Some(2),
                heading: None,
                body: "second page text".into(),
            },
        ]);
        assert!(a.text.starts_with("--- page 1 ---\nfirst page text"));
        let out = chunk(&a.sections, sizes(100, 20), &Words);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].page, out[1].page), (Some(1), Some(2)));
        assert_eq!(&a.text[out[1].span.0..out[1].span.1], "second page text");

        let sheets = assemble(vec![
            Part {
                page: None,
                heading: Some("Sheet: Budget".into()),
                body: "| a |\n| - |\n| 1 |".into(),
            },
            Part {
                page: None,
                heading: Some("Sheet: Plan".into()),
                body: "| b |\n| - |\n| 2 |".into(),
            },
        ]);
        let out = chunk(&sheets.sections, sizes(100, 0), &Words);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].heading_path, "Sheet: Budget");
        assert_eq!(out[1].heading_path, "Sheet: Plan");
    }

    #[test]
    fn a_line_too_long_for_a_chunk_splits_at_sentences_then_words() {
        let long = (0..20)
            .map(|i| format!("Sentence number {i} is here."))
            .collect::<Vec<_>>()
            .join(" ");
        let out = chunk(&one(&long), sizes(12, 0), &Words);
        assert!(out.len() > 5);
        for d in &out {
            assert!(d.tokens <= 12, "{:?}", d.payload);
        }
        let words = (0..50)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = chunk(&one(&words), sizes(10, 0), &Words);
        for d in &out {
            assert!(d.tokens <= 10);
        }
        assert_eq!(
            out.iter()
                .map(|d| d.payload.split_whitespace().count())
                .sum::<usize>(),
            50
        );
    }

    #[test]
    fn the_same_input_chunks_the_same_way() {
        let text = "# A\n\none two three\n\n## B\n\n| x | y |\n| - | - |\n| 1 | 2 |";
        let a = chunk(&one(text), sizes(8, 2), &Words);
        let b = chunk(&one(text), sizes(8, 2), &Words);
        assert_eq!(a, b);
    }

    /// With the gateway's real tokenizer, whose counts of a joined text are
    /// not the sum of its parts, the budget still holds for every chunk that
    /// is more than one piece.
    #[test]
    fn the_budget_holds_under_the_real_tokenizer() {
        let counter = crate::quickdoc::query::TiktokenCounter;
        let mut text = String::from("# Steuererklärung 2025\n\n");
        for i in 0..80 {
            text.push_str(&format!(
                "Zeile {i}: Die Gebühr für März betrug {} €, fällig am {}.{}.\n",
                i * 37,
                i % 28 + 1,
                i % 12 + 1
            ));
            if i % 17 == 0 {
                text.push_str(
                    "\n## Abschnitt\n\n| Monat | Betrag | Notiz |\n| --- | --- | --- |\n",
                );
                for r in 0..12 {
                    text.push_str(&format!("| M{r} | {} | ok |\n", r * 11));
                }
                text.push('\n');
            }
        }
        for (budget, overlap) in [(64, 16), (128, 32), (512, 64)] {
            let out = chunk(&one(&text), sizes(budget, overlap), &counter);
            assert!(!out.is_empty());
            for d in &out {
                assert!(
                    d.tokens <= budget,
                    "{} > {budget}: {:?}",
                    d.tokens,
                    d.payload
                );
                assert_eq!(
                    d.tokens,
                    counter.count(&embed_text(&d.heading_path, &d.payload))
                );
            }
        }
    }

    /// Counts what it is asked to count: the work a chunker does, independent of
    /// how fast the machine is. One token per four bytes, like real text.
    struct Metered(std::sync::atomic::AtomicUsize);

    impl TokenCounter for Metered {
        fn name(&self) -> String {
            "metered".into()
        }
        fn count(&self, text: &str) -> usize {
            self.0
                .fetch_add(text.len(), std::sync::atomic::Ordering::Relaxed);
            text.len().div_ceil(4)
        }
    }

    /// A single line — minified JSON, a paragraph-per-line export — used to
    /// re-count the whole remaining line for every piece cut from it: work
    /// quadratic in the line (26-46 s for 1.6 MB). Bounded here by the bytes
    /// the counter is asked to measure, which does not depend on the clock.
    #[test]
    fn a_single_long_line_is_chunked_in_linear_work() {
        for line in [
            "Die Steuer ist bis Juli faellig. Belege aufbewahren. ".repeat(8_000),
            "{\"id\":1,\"betrag\":\"12.50\"},".repeat(15_000),
            "x".repeat(400_000),
        ] {
            let m = Metered(Default::default());
            let t0 = std::time::Instant::now();
            let out = chunk(&one(&line), sizes(128, 16), &m);
            let counted = m.0.load(std::sync::atomic::Ordering::Relaxed);
            assert!(out.len() > 100, "{} chunks", out.len());
            assert!(
                counted < line.len() * 40,
                "{counted} bytes counted for a {}-byte line",
                line.len()
            );
            // Debug builds included; the old code took minutes here.
            assert!(t0.elapsed().as_secs() < 30, "{:?}", t0.elapsed());
            for d in &out {
                assert!(d.tokens <= 128, "{} tokens", d.tokens);
            }
        }
    }

    /// Splitting a long line loses nothing: without overlap the pieces tile
    /// the line exactly, whichever level (sentence, word, character) cut them.
    #[test]
    fn a_long_line_is_tiled_exactly_at_every_split_level() {
        for line in [
            "One two. Three four five! Six? ".repeat(300),
            "word ".repeat(2_000),
            "abcdefghij".repeat(1_000),
        ] {
            let out = chunk(&one(&line), sizes(50, 0), &Metered(Default::default()));
            let joined: String = out.iter().map(|d| d.payload.as_str()).collect();
            assert_eq!(joined.trim_end(), line.trim_end());
        }
    }

    #[test]
    fn a_stopped_chunker_returns_none() {
        let text = "line of words\n".repeat(2_000);
        let m = Metered(Default::default());
        assert!(chunk_until(&one(&text), sizes(50, 0), &m, &|| true).is_none());
        let seen = std::sync::atomic::AtomicUsize::new(0);
        // Stops after some work has been done, mid-file.
        let stop = || seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 500;
        assert!(chunk_until(&one(&text), sizes(50, 0), &m, &stop).is_none());
        assert!(chunk_until(&one(&text), sizes(50, 0), &m, &|| false).is_some());
    }

    #[test]
    fn an_empty_section_has_no_chunks() {
        assert!(chunk(&one("   \n\n"), sizes(10, 0), &Words).is_empty());
    }
}
