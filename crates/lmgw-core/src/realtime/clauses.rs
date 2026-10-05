//! The chat stream → TTS seam (realtime §8.1).
//!
//! - [`ClauseAggregator`] is a clause splitter taken over from an earlier
//!   prototype, ported with its six tests and extended for what the
//!   first live run spoke wrongly (§23, L7). **It cuts only where a
//!   sentence ends** — a `.`, `!` or `?` and then whitespace — **and at a
//!   line end**: never at a comma, never at a word count (TTS batches,
//!   2026-10-05). A TTS speaks each request as an utterance of its own, with
//!   the engine's silence at both ends, so a cut inside a sentence ("In
//!   Berlin," | "it's currently around 14 degrees.") was heard as two, and
//!   the owner's A/B listening preferred the sentence whole. The prototype's
//!   first chunk at the first comma (+9 ms median against a 6-word cap) and
//!   its later 24-word cap are gone; an engine that needs shorter input chunks
//!   it itself (audio.cpp does, 300 characters for Supertonic). Decimals
//!   (`3.14`, German `3,14`) stay whole; a sentence end at the buffer end
//!   waits for the next character. Since the live run also:
//!   - **other scripts' sentence ends count** (`stops`): `।` `॥` `։` `؟` `።`
//!     end a sentence like `!` and `?`, with whitespace after them; the CJK
//!     `。` `！` `？` `｡` need none, and a closing quote or bracket after one
//!     stays with its sentence — the cut is after the last, so the buffer's
//!     end waits for the next character, as a `.` does. Without them such a
//!     text got no cut until a line end, and its first audio waited for a
//!     whole paragraph;
//!   - **a line end is a boundary** — an answer opening with a heading, a
//!     list or a code block otherwise gave no audio until its first
//!     punctuation, maybe the end of the stream, and then one huge TTS call;
//!   - **a list marker at a line start is not text** — the "2." of the next
//!     item is no sentence end, and was glued to the end of the item before;
//!   - **a known abbreviation is no sentence end** ([`ABBREVIATIONS`]),
//!     unless it is one that can end a sentence and a capital follows
//!     ([`SENTENCE_FINAL`]: "… usw. Dann"); titles ([`TITLES`]: "Dr.") never
//!     are;
//!   - **the scan is incremental**: each delta costs its own length, not the
//!     whole buffer's again;
//!   - **a clause cut at a line end, or the last one, is closed**: one
//!     without closing punctuation ("## Weather", a list item) gets a full
//!     stop, so it is spoken as finished rather than trailing off (package B
//!     review 4) — and the transcript says it as it was spoken;
//!   - **block markdown is not text** (`blocks`): a list marker is dropped
//!     from its clause, `N.` is one only when it starts or continues the
//!     numbering or follows another item, and never before a month ("3.
//!     Oktober", "1. Mai" at a line start keep their number), and fenced
//!     code is never a clause at all — a backtick "fence" with a backtick
//!     later on its line is inline code, and is text;
//!   - **a table is no clause either** (chat-voice design §6.2): a line that
//!     starts with `|` (after up to three spaces) and closes with one, or is
//!     a delimiter row, or is followed by a delimiter row, starts a table,
//!     and every line that starts with `|` after it is a row — skipped whole
//!     once its line end is here, or at the end of the stream. A line that
//!     starts with `|` and is none of these ("|x| ist der Betrag") is text;
//!     it waits for the line after it. A table without leading pipes is not
//!     detected (`blocks`);
//!   - **a skipped block is reported where it begins** ([`Item::Block`],
//!     [`Self::push_items`]): a fence as it opens, with its info string's
//!     first word, and a table at its first row. The speech splitter says
//!     it only for the Chat's callers ([`Announce`]); a stock session skips
//!     it silently;
//!   - **a German ordinal or date is no sentence end** (`ordinal`): "am 3.
//!     Oktober" stays one clause, so a voice reads "dritten" without a
//!     pause, while "Das kostet 30. Dann …" still ends the sentence;
//!   - **a bare number after a sentence end opens the next clause** (live
//!     run 3, D4; R4 M2): a "N." right after a sentence end, before a
//!     capital, that starts or continues the numbering ("… weiter. 1.
//!     Wasser holen. 2. Brot kaufen.", "Das ist klar. 1. FC Köln ist
//!     abgestiegen.") is no sentence end — it was a clause of its own,
//!     spoken "Eins." with a gap. It stays the text of the clause it opens
//!     and is said with it: an inline list's marker reads like a German
//!     ordinal ("1. Bundesliga", "1. Advent", "Wie viele? 1. Das reicht."),
//!     so it is never dropped — and since the clause begins mid-line
//!     ([`Placed::line_start`]), the speakable pass keeps it too. The TTS
//!     gets it without its dot, which a voice read as a sentence end and
//!     paused after; the transcript keeps it ([`Placed::inline_item`], R5
//!     F2);
//!   - **an inline tag is no words** (`tagged`, WP10 D8): a tag alone on a
//!     line is no clause;
//! - [`speakable`] runs on each clause before TTS (`speakable`): markdown
//!   syntax, rule lines, list markers, abbreviations, parentheses — never an
//!   inline tag, which comes out canonical (`(laughs)` → `[laughs]`). The
//!   cleaned text is also the spoken transcript ([`Spoken`]: without its
//!   tags, and with an inline list's dot the TTS does not get).

mod abbrev;
mod announce;
mod blocks;
mod ordinal;
mod speakable;
mod stops;
mod tagged;
#[cfg(test)]
mod tests;

use abbrev::AtDot;
pub use abbrev::{ABBREVIATIONS, SENTENCE_FINAL, TITLES};
pub use announce::{Announce, Block, Item};
use blocks::{Fence, Marker, Numbering, Probe};
use ordinal::Ordinal;
pub use speakable::{speakable, speakable_at};
pub use tagged::Spoken;
use tagged::TagScan;

// ─── clause aggregator (LLM→TTS seam) ───────────────────────────────────────

/// A clause and where it sits in the stream: byte offsets into everything
/// pushed since the aggregator was made. What lies between the end of the
/// clause before and `start` is no text of this clause — fenced code, a list
/// marker, whitespace, a clause that said nothing — and `start..end` is the
/// clause as the model wrote it (B2 review 2: the model's history keeps it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// The clause, trimmed and closed, as [`ClauseAggregator::push`] gives it.
    pub text: String,
    /// Where the clause's own text begins.
    pub start: usize,
    /// One past its last byte, the boundary included.
    pub end: usize,
    /// Its text begins at a line start of the stream — the stream's start,
    /// or nothing but spaces before it since a line end. One that begins
    /// mid-line, after a sentence end, has no list marker first in it: a
    /// number there is text, said ([`speakable_at`], R4 M2).
    pub line_start: bool,
    /// Its text opens with an inline list's number, a "N." right after a
    /// sentence end that starts or continues the numbering
    /// (`ClauseAggregator::inline_marker`): said, and kept in the
    /// transcript with its dot, but sent to the TTS without it
    /// ([`unstopped_number`], R5 F2).
    pub inline_item: bool,
}

/// Accumulates streamed content and emits speakable chunks (module doc).
pub struct ClauseAggregator {
    buf: String,
    /// Bytes taken off the front of `buf` so far — clauses, and what was no
    /// text — over the whole stream: where `buf` starts in it.
    taken: usize,
    /// How far the buffer has been judged, so a delta is scanned once.
    scan: Scan,
    /// Byte ranges of the judged part of `buf` that are no text — list
    /// markers, fence lines, fenced code — in order: left out of the clause
    /// they fall in.
    skip: Vec<(usize, usize)>,
    /// The list numbering so far (`blocks`).
    numbering: Numbering,
    /// Where the open code fence's opening line starts in `buf`.
    fence_at: Option<usize>,
    /// `buf` begins at a line start of the stream ([`Placed::line_start`]).
    buf_line_start: bool,
    /// The clause being scanned opened with an inline list's number
    /// ([`Placed::inline_item`]); taken with it.
    inline_item: bool,
    /// The skipped blocks that began since the last clause was handed on,
    /// in order, each with its offset in the stream ([`Self::push_items`]).
    blocks: Vec<(Block, usize)>,
    /// The last line judged was a table row: the next one continues the
    /// table, and is not reported again.
    table: bool,
}

/// The scan's state at `pos`: everything before it has been judged and is
/// no boundary.
#[derive(Debug, Clone, Copy)]
struct Scan {
    /// Byte offset of the next character to judge.
    pos: usize,
    /// The words of the clause so far, a tag's not counted (`tagged`): a
    /// line end or a sentence end cuts only once there is one.
    words: usize,
    in_word: bool,
    prev: Option<char>,
    /// The next character starts a line: the stream's start, or after a
    /// line end.
    line_start: bool,
    /// Inside a fenced code block, opened by this fence.
    fence: Option<Fence>,
    /// How far the buffer is known to hold no line end, from `pos` on: a
    /// line that waits for its end is not searched again from its start on
    /// every delta (B2 review D).
    no_newline_to: usize,
    /// The line so far began with spaces or tabs (`blocks`: a nested
    /// item).
    indented: bool,
    /// How many columns of indentation the line so far began with (a tab
    /// is four): a `|` after more than three is no table row (`blocks`).
    indent: u8,
    /// The line at `pos` starts with `|`, is whole up to here, and is no
    /// row on its own: it waits for the line after it, a table's header
    /// only when that is a delimiter row (`blocks`).
    held: Option<usize>,
    /// A `[` that may open an inline tag (`tagged`).
    tag: TagScan,
    /// The clause began right after a sentence end (`.`, `!`, `?`): a bare
    /// "N." first in it may be an inline list's marker (live run 3, D4).
    after_stop: bool,
}

impl Scan {
    fn at_line_start(line_start: bool) -> Self {
        Self {
            pos: 0,
            words: 0,
            in_word: false,
            prev: None,
            line_start,
            fence: None,
            no_newline_to: 0,
            indented: false,
            indent: 0,
            held: None,
            tag: TagScan::default(),
            after_stop: false,
        }
    }

    /// Where the line from `pos` ends in `buf` (just past its `\n`), or
    /// `None` with the search noted, so the next delta goes on from there.
    fn line_end(&mut self, buf: &str) -> Option<usize> {
        self.line_end_from(buf, self.pos)
    }

    /// [`Self::line_end`] for the line from `at`, at or after `pos`: the
    /// line after a [`Self::held`] one.
    fn line_end_from(&mut self, buf: &str, at: usize) -> Option<usize> {
        let from = self.no_newline_to.max(at).min(buf.len());
        match buf[from..].find('\n') {
            Some(k) => Some(from + k + 1),
            None => {
                self.no_newline_to = buf.len();
                None
            }
        }
    }
}

/// How a clause ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// At a sentence end: `.`, `!`, `?` or another script's (`stops`).
    Stop,
    /// At a line end, or the stream's: closed with a full stop when it has
    /// no punctuation of its own (module doc).
    Closed,
}

impl ClauseAggregator {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            taken: 0,
            scan: Scan::at_line_start(true),
            skip: Vec::new(),
            numbering: Numbering::default(),
            fence_at: None,
            buf_line_start: true,
            inline_item: false,
            blocks: Vec::new(),
            table: false,
        }
    }

    /// While a code fence is open: its bytes so far, from its opening line
    /// on — what a [`Self::flush`] now would leave unspoken. The caller
    /// logs it, with the session it belongs to.
    pub fn unclosed_fence(&self) -> Option<usize> {
        self.scan.fence?;
        Some(self.buf.len() - self.fence_at.unwrap_or(0).min(self.buf.len()))
    }

    /// Append a content delta; return any completed clauses (trimmed, non-empty).
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.push_placed(delta)
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    /// [`Self::push`], each clause with where it sits in the stream.
    pub fn push_placed(&mut self, delta: &str) -> Vec<Placed> {
        clauses_of(self.push_items(delta))
    }

    /// [`Self::push_placed`], with the skipped blocks that began among the
    /// clauses ([`Item::Block`]), each before the clause that follows it.
    pub fn push_items(&mut self, delta: &str) -> Vec<Item> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            let found = self.find_boundary(false);
            // A block judged on the way to this boundary began before it.
            out.extend(self.blocks.drain(..).map(|(b, at)| Item::Block(b, at)));
            match found {
                Some((end, cut)) => out.extend(self.take(end, cut).map(Item::Clause)),
                None => return out,
            }
        }
    }

    /// Bytes of the stream taken so far — after a [`Self::flush`], all of
    /// it.
    pub fn taken(&self) -> usize {
        self.taken
    }

    /// Emit whatever remains at end of stream (the final, possibly
    /// unpunctuated clause — closed, module doc). An unclosed code fence's
    /// lines are left out with it.
    pub fn flush(&mut self) -> Option<String> {
        self.flush_placed().map(|c| c.text)
    }

    /// [`Self::flush`], the clause with where it sits in the stream.
    pub fn flush_placed(&mut self) -> Option<Placed> {
        clauses_of(self.flush_items()).pop()
    }

    /// [`Self::flush_placed`], with the clauses and blocks the end of the
    /// stream settles ([`Item::Block`]): a table whose first row is the last,
    /// unfinished line, after the clause before it.
    pub fn flush_items(&mut self) -> Vec<Item> {
        // Every line is whole now: a row the stream ends inside is a row, and
        // a `|` line waiting for the line after it is judged by what came.
        let mut out = Vec::new();
        while let Some((end, cut)) = self.find_boundary(true) {
            out.extend(self.blocks.drain(..).map(|(b, at)| Item::Block(b, at)));
            out.extend(self.take(end, cut).map(Item::Clause));
        }
        // What was judged since the last boundary runs to the stream's end:
        // after the text still here.
        let blocks: Vec<Item> = self
            .blocks
            .drain(..)
            .map(|(b, at)| Item::Block(b, at))
            .collect();
        // The model never closed a fence: CommonMark runs it to the end, and
        // code is not spoken ([`Self::unclosed_fence`]).
        let end = match self.scan.fence {
            Some(_) => self.scan.pos,
            None => self.buf.len(),
        };
        out.extend(self.take(end, Cut::Closed).map(Item::Clause));
        out.extend(blocks);
        // An unclosed fence's lines are taken too, unsaid, and so are the
        // last rows.
        self.taken += self.buf.len();
        self.buf.clear();
        self.skip.clear();
        self.scan = Scan::at_line_start(true);
        self.numbering = Numbering::default();
        self.fence_at = None;
        self.buf_line_start = true;
        self.blocks.clear();
        self.table = false;
        out
    }

    /// The clause `buf[..end]`, without what is no text, trimmed and closed
    /// as `cut` says — taken off the buffer. `None` when nothing is left.
    fn take(&mut self, end: usize, cut: Cut) -> Option<Placed> {
        let mut text = String::with_capacity(end);
        // Where the first character of text sits in `buf`.
        let mut start = None;
        let mut keep = |text: &mut String, from: usize, piece: &str| {
            if start.is_none() {
                start = piece
                    .char_indices()
                    .find(|(_, c)| !c.is_whitespace())
                    .map(|(k, _)| from + k);
            }
            text.push_str(piece);
        };
        let mut at = 0;
        for &(from, to) in self.skip.iter().take_while(|(from, _)| *from < end) {
            let upto = from.max(at);
            keep(&mut text, at, &self.buf[at..upto]);
            at = to.min(end).max(at);
        }
        keep(&mut text, at, &self.buf[at..end]);
        let line_start = start.is_some_and(|s| self.begins_line(s));
        let inline_item = std::mem::take(&mut self.inline_item);
        let base = self.taken;
        self.taken += end;
        if end > 0 {
            self.buf_line_start = self.buf.as_bytes()[end - 1] == b'\n';
        }
        self.buf.drain(..end);
        self.fence_at = self.fence_at.map(|at| at.saturating_sub(end));
        self.skip.retain(|&(_, to)| to > end);
        for span in &mut self.skip {
            *span = (span.0.saturating_sub(end), span.1 - end);
        }
        let text = text.trim();
        let start = start?;
        if text.is_empty() {
            return None;
        }
        Some(Placed {
            text: match cut {
                Cut::Closed => closed(text),
                Cut::Stop => text.to_string(),
            },
            start: base + start,
            end: base + end,
            line_start,
            inline_item,
        })
    }

    /// Whether `buf[at]` begins a line of the stream: only spaces or tabs
    /// before it since a line end, or since the stream's start. A skipped
    /// list marker before it on its line is not nothing.
    fn begins_line(&self, at: usize) -> bool {
        let before = &self.buf[..at];
        let (line, from_start) = match before.rfind('\n') {
            Some(k) => (&before[k + 1..], true),
            None => (before, self.buf_line_start),
        };
        from_start && line.bytes().all(|b| b == b' ' || b == b'\t')
    }

    /// Bytes `from..to` of the buffer are no text.
    fn skip_span(&mut self, from: usize, to: usize) {
        match self.skip.last_mut() {
            Some(last) if last.1 == from => last.1 = to,
            _ => self.skip.push((from, to)),
        }
    }

    /// Byte index just past the first speakable boundary in `buf`, and how
    /// the clause before it ended, or None — judging only what the last call
    /// left unjudged. `ended`: the stream is over, so the last line is whole
    /// without its line end (a table row's judgement waits for none).
    fn find_boundary(&mut self, ended: bool) -> Option<(usize, Cut)> {
        let mut st = self.scan;
        loop {
            // What is left undecided waits for more text.
            let wait = |me: &mut Self, st: Scan| {
                me.scan = st;
                None
            };
            let Some(c) = self.buf[st.pos..].chars().next() else {
                return wait(self, st);
            };
            let off = st.pos;
            let byte_end = off + c.len_utf8();
            if st.line_start {
                let line = &self.buf[off..];
                // Fenced code is no clause (`blocks`): whole lines, skipped
                // once they are here, up to and with the closing fence.
                let fence = match st.fence {
                    Some(f) => Some((f, true)),
                    // A backtick fence is judged on its whole line: wait for
                    // its end without searching it again each delta.
                    None => match blocks::fence_open(line) {
                        Probe::Pending => {
                            st.line_end(&self.buf);
                            return wait(self, st);
                        }
                        Probe::Fence(f) => Some((f, false)),
                        Probe::None => None,
                    },
                };
                if let Some((f, inside)) = fence {
                    let Some(end) = st.line_end(&self.buf) else {
                        return wait(self, st);
                    };
                    // An opening line opens it; inside, only the closing
                    // fence ends it.
                    st.fence =
                        (!inside || !blocks::closes(&self.buf[off..end - 1], f)).then_some(f);
                    if !inside {
                        self.fence_at = Some(off);
                        let info = blocks::info(&self.buf[off..end - 1], f);
                        self.blocks.push((Block::Code { info }, self.taken + off));
                        self.table = false;
                    }
                    self.skip_span(off, end);
                    st.pos = end;
                    st.prev = Some('\n');
                    st.indent = 0;
                    continue;
                }
                // A table row is no clause either (`blocks`): skipped whole
                // once it is judged, the table reported at its first row. A
                // row continues a table, or starts one when it closes with a
                // pipe or is a delimiter row; any other line that starts with
                // `|` is a header only when a delimiter row follows it, so it
                // waits for that line to be whole — and is text otherwise.
                if c == '|' && st.indent <= 3 {
                    let end = match st.held {
                        Some(end) => end,
                        None => match st.line_end(&self.buf) {
                            Some(end) => end,
                            None if ended => self.buf.len(),
                            None => return wait(self, st),
                        },
                    };
                    let rows = if self.table || blocks::row_closed(&self.buf[off..end]) {
                        Some(end)
                    } else {
                        st.held = Some(end);
                        let next = match st.line_end_from(&self.buf, end) {
                            Some(next) => next,
                            None if ended => self.buf.len(),
                            None => return wait(self, st),
                        };
                        st.held = None;
                        blocks::delimiter_row(&self.buf[end..next]).then_some(next)
                    };
                    if let Some(rows) = rows {
                        if !std::mem::replace(&mut self.table, true) {
                            self.blocks.push((Block::Table, self.taken + off));
                        }
                        self.numbering.text();
                        self.skip_span(off, rows);
                        st.pos = rows;
                        st.prev = Some('\n');
                        st.indented = false;
                        st.indent = 0;
                        continue;
                    }
                }
                // Any other line ends the table: a blank one, text, an item.
                if c != ' ' && c != '\t' {
                    self.table = false;
                }
                // A list marker is not text: its number is no word and its
                // dot no sentence end, and it is left out of the clause;
                // indentation keeps the line start.
                match blocks::list_marker(line, self.numbering, false, st.indented) {
                    Marker::Pending => return wait(self, st),
                    Marker::Len { len, number } => {
                        self.numbering.took(number, st.indented);
                        self.skip_span(off, off + len);
                        st = Scan {
                            pos: off + len,
                            in_word: false,
                            prev: Some(' '),
                            line_start: false,
                            ..st
                        };
                        continue;
                    }
                    Marker::None if c == ' ' || c == '\t' => {
                        st.pos = byte_end;
                        st.prev = Some(c);
                        st.indented = true;
                        st.indent = st.indent.saturating_add(blocks::indent_of(c));
                        continue;
                    }
                    Marker::None => {
                        // A line with text ends a list; a blank one does not.
                        if !matches!(c, '\n' | '\r') {
                            self.numbering.text();
                        }
                        st.line_start = false;
                    }
                }
            }
            let next = self.buf[byte_end..].chars().next();
            if (c == '.' || stops::spaced(c) || stops::wide(c)) && next.is_none() {
                // A boundary punctuation needs a confirmed following
                // whitespace; at the buffer end it may be a streamed decimal
                // or abbreviation tail, so defer until more arrives (or flush).
                // A wide stop wants the character after it too: a closing
                // quote belongs to its sentence.
                return wait(self, st);
            }
            let was_in_word = st.in_word;
            let in_word = c.is_alphanumeric();
            // A tag's letters are no words (`tagged`); taken into the scan
            // only below, as the character is judged for good.
            let mut tag = st.tag;
            let words = st.words + tag.feed(c, in_word && !was_in_word);
            let next_ws = next.is_some_and(char::is_whitespace);
            // Numeric separator guard: digit-PUNCT-digit is a number, not a
            // break ("3.14"; a comma is none, so "3,14" needs no guard).
            let numeric = st.prev.is_some_and(|p| p.is_ascii_digit())
                && next.is_some_and(|n| n.is_ascii_digit());

            // A line end, once the line said something.
            if c == '\n' && words >= 1 {
                return self.cut(byte_end, Cut::Closed, &st);
            }
            // A sentence end — unless the dot is an abbreviation's.
            if (c == '.' || stops::spaced(c)) && next_ws && !numeric && words >= 1 {
                match (c, abbrev::at_dot(&self.buf, byte_end)) {
                    ('.', AtDot::Pending) => return wait(self, st),
                    ('.', AtDot::Yes) => {}
                    // A date or an ordinal ("am 3. Oktober") goes on.
                    ('.', AtDot::No) => match ordinal::at_dot(&self.buf, byte_end) {
                        Ordinal::Pending => return wait(self, st),
                        Ordinal::Yes => {}
                        // A bare "N." after a sentence end, a capital
                        // next: an inline list's marker, or a "1. FC
                        // Köln" — no sentence end, and the text of the
                        // clause it opens, said with it (D4, R4 M2).
                        Ordinal::No => match self.inline_marker(&st, byte_end) {
                            Some(n) => {
                                self.numbering.took(Some(n), false);
                                self.inline_item = true;
                            }
                            None => return self.cut(byte_end, Cut::Stop, &st),
                        },
                    },
                    _ => return self.cut(byte_end, Cut::Stop, &st),
                }
            }
            // A CJK sentence end needs no space after it, but takes the
            // closing quotes and brackets (and stops) that follow with it.
            if stops::wide(c) && words >= 1 {
                return match stops::run_end(&self.buf, byte_end) {
                    Some(end) => self.cut(end, Cut::Stop, &st),
                    None => wait(self, st),
                };
            }
            st = Scan {
                pos: byte_end,
                words,
                in_word,
                prev: Some(c),
                line_start: c == '\n',
                fence: st.fence,
                no_newline_to: st.no_newline_to,
                indented: false,
                indent: 0,
                held: None,
                tag,
                after_stop: st.after_stop,
            };
        }
    }

    /// The bare number before the dot ending at `end`, when it opens the
    /// clause it stands in rather than ending one (live run 3, D4; R4 M2):
    /// the clause so far is that number alone, right after a sentence end
    /// ("… weiter. 1. Wasser holen. 2. Brot kaufen."), and it starts or
    /// continues the numbering, as a line-start marker must (`blocks`). The
    /// caller has ruled out an ordinal or a date (`ordinal`): a capital
    /// follows, no month. Its text is never dropped.
    fn inline_marker(&self, st: &Scan, end: usize) -> Option<u64> {
        if !st.after_stop {
            return None;
        }
        let number = &self.buf[..end - 1];
        let digits = number.bytes().rev().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&digits) {
            return None;
        }
        let at = number.len() - digits;
        if !self.buf[..at].trim().is_empty() {
            return None;
        }
        let n: u64 = number[at..].parse().ok()?;
        self.numbering.continues(n, false).then_some(n)
    }

    /// A boundary at `end`: the scan starts afresh on what follows it — at
    /// a line start after a line end — still inside the fence it was in,
    /// and after a sentence end when it is one (`inline_marker`).
    fn cut(&mut self, end: usize, cut: Cut, st: &Scan) -> Option<(usize, Cut)> {
        self.scan = Scan {
            fence: st.fence,
            after_stop: cut == Cut::Stop,
            ..Scan::at_line_start(cut == Cut::Closed)
        };
        Some((end, cut))
    }
}

/// The clauses of `items`, the blocks left out.
fn clauses_of(items: Vec<Item>) -> Vec<Placed> {
    items
        .into_iter()
        .filter_map(|i| match i {
            Item::Clause(c) => Some(c),
            Item::Block(..) => None,
        })
        .collect()
}

/// `clause` with a full stop when it says something and has no closing
/// punctuation of its own (module doc) — after any closing quote, bracket
/// or emphasis marker, which the speakable pass handles as usual.
fn closed(clause: &str) -> String {
    const CLOSERS: &[char] = &['"', '\'', '”', '’', '«', '»', ')', ']', '*', '_', '`', '~'];
    const STOPS: &[char] = &['.', '!', '?', ':', ';', ',', '…'];
    let body = clause.trim_end_matches(|c| CLOSERS.contains(&c) || stops::closer(c));
    let stop = |c: char| STOPS.contains(&c) || stops::spaced(c) || stops::wide(c);
    if body.ends_with(stop) || !clause.chars().any(char::is_alphanumeric) {
        return clause.to_string();
    }
    format!("{clause}.")
}

/// The TTS's text of a clause that opens with an inline list's number
/// ([`Placed::inline_item`]): `tts`, its speakable text, with that number's
/// dot left out — "2. Brot kaufen." is sent "2 Brot kaufen." (R5 F2). Pocket
/// TTS read the dot as a sentence end and paused 450 to 650 ms between
/// "zwei" and the item's words (live run 3c), longer than the gap between
/// two clauses. The bare number is the same word said straight on, as a
/// line-start item's marker is no stop either (it is dropped); "1. FC Köln"
/// still says "eins" with no pause, which a colon or a comma would put
/// back. The transcript keeps the dot. A `tts` that does not open with
/// "N." and a space is sent as it is.
fn unstopped_number(tts: &str) -> String {
    let digits = tts.bytes().take_while(u8::is_ascii_digit).count();
    match tts[digits..].strip_prefix('.') {
        Some(rest) if digits > 0 && rest.starts_with(' ') => format!("{}{rest}", &tts[..digits]),
        _ => tts.to_string(),
    }
}

impl Default for ClauseAggregator {
    fn default() -> Self {
        Self::new()
    }
}
