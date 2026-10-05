//! Block markdown a voice does not read (realtime §8.1), judged at a line
//! start: list markers, code fences, setext underlines. The clause splitter
//! asks as the stream arrives — so a list item's marker is never text and a
//! fenced block is never a clause — and the speakable pass asks again of a
//! clause on its own.
//!
//! **Numbered markers follow the numbering** (package B review 3). `N.` at
//! a line start reads exactly like an ordinal or a year ("3. Oktober",
//! "2024. war …"), so it is a list marker only when it can start a list
//! (`1.`) or continues the one before at its own depth (`3.` after `2.`) —
//! a nested list's numbering is its own, so the outer "2." after "  2. b"
//! is still one (B2 review B). Any other number is a marker only as an
//! indented item right inside a list, a nested list that does not start at
//! one (B3 review L3): at the line start after a list, "42. Minute: Tor",
//! "100. Geburtstag" or "2026. war …" keep their number, blank line or not.
//! The cost: a list with a gap at the top ("1.", "2.", "4.") keeps "4." as
//! text. And never a marker when a month name follows: "1. Mai ist
//! Feiertag" is a date (`ordinal`, B2 review C). `N)` is never an ordinal,
//! and always a marker.
//!
//! **Fenced code is not spoken.** A voice reading `let x = 1;` aloud helps
//! nobody, so the lines between an opening fence (three or more backticks
//! or tildes) and its closing fence are skipped, as the fence lines are —
//! silently: the spoken transcript is what was said, so it carries no code
//! either. A backtick fence's info string may not hold a backtick
//! (CommonMark), so "```npm i``` installiert …" is inline code, not a fence
//! that would silence the rest of the answer (B2 review A): such a line is
//! judged once it is whole.
//!
//! **Tables are not spoken either** (chat-voice design §6.2). A table
//! starts at a line that starts with `|` — after up to three spaces; four or
//! more make it indented code in CommonMark — and closes with one too, or
//! is a delimiter row ([`row_closed`]); or at a line that starts with `|`
//! and is followed by a delimiter row ([`delimiter_row`]), GFM's header
//! row, whose closing pipe is optional. A line that starts with `|` and is
//! neither — "|x| ist der Betrag von x.", "|| true" — is text: such a line
//! waits for the line after it, one line of latency for it alone. Every
//! line that starts with `|` after the first continues the table. The rows
//! are skipped like fenced code, judged once their line is whole (the
//! clause splitter). A table written without leading pipes (`a | b`) is not
//! detected and is read as text: its rows cannot be told from a sentence
//! with a pipe in it. Nor is a table inside a list item indented four
//! columns or more: CommonMark measures that from the item's content
//! column, the splitter from the line start, so it is read as indented code
//! is, as text.

use super::ordinal::{self, Ordinal};

/// What starts a line.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Marker {
    /// A list marker `len` bytes long, its space included: `-`, `*`, `+` or
    /// `•`, or 1–9 digits and `.` or `)`, then a space or a tab. `number`:
    /// a numbered marker's number.
    Len {
        len: usize,
        number: Option<u64>,
    },
    /// The text so far could still become one.
    Pending,
    None,
}

/// The list so far (module doc): the latest numbered marker's number at
/// the line start and indented, and whether the last line with text was a
/// list item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Numbering {
    /// `[at the line start, indented]`.
    last: [Option<u64>; 2],
    in_list: bool,
}

impl Numbering {
    /// A marker was taken, `indented` or not: a numbered one moves that
    /// depth's numbering on, and an outer item starts a fresh nested list.
    pub fn took(&mut self, number: Option<u64>, indented: bool) {
        if number.is_some() {
            self.last[usize::from(indented)] = number;
        }
        if !indented {
            self.last[1] = None;
        }
        self.in_list = true;
    }

    /// A line with text and no marker: the list, if any, is over.
    pub fn text(&mut self) {
        self.in_list = false;
    }

    /// Whether `N.` may be a list marker here (module doc) — at a line
    /// start, or inline after a sentence end (`ClauseAggregator`, D4).
    pub fn continues(self, n: u64, indented: bool) -> bool {
        let last = self.last[usize::from(indented)];
        n == 1 || last.is_some_and(|l| l.checked_add(1) == Some(n)) || (indented && self.in_list)
    }
}

/// The list marker `line` starts with (indentation already skipped,
/// `indented` says whether there was any), with `numbering` the list's so
/// far. `whole`: `line` is all of the line (the speakable pass) rather than
/// what of the stream has arrived.
pub(super) fn list_marker(line: &str, numbering: Numbering, whole: bool, indented: bool) -> Marker {
    let mut chars = line.char_indices();
    let Some((_, first)) = chars.next() else {
        return Marker::Pending;
    };
    let gap = |at: usize, number: Option<u64>| match line[at..].chars().next() {
        None => Marker::Pending,
        Some(' ' | '\t') => Marker::Len {
            len: at + 1,
            number,
        },
        Some(_) => Marker::None,
    };
    if matches!(first, '-' | '*' | '+' | '•') {
        return gap(first.len_utf8(), None);
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return Marker::None;
    }
    let n: u64 = line[..digits].parse().unwrap_or(0);
    match line.as_bytes().get(digits) {
        None => Marker::Pending,
        Some(b'.') if !numbering.continues(n, indented) => Marker::None,
        Some(b'.') => match gap(digits + 1, Some(n)) {
            // A date, not an item: "1. Mai ist Feiertag".
            Marker::Len { len, number } => {
                match ordinal::month(&line[len..], whole || line[len..].contains('\n')) {
                    Ordinal::Yes => Marker::None,
                    Ordinal::Pending => Marker::Pending,
                    Ordinal::No => Marker::Len { len, number },
                }
            }
            other => other,
        },
        Some(b')') => gap(digits + 1, Some(n)),
        Some(_) => Marker::None,
    }
}

/// A code fence: its character and how many of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fence {
    mark: u8,
    len: usize,
}

/// Whether a line opens a fence.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Probe {
    Fence(Fence),
    /// The text so far could still become one.
    Pending,
    None,
}

/// Whether the text at a line start (indentation skipped) opens a code
/// fence: three or more backticks or tildes — for backticks, with no
/// backtick in the rest of the line (module doc). `text` may run past the
/// line's end; without a line end in it, the line is not all here yet, so
/// a backtick fence is `Pending` until it is.
pub(super) fn fence_open(text: &str) -> Probe {
    let line = text.split('\n').next().unwrap_or("");
    fence_line(line, line.len() < text.len())
}

/// [`fence_open`] for a whole line, without its line end (the speakable
/// pass).
pub(super) fn fence_of_line(line: &str) -> Probe {
    fence_line(line, true)
}

fn fence_line(line: &str, whole: bool) -> Probe {
    let b = line.as_bytes();
    let Some(&mark) = b.first().filter(|m| matches!(m, b'`' | b'~')) else {
        return if b.is_empty() && !whole {
            Probe::Pending
        } else {
            Probe::None
        };
    };
    let len = b.iter().take_while(|&&x| x == mark).count();
    if len < 3 {
        return if len == b.len() && !whole {
            Probe::Pending
        } else {
            Probe::None
        };
    }
    if mark == b'`' {
        if line[len..].contains('`') {
            return Probe::None;
        }
        if !whole {
            return Probe::Pending;
        }
    }
    Probe::Fence(Fence { mark, len })
}

/// The first word of the info string of `line`, a fence's opening line (its
/// line end left out): ```` ```rust title="x" ```` → `rust`; a word wrapped in
/// braces or dots (```` ```{.python} ````) gives what is inside. `None` for
/// a fence with no info string.
pub(super) fn info(line: &str, fence: Fence) -> Option<String> {
    let t = line.trim_start();
    let rest = t.get(fence.len..)?;
    let word = rest
        .split(|c: char| c.is_whitespace() || c == ',')
        .find(|w| !w.is_empty())?;
    let word = word.trim_matches(|c: char| !(c.is_alphanumeric() || c == '+' || c == '#'));
    (!word.is_empty()).then(|| word.to_string())
}

/// The number of columns of indentation `c` is worth at a line start: a tab
/// runs to the next stop of four (CommonMark), so it is four on its own.
pub(super) fn indent_of(c: char) -> u8 {
    if c == '\t' {
        4
    } else {
        1
    }
}

/// Whether the whole `line` closes `fence`: at least as many of its
/// character, and nothing else.
pub(super) fn closes(line: &str, fence: Fence) -> bool {
    let t = line.trim();
    t.len() >= fence.len && t.bytes().all(|x| x == fence.mark)
}

/// Whether `line`, which starts with `|`, is a table row on its own (module
/// doc): it closes with a pipe too, or is a delimiter row.
pub(super) fn row_closed(line: &str) -> bool {
    let t = line.trim();
    (t.len() > 1 && t.ends_with('|')) || delimiter_row(line)
}

/// Whether `line` is a GFM table's delimiter row: cells of hyphens, each
/// with an optional colon at either end, split by pipes — at least one, so
/// a thematic break or a setext underline (`---`) is none — after up to
/// three columns of indentation.
pub(super) fn delimiter_row(line: &str) -> bool {
    let body = line.trim_start_matches([' ', '\t']);
    let indent: u32 = line[..line.len() - body.len()]
        .chars()
        .map(|c| u32::from(indent_of(c)))
        .sum();
    let t = body.trim_end();
    if indent > 3 || !t.contains('|') {
        return false;
    }
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').all(|cell| {
        let c = cell.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.bytes().all(|b| b == b'-')
    })
}

/// A setext heading's underline — a line of only `=` or only `-` — which
/// says nothing (package B review, small items). Three or more dashes are
/// a thematic break as well; either way the line is silent.
pub(super) fn is_underline(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && (t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_line_is_a_row_when_it_closes_with_one_or_is_a_delimiter_row() {
        assert!(row_closed("| a | b |\n"));
        assert!(row_closed("|x|"));
        assert!(row_closed("|---|---\n"), "a delimiter row");
        assert!(!row_closed("|x| ist der Betrag von x.\n"));
        assert!(!row_closed("|| true"));
        assert!(
            !row_closed("| a | b"),
            "a header only with a delimiter row after it"
        );
        assert!(!row_closed("|"));
        for d in ["|---|---|", "---|---", " :--- | ---: ", "| :-: |\n", "|-"] {
            assert!(delimiter_row(d), "{d:?}");
        }
        for d in ["---", "|", "| a |", "|--x|", "    |---|", "\t|---|", "|  |"] {
            assert!(!delimiter_row(d), "{d:?}");
        }
    }

    #[test]
    fn numbered_markers_follow_the_numbering() {
        let none = Numbering::default();
        let len = |line: &str, numbering| match list_marker(line, numbering, true, false) {
            Marker::Len { len, number } => Some((len, number)),
            _ => None,
        };
        let nested = |line: &str, numbering| match list_marker(line, numbering, true, true) {
            Marker::Len { len, number } => Some((len, number)),
            _ => None,
        };
        assert_eq!(len("1. Apples", none), Some((3, Some(1))));
        // An ordinal or a year starts no list.
        assert_eq!(len("3. Oktober", none), None);
        assert_eq!(len("2024. war", none), None);
        // After 2., 3. continues it. Any other number right inside the list
        // is an item only when indented — a nested list of its own (B3
        // review L3): at the line start it is text, "42. Minute".
        let mut after_two = none;
        after_two.took(Some(2), false);
        assert_eq!(len("3. Plums", after_two), Some((3, Some(3))));
        assert_eq!(len("5. Plums", after_two), None);
        assert_eq!(len("42. Minute: Tor", after_two), None);
        assert_eq!(nested("5. Plums", after_two), Some((3, Some(5))));
        after_two.text();
        assert_eq!(nested("5. Plums", after_two), None);
        assert_eq!(len("3. Plums", after_two), Some((3, Some(3))));
        // A nested list's numbering is its own, and an outer item ends it.
        let mut inner = after_two;
        inner.took(Some(1), true);
        inner.took(Some(2), true);
        assert_eq!(len("3. Outer", inner), Some((3, Some(3))));
        assert_eq!(nested("3. c", inner), Some((3, Some(3))));
        inner.took(Some(3), false);
        assert_eq!(inner.last, [Some(3), None]);
        // A bullet leaves the numbering alone; `N)` is always a marker.
        after_two.took(None, false);
        assert_eq!(after_two.last[0], Some(2));
        assert_eq!(len("7) last", none), Some((3, Some(7))));
        assert_eq!(len("- item", none), Some((2, None)));
        assert_eq!(list_marker("12", none, false, false), Marker::Pending);
        assert_eq!(
            list_marker("1234567890. x", none, true, false),
            Marker::None
        );
        // A month after the number is a date, not an item (B2 review C).
        assert_eq!(len("1. Mai ist Feiertag", none), None);
        assert_eq!(len("1. Mai", none), None);
        assert_eq!(list_marker("1. Ma", none, false, false), Marker::Pending);
        assert_eq!(len("1. Mais", none), Some((3, Some(1))));
        assert_eq!(len("1. Äpfel", none), Some((3, Some(1))));
    }

    #[test]
    fn fences_and_underlines() {
        let tick = Fence { mark: b'`', len: 3 };
        assert_eq!(fence_open("```rust\n"), Probe::Fence(tick));
        assert_eq!(fence_of_line("```rust"), Probe::Fence(tick));
        assert_eq!(
            fence_open("```rust"),
            Probe::Pending,
            "the line is not whole"
        );
        assert_eq!(
            fence_open("~~~~"),
            Probe::Fence(Fence { mark: b'~', len: 4 })
        );
        assert_eq!(fence_open("``"), Probe::Pending);
        assert_eq!(fence_open(""), Probe::Pending);
        assert_eq!(fence_open("`code` here"), Probe::None);
        assert_eq!(fence_open("text"), Probe::None);
        // A backtick in the info string: inline code, no fence (B2 review A).
        assert_eq!(fence_open("```npm i``` installiert"), Probe::None);
        assert_eq!(fence_of_line("```npm i``` installiert"), Probe::None);
        assert_eq!(fence_of_line("``"), Probe::None);
        assert!(closes("```", tick) && closes("  ````  ", tick));
        // The info string's first word, for the Chat's announcement.
        assert_eq!(info("```rust", tick).as_deref(), Some("rust"));
        assert_eq!(info("```  rust title=\"x\"", tick).as_deref(), Some("rust"));
        assert_eq!(info("```{.python}", tick).as_deref(), Some("python"));
        assert_eq!(info("```c++", tick).as_deref(), Some("c++"));
        assert_eq!(info("```rust,ignore", tick).as_deref(), Some("rust"));
        assert_eq!(info("```", tick), None);
        assert_eq!(info("```   ", tick), None);
        assert!(!closes("``` rust", tick) && !closes("~~~", tick) && !closes("``", tick));
        for line in ["===", "=", "--", " ------ "] {
            assert!(is_underline(line), "{line}");
        }
        for line in ["", "= x", "-=-", "- item"] {
            assert!(!is_underline(line), "{line}");
        }
    }
}
