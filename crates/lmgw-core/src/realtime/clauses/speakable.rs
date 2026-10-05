//! The speakable pass (realtime §8.1): what of a clause a voice reads.
//!
//! It strips markdown syntax a voice must not read out — emphasis markers,
//! heading hashes, code fences with the code between them, backticks, link
//! targets (keeping the link text), bullet or number markers at a line start
//! (`N.` only when it starts or continues the numbering, `blocks`), setext
//! underlines (`===`), and thematic breaks (`---`, `***`, `___`: the first
//! live run spoke one as 1.48 s of silence, §23 L7) — writes out the known
//! abbreviations ([`super::ABBREVIATIONS`]),
//! and turns parentheses into commas: a voice has no brackets, and the live
//! run's "(Computer, Smartphone etc.) ablesen." came back as ten seconds of
//! audio with a word lost. A German date the splitter kept in its clause is
//! written out ("am 3. Oktober" → "am dritten Oktober", `ordinal`): the
//! second live run's voice read "3." as "drei" and went silent. Any other
//! "N." is left as written (fix package B6). An inline tag (`[laughs]`,
//! `(sighs)`, `*laughs*`) is none of this: it is hidden from every step
//! after the link pass and comes back canonical, `[laughs]` (`tagged`, WP10
//! D8). The cleaned text is the spoken transcript.
//!
//! It works per clause, so it knows only the clause: the numbering and the
//! fences it judges are the clause's own lines' (the clause splitter has
//! already dropped the markers and the code it saw). A clause that begins
//! mid-line, after a sentence end ([`super::Placed::line_start`]), has no
//! line start first: a number there is no marker but text, said — "1. FC
//! Köln ist abgestiegen.", an inline list's "1. Wasser holen." (R4 M2;
//! [`speakable_at`]). Each inline marker is
//! judged by its neighbours (CommonMark's "flanking" idea): a `*` between
//! spaces or digits
//! is arithmetic, an `_` inside a word is `snake_case`, and punctuation
//! other than parentheses is never touched.

use super::abbrev;
use super::blocks::{self, Fence, Marker, Numbering, Probe};
use super::ordinal;
use super::stops;
use super::tagged::{self, Hidden};

/// Strips markdown syntax from a clause so TTS does not read it out
/// (module doc); whitespace collapses to single spaces. Inline tags are kept,
/// canonical.
pub fn speakable(text: &str) -> String {
    speakable_at(text, true)
}

/// [`speakable`] of a clause whose text begins at a line start
/// (`line_start`), or mid-line after a sentence end — where a number first
/// in it is text, not a list marker (module doc, R4 M2).
pub fn speakable_at(text: &str, line_start: bool) -> String {
    // The text's own private-use characters go: a tag is hidden behind one.
    let text: String = text.chars().filter(|c| !tagged::private(*c)).collect();
    let mut tags = Hidden::default();
    let mut out = String::with_capacity(text.len());
    let mut numbering = Numbering::default();
    let mut fence: Option<Fence> = None;
    for (k, line) in text.lines().enumerate() {
        let numbered = line_start || k > 0;
        let t = line.trim_start();
        // Fence lines and the code between them say nothing.
        if let Some(f) = fence {
            if blocks::closes(t, f) {
                fence = None;
            }
            continue;
        }
        if let Probe::Fence(f) = blocks::fence_of_line(t) {
            fence = Some(f);
            continue;
        }
        if let Some(body) = line_body(t, line.len() > t.len(), numbered, &mut numbering) {
            // A link's text first (`[see here](url)` is no tag), then the
            // tags out of everything's way.
            out.push_str(&inline(&tags.hide(&links(body))));
            out.push(' ');
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    tags.restore(&parentheses(&abbrev::expand(&ordinal::spoken(&out))))
}

/// A thematic break: three or more of one of `-`, `*`, `_`, nothing else
/// but spaces and tabs (CommonMark).
fn is_rule(line: &str) -> bool {
    let marks: Vec<char> = line.chars().filter(|c| !matches!(c, ' ' | '\t')).collect();
    marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0])
}

/// Parentheses as commas, the way a voice sets an aside off: none at the
/// start, none doubled, none before or after other punctuation, and none
/// for a closing parenthesis that ends the clause. A clause without
/// parentheses is left exactly as it is.
fn parentheses(s: &str) -> String {
    if !s.contains(['(', ')']) {
        return s.to_string();
    }
    let stop = |c: char| {
        matches!(c, ',' | '.' | '!' | '?' | ';' | ':') || stops::spaced(c) || stops::wide(c)
    };
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = if matches!(c, '(' | ')') { ',' } else { c };
        if stop(c) {
            out.truncate(out.trim_end().len());
            if c == ',' {
                if out.is_empty() || out.ends_with(stop) {
                    out.push(' ');
                    continue;
                }
                out.push_str(", ");
                continue;
            }
            if out.ends_with(',') {
                out.pop();
            }
        }
        out.push(c);
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    match out.strip_suffix(',') {
        Some(rest) if s.trim_end().ends_with(')') => rest.to_string(),
        _ => out,
    }
}

/// A line (indentation skipped — `indented` says whether there was any)
/// without its block markers; `None` for a thematic break or a setext
/// underline. `numbered`: the line starts a line of the stream, so a number
/// first in it may be a marker; mid-line it is text (module doc).
fn line_body<'a>(
    line: &'a str,
    indented: bool,
    numbered: bool,
    numbering: &mut Numbering,
) -> Option<&'a str> {
    let mut t = line;
    if is_rule(t) || blocks::is_underline(t) {
        return None;
    }
    let hashes = t.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && t[hashes..].chars().next().is_none_or(char::is_whitespace) {
        t = closing_hashes(t[hashes..].trim());
    }
    // A bullet, then maybe a number ("- 1. …"); each only a marker when
    // text follows: a lone "1." is an answer.
    let mut item = false;
    for _ in 0..2 {
        match blocks::list_marker(t, *numbering, true, indented) {
            Marker::Len { len, number }
                if !t[len..].trim().is_empty() && (numbered || number.is_none()) =>
            {
                numbering.took(number, indented);
                item = true;
                t = t[len..].trim_start();
            }
            _ => break,
        }
    }
    if !item && !t.trim().is_empty() {
        numbering.text();
    }
    Some(t)
}

/// An ATX heading's text without its closing sequence: "Titel ##" is
/// "Titel" — a run of `#` at the end, after a space, or the whole text
/// (CommonMark). Not spoken as hashes (B2 review D).
fn closing_hashes(text: &str) -> &str {
    let body = text.trim_end_matches('#');
    if body.len() == text.len() {
        return text;
    }
    if body.is_empty() {
        return body;
    }
    if body.ends_with([' ', '\t']) {
        body.trim_end()
    } else {
        text
    }
}

/// `[text](target)` → `text` and `![alt](target)` → `alt`; anything that
/// is not a complete link stays as written.
fn links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let open = if s[i..].starts_with("![") {
            Some(i + 2)
        } else if s[i..].starts_with('[') {
            Some(i + 1)
        } else {
            None
        };
        if let Some((text, next)) = open.and_then(|t| link_at(s, t)) {
            out.push_str(text);
            i = next;
            continue;
        }
        // All delimiters are ASCII, so stepping a whole char is safe.
        let c = s[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// For a link text starting at `t`: the text and the index after `)`.
fn link_at(s: &str, t: usize) -> Option<(&str, usize)> {
    let close = t + s[t..].find(']')?;
    if s[t..close].contains('[') || !s[close + 1..].starts_with('(') {
        return None;
    }
    // Targets may hold balanced parentheses (Wikipedia URLs).
    let mut depth = 0usize;
    for (k, c) in s[close + 1..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[t..close], close + 1 + k + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// Drops backticks and emphasis runs (`*`, `_`, `~~`) that flank text.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            i += 1;
            continue;
        }
        if matches!(c, '*' | '_' | '~') {
            let run = chars[i..].iter().take_while(|&&x| x == c).count();
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + run).copied();
            let space = |x: Option<char>| x.is_none_or(char::is_whitespace);
            let both =
                |f: fn(&char) -> bool| prev.is_some_and(|p| f(&p)) && next.is_some_and(|n| f(&n));
            let keep = (space(prev) && space(next))
                || match c {
                    '*' => both(char::is_ascii_digit),
                    '_' => both(|x| x.is_alphanumeric()),
                    _ => run < 2,
                };
            if keep {
                out.extend(&chars[i..i + run]);
            }
            i += run;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}
