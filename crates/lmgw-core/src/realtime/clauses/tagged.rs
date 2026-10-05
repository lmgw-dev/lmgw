//! Inline tags in a spoken answer (realtime design §8.1; WP10 D8): a
//! `[laughs]` the model writes is a sound to make, never words to read, so
//! the clause splitter and the speakable pass keep it whole.
//!
//! - **The scan** ([`TagScan`]): a `[` may open a tag — up to 31 bytes of
//!   `[a-z _'-]` on the same line, starting with a letter, then `]`. The
//!   words inside are held back from the count: once the `]` comes they were
//!   no words, so a tag alone on a line is no clause. A candidate that turns
//!   out to be no tag gives its words back.
//! - **The speakable pass** ([`Hidden`]): after the link pass, every tag of
//!   the clause's grammar (`crate::audio::tags`) is hidden behind one
//!   private-use character and put back, canonical, at the end — so
//!   emphasis, abbreviations, ordinals and parentheses never touch it:
//!   `*laughs*` used to become the word "laughs", `(laughs)` ", laughs,".
//!   Stage directions come out in the canonical form (`(laughs)` →
//!   `[laughs]`); shaping maps them onto the TTS's own tags.
//! - **Two texts per clause** ([`Spoken`], D9): what the voice is sent,
//!   tags kept, and what was said — the transcript — without them: the
//!   listener heard a laugh, not the word. An inline list's number opening
//!   the clause is sent without its dot and said with it (R5 F2).

use crate::audio::tags::{self, TagMode};

/// What a tag is hidden behind in the speakable pass: the first character of
/// Unicode's private-use area, which no text means anything by. Every tag
/// gets the same one and they are put back in order — nothing in the pass
/// drops or reorders a character that is no letter, space or markdown
/// marker — so a clause may hold any number of tags.
const HIDDEN: char = '\u{E000}';

/// A character of the Basic Multilingual Plane's private-use area. The
/// text's own are dropped before tags are hidden, so none is mistaken for
/// one.
pub(super) fn private(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
}

/// The longest tag name, in bytes — the canonical grammar's
/// (`crate::audio::tags`).
const MAX_NAME: usize = 31;

/// The clause splitter's view of a `[` that may open a tag (module doc).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TagScan {
    /// The candidate open now: the bytes of its name so far, and the words
    /// in it, held back from the count.
    open: Option<(usize, usize)>,
}

impl TagScan {
    /// Feed one character, `starts_word` when it starts a word. Answers
    /// how many words to count for it: its own, unless a tag holds it back,
    /// plus the ones a candidate that died here held.
    pub fn feed(&mut self, c: char, starts_word: bool) -> usize {
        let own = usize::from(starts_word);
        let Some((len, held)) = self.open else {
            if c == '[' {
                self.open = Some((0, 0));
            }
            return own;
        };
        let name_char = if len == 0 {
            c.is_ascii_lowercase()
        } else {
            c.is_ascii_lowercase() || matches!(c, ' ' | '_' | '\'' | '-')
        };
        if c == ']' && len > 0 {
            // A tag: its words were none.
            self.open = None;
            return 0;
        }
        if name_char && len + c.len_utf8() <= MAX_NAME {
            self.open = Some((len + c.len_utf8(), held + own));
            return 0;
        }
        // No tag after all: its words count, and this character may open
        // the next one.
        self.open = (c == '[').then_some((0, 0));
        held + own
    }
}

/// The tags of one clause, hidden from the speakable pass (module doc).
#[derive(Debug, Default)]
pub(super) struct Hidden {
    /// Their names, canonical, in order.
    names: Vec<String>,
}

impl Hidden {
    /// `text` with every tag in it hidden.
    pub fn hide(&mut self, text: &str) -> String {
        let spans = tags::spans(text);
        if spans.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for s in spans {
            out.push_str(&text[at..s.start]);
            out.push(HIDDEN);
            self.names.push(s.name);
            at = s.end;
        }
        out.push_str(&text[at..]);
        out
    }

    /// `text` with the tags back, each as `[name]`.
    pub fn restore(&self, text: &str) -> String {
        if self.names.is_empty() {
            return text.to_string();
        }
        let mut names = self.names.iter();
        let mut out = String::with_capacity(text.len() + self.names.len() * 8);
        for c in text.chars() {
            if c != HIDDEN {
                out.push(c);
                continue;
            }
            // One per tag hidden; nothing in between makes or drops one.
            if let Some(name) = names.next() {
                out.push('[');
                out.push_str(name);
                out.push(']');
            }
        }
        debug_assert!(names.next().is_none(), "a hidden tag went missing");
        out
    }
}

/// A clause's two texts (module doc, WP10 D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    /// What the TTS is sent: the speakable text, inline tags kept in the
    /// canonical form, which shaping maps, keeps or strips per route.
    pub tts: String,
    /// What was said — the transcript: the same text without its tags.
    pub said: String,
}

impl Spoken {
    /// The clause as the splitter gave it: a number first in one that
    /// begins mid-line is said (`Placed::line_start`, R4 M2) — an inline
    /// list's without its dot to the TTS, with it in the transcript
    /// (`Placed::inline_item`, R5 F2).
    pub fn of(clause: &super::Placed) -> Self {
        Self::after("", clause)
    }

    /// [`Self::of`] after `carry`, the tags of clauses before it that said
    /// nothing (`responder::speech`): sent first, and not said.
    pub fn after(carry: &str, clause: &super::Placed) -> Self {
        let text = super::speakable_at(&clause.text, clause.line_start);
        let joined = |text: &str| match carry {
            "" => text.to_string(),
            carry => format!("{carry} {text}"),
        };
        let mut spoken = Self::from_tts(joined(&text));
        if clause.inline_item {
            spoken.tts = joined(&super::unstopped_number(&text));
        }
        spoken
    }

    /// The texts of `tts`, a speakable clause.
    pub fn from_tts(tts: String) -> Self {
        // Stripped as for a TTS that renders none: "Hello [laughs]." is
        // "Hello.", the space before the tag gone with it.
        let Some((said, _)) = tags::shape_tags(&tts, TagMode::None, &[]) else {
            return Self {
                said: tts.clone(),
                tts,
            };
        };
        // "[laughs], ja." is said "ja.": what punctuated a leading tag goes
        // with it.
        let leading = tags::spans(&tts).first().is_some_and(|t| t.start == 0);
        let said = if leading {
            said.trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, ',' | ';' | ':' | '.')
            })
            .to_string()
        } else {
            said
        };
        Self { tts, said }
    }

    /// The clause is tags and nothing else to say (`audio::tags::only_tags`):
    /// no TTS can speak it (WP10 D10).
    pub fn only_tags(&self) -> bool {
        tags::only_tags(&self.tts)
    }

    /// Its tags, canonical, one space apart.
    pub fn tags(&self) -> String {
        tags::spans(&self.tts)
            .iter()
            .map(|t| format!("[{}]", t.name))
            .collect::<Vec<_>>()
            .join(" ")
    }
}
