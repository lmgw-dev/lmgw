//! Heard, not generated (realtime §7.3).
//!
//! The history must match what the user heard, which is why OpenAI's
//! `conversation.item.truncate {audio_end_ms}` exists. An earlier prototype
//! counted dispatched clauses; here every assistant audio item keeps
//! a table of `(clause text, first sample, end sample)`, counted
//! cumulatively at the output rate after resampling, so a truncate maps a
//! time straight to text:
//! - clauses that end at or before the cut are kept whole;
//! - the clause the cut falls into is cut by character, interpolated
//!   linearly over its samples (characters, never bytes, so UTF-8 is never
//!   split), and then back to the end of the last word heard whole
//!   (`words.rs`, WP9 review B): "… bestimmte Lichtw" is "… bestimmte". The
//!   clause's audio still ends at the cut; only the text it is given moves.
//!   A cut inside its first word leaves nothing of it, and what was written
//!   before it goes with it, as with a clause none of which was heard;
//! - later clauses are dropped;
//! - a cut beyond the item's length is an error, as with OpenAI. The length
//!   is rounded up to whole milliseconds, so a client that rounds up the
//!   duration it played is not refused.
//!
//! The table also keeps what the model **wrote** for each clause (`written`):
//! the transcript is what was said, but the model's history is its own text —
//! code and markdown the voice left out included — up to the same cut.

use std::fmt;

mod words;
mod written;

pub use written::Written;

/// One synthesized clause and where its audio sits in the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeardClause {
    /// What was said: the clause after the speakable pass, cut to the words
    /// heard whole when only part of it was heard.
    pub text: String,
    pub first_sample: u64,
    /// One past the last sample.
    pub end_sample: u64,
    /// What the model wrote for it ([`Written`]).
    pub written: Written,
    /// Only part of the clause was heard: `text` is that part.
    pub partial: bool,
}

/// Errors for the heard table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeardError {
    /// The output rate must not be zero.
    ZeroRate,
    /// `audio_end_ms` is past the item's audio.
    BeyondEnd { audio_end_ms: u64, total_ms: u64 },
}

impl fmt::Display for HeardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRate => write!(f, "output sample rate must not be zero"),
            Self::BeyondEnd {
                audio_end_ms,
                total_ms,
            } => write!(
                f,
                "audio_end_ms {audio_end_ms} is beyond the item's audio ({total_ms} ms)"
            ),
        }
    }
}

impl std::error::Error for HeardError {}

/// The clause table of one assistant audio item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeardTable {
    rate: u64,
    clauses: Vec<HeardClause>,
    total: u64,
    /// What the model wrote after the last clause and never said — a code
    /// block that ends the answer ([`Self::push_unspoken`]).
    tail: String,
    /// Audio was cut away: what the model wrote after the cut is not part
    /// of what was heard, the tail included.
    clipped: bool,
}

impl HeardTable {
    /// An empty item at the session's output rate.
    pub fn new(rate: u32) -> Result<Self, HeardError> {
        if rate == 0 {
            return Err(HeardError::ZeroRate);
        }
        Ok(Self {
            rate: u64::from(rate),
            clauses: Vec::new(),
            total: 0,
            tail: String::new(),
            clipped: false,
        })
    }

    /// Records a clause whose audio, `n_samples` long at the output rate,
    /// follows everything pushed so far — written as it was said.
    pub fn push_clause(&mut self, text: impl Into<String>, n_samples: u64) {
        let text = text.into();
        let before = if self.clauses.is_empty() { "" } else { " " };
        let written = Written::said(before, &text);
        self.push_written(text, written, n_samples);
    }

    /// [`Self::push_clause`] for a clause the model wrote as `written`.
    /// Unspoken text pushed since the clause before — what the model wrote
    /// before a tool call, when more clauses follow it — goes before this
    /// one, in the order it was written (`written`).
    pub fn push_written(&mut self, text: impl Into<String>, mut written: Written, n_samples: u64) {
        if !self.tail.is_empty() {
            written
                .before
                .insert_str(0, &std::mem::take(&mut self.tail));
        }
        let first = self.total;
        self.total = self.total.saturating_add(n_samples);
        self.clauses.push(HeardClause {
            text: text.into(),
            first_sample: first,
            end_sample: self.total,
            written,
            partial: false,
        });
    }

    /// Text the model wrote after the last clause that is not said (a code
    /// block that ends the answer, or text before a tool call): part of its
    /// history unless audio is cut away before it — and before the next
    /// clause if one follows.
    pub fn push_unspoken(&mut self, raw: &str) {
        self.tail.push_str(raw);
    }

    pub fn clauses(&self) -> &[HeardClause] {
        &self.clauses
    }

    /// Whether audio was cut away: a truncate or a cancel kept less than
    /// the item had.
    pub fn clipped(&self) -> bool {
        self.clipped
    }

    pub fn total_samples(&self) -> u64 {
        self.total
    }

    /// The item's length in milliseconds, rounded up.
    pub fn total_ms(&self) -> u64 {
        (u128::from(self.total) * 1000).div_ceil(u128::from(self.rate)) as u64
    }

    /// The whole transcript (what was generated and queued).
    pub fn text(&self) -> String {
        join(self.clauses.iter().map(|c| c.text.as_str()))
    }

    /// The text heard up to `audio_end_ms`, without changing the table.
    pub fn heard(&self, audio_end_ms: u64) -> Result<String, HeardError> {
        let (whole, partial) = self.cut(self.cut_sample(audio_end_ms)?);
        let texts = self.clauses[..whole].iter().map(|c| c.text.as_str());
        Ok(join(texts.chain(partial.as_deref())))
    }

    /// `conversation.item.truncate`: keeps only what was heard, makes it the
    /// item's audio, and returns the heard transcript.
    pub fn truncate(&mut self, audio_end_ms: u64) -> Result<String, HeardError> {
        let cut = self.cut_sample(audio_end_ms)?;
        Ok(self.keep(cut))
    }

    /// Keeps the first `samples` of the item's audio — what left for the
    /// listener when a response is cancelled (§7.3) — cut like a truncate,
    /// and returns the transcript. More than the item holds keeps it whole.
    pub fn keep(&mut self, samples: u64) -> String {
        let cut = samples.min(self.total);
        let (whole, partial) = self.cut(cut);
        let next = self.clauses.get(whole).cloned();
        let after = self
            .clauses
            .get(whole + 1)
            .map(|c| c.written.before.clone());
        self.clauses.truncate(whole);
        match (partial, next) {
            (Some(text), Some(c)) => {
                // Every word of it heard — the cut fell in its last
                // character or its trailing silence — is a clause heard
                // whole: `written()` gives the model's own text for it,
                // not the said one (WP11 binding review m2). `end_sample`
                // and `clipped` still say where the audio was cut.
                let partial = text.trim_end() != c.text.trim_end();
                let mut written = c.written;
                // An announcement heard to its last word: its block was
                // heard, as for a cut right after it (below).
                if let (false, true, Some(after)) = (partial, written.announcement, after) {
                    written.own.push_str(&after);
                }
                self.clauses.push(HeardClause {
                    text,
                    first_sample: c.first_sample,
                    end_sample: cut,
                    written,
                    partial,
                })
            }
            // Cut right after an announcement heard whole: its block was
            // heard, and the clause cut away carried it (`written`).
            (None, Some(c)) => {
                if let Some(a) = self
                    .clauses
                    .last_mut()
                    .filter(|a| a.written.announcement && !a.partial)
                {
                    a.written.own.push_str(&c.written.before);
                }
            }
            _ => {}
        }
        self.clipped |= cut < self.total;
        self.total = cut;
        self.text()
    }

    /// `audio_end_ms` as a sample index — or the error for a cut beyond the
    /// item's audio.
    fn cut_sample(&self, audio_end_ms: u64) -> Result<u64, HeardError> {
        let total_ms = self.total_ms();
        if audio_end_ms > total_ms {
            return Err(HeardError::BeyondEnd {
                audio_end_ms,
                total_ms,
            });
        }
        let cut = u128::from(audio_end_ms) * u128::from(self.rate) / 1000;
        Ok(cut.min(u128::from(self.total)) as u64)
    }

    /// The number of whole clauses before sample `cut` and the partial
    /// clause's text.
    fn cut(&self, cut: u64) -> (usize, Option<String>) {
        let whole = self
            .clauses
            .iter()
            .take_while(|c| c.end_sample <= cut)
            .count();
        let partial = self
            .clauses
            .get(whole)
            .filter(|c| c.first_sample < cut)
            .map(|c| {
                let done = cut - c.first_sample;
                let span = c.end_sample - c.first_sample;
                let n = c.text.chars().count();
                let keep = (n as u128 * u128::from(done) / u128::from(span)) as usize;
                words::heard_words(&c.text, keep).to_string()
            })
            .filter(|t| !t.trim().is_empty());
        (whole, partial)
    }
}

/// Combining diacritical marks: part of the letter before them.
fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

fn join<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    let parts: Vec<&str> = texts.map(str::trim).filter(|t| !t.is_empty()).collect();
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Hallo Jürgen," for 1 s, then "schön dich zu hören." for 2 s, 24 kHz.
    fn table() -> HeardTable {
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_clause("Hallo Jürgen,", 24_000);
        t.push_clause("schön dich zu hören.", 48_000);
        t
    }

    #[test]
    fn cuts_at_zero_mid_and_end() {
        let t = table();
        assert_eq!(t.total_ms(), 3000);
        assert_eq!(t.heard(0).unwrap(), "");
        assert_eq!(t.heard(1000).unwrap(), "Hallo Jürgen,", "a clause boundary");
        // Half of 13 characters: 6, "Hallo " trimmed.
        assert_eq!(t.heard(500).unwrap(), "Hallo");
        assert_eq!(t.heard(2000).unwrap(), "Hallo Jürgen, schön dich");
        assert_eq!(t.heard(3000).unwrap(), t.text());
    }

    #[test]
    fn a_cut_inside_a_word_keeps_the_words_before_it() {
        let t = table();
        // 400 ms into a 2 s clause of 20 characters is 4 characters, "schö"
        // (5 bytes, so a byte cut would have split the "ö"): inside the
        // clause's first word, so nothing of it was heard.
        assert_eq!(t.heard(1400).unwrap(), "Hallo Jürgen,");
        // 1.4 s into it: 14 characters, "schön dich zu " and no "hören".
        assert_eq!(t.heard(2400).unwrap(), "Hallo Jürgen, schön dich zu");
        // Inside the first clause's second word: its first word.
        assert_eq!(t.heard(700).unwrap(), "Hallo");
        let mut d = HeardTable::new(24_000).unwrap();
        d.push_clause("u\u{308}ber alles", 24_000);
        // The bare "u" of "über" is no word heard; "über " is.
        assert_eq!(d.heard(100).unwrap(), "");
        assert_eq!(d.heard(600).unwrap(), "u\u{308}ber");
    }

    #[test]
    fn a_compound_cut_inside_is_cut_before_it() {
        // WP9 review B: the live drive stored "… bestimmte Lichtw".
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_clause("Blau streut bestimmte Lichtwellenlängen stärker.", 48_000);
        // 1.4 s of 2 s: 33 of 48 characters, inside the compound.
        assert_eq!(t.heard(1400).unwrap(), "Blau streut bestimmte");
        let kept = t.truncate(1400).unwrap();
        assert_eq!(kept, "Blau streut bestimmte");
        let c = &t.clauses()[0];
        assert!(c.partial);
        assert_eq!(
            (c.end_sample, t.total_ms()),
            (33_600, 1400),
            "the audio still ends at the cut"
        );
        assert_eq!(t.written(), "Blau streut bestimmte");
    }

    #[test]
    fn a_clause_heard_to_its_last_word_is_heard_whole() {
        // WP11 binding review m2: 14 of 15 characters of a clause said
        // otherwise than written — every word heard, the punctuation
        // absorbed — is no partial clause: the history keeps what the model
        // wrote for it.
        use super::written::Written;
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Oh.", Written::said("", "Oh."), 24_000);
        t.push_written(
            "Das ist lustig.",
            Written::said(" ", "Das ist *laughs* lustig."),
            24_000,
        );
        t.push_written("Fertig.", Written::said(" ", "Fertig."), 24_000);
        // 1.95 s: 0.95 of the second clause, 14 of its 15 characters.
        t.keep(46_800);
        let c = t.clauses().last().unwrap();
        assert!(!c.partial, "{c:?}");
        assert_eq!(c.end_sample, 46_800, "the audio still ends at the cut");
        assert!(t.clipped());
        assert_eq!(t.written(), "Oh. Das ist *laughs* lustig.");
        assert_eq!(t.written_whole(), "Oh. Das ist *laughs* lustig.");
        // One word short is partial, as before.
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written(
            "Das ist lustig.",
            Written::said("", "Das ist *laughs* lustig."),
            24_000,
        );
        t.keep(15_000);
        assert!(t.clauses()[0].partial);
    }

    #[test]
    fn an_announcement_heard_to_its_last_word_keeps_its_block() {
        use super::written::Written;
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Hier:", Written::said("", "Hier:"), 24_000);
        t.push_written("Codeblock, rust.", Written::announcing("\n"), 24_000);
        t.push_written(
            "Fertig.",
            Written::said("```rust\nfn main() {}\n```\n", "Fertig."),
            24_000,
        );
        // Inside the announcement's trailing silence: every word heard.
        t.keep(47_900);
        assert_eq!(t.written(), "Hier:\n```rust\nfn main() {}\n```");
    }

    #[test]
    fn han_cuts_per_character() {
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_clause("今日は晴れです。", 24_000);
        // Half of 8 characters: 4, each its own word.
        assert_eq!(t.heard(500).unwrap(), "今日は晴");
    }

    #[test]
    fn beyond_the_end_is_an_error() {
        let t = table();
        assert_eq!(
            t.heard(3001),
            Err(HeardError::BeyondEnd {
                audio_end_ms: 3001,
                total_ms: 3000
            })
        );
        assert!(t.heard(u64::MAX).is_err(), "no overflow");
        // 1000 samples are 41.67 ms: a client rounding up to 42 is fine.
        let mut r = HeardTable::new(24_000).unwrap();
        r.push_clause("Ja.", 1000);
        assert_eq!(r.heard(42).unwrap(), "Ja.");
        assert!(r.heard(43).is_err());
        assert_eq!(HeardTable::new(0), Err(HeardError::ZeroRate));
    }

    #[test]
    fn truncate_keeps_only_what_was_heard() {
        let mut t = table();
        assert_eq!(t.truncate(2000).unwrap(), "Hallo Jürgen, schön dich");
        assert_eq!((t.total_ms(), t.clauses().len()), (2000, 2));
        assert_eq!(t.clauses()[1].end_sample, 48_000);
        assert!(t.heard(2500).is_err(), "the dropped audio is gone");
        assert_eq!(t.truncate(500).unwrap(), "Hallo");
        assert_eq!(t.truncate(0).unwrap(), "");
        assert!(t.clauses().is_empty());
    }

    #[test]
    fn keep_cuts_at_a_sample_and_never_beyond() {
        let mut t = table();
        // 36 000 samples: the first clause, and a quarter of the second
        // (5 of 20 characters: "schön", whole).
        assert_eq!(t.keep(36_000), "Hallo Jürgen, schön");
        assert_eq!(t.total_samples(), 36_000);
        assert_eq!(
            t.keep(u64::MAX),
            "Hallo Jürgen, schön",
            "nothing grows back"
        );
        assert_eq!(t.keep(0), "");
    }

    #[test]
    fn silent_clauses_and_empty_items() {
        let mut t = HeardTable::new(24_000).unwrap();
        assert_eq!(t.heard(0).unwrap(), "");
        t.push_clause("Eins.", 0);
        t.push_clause("Zwei.", 2400);
        // A clause with no audio sits at its position: heard once reached.
        assert_eq!(t.heard(0).unwrap(), "Eins.");
        assert_eq!(t.heard(100).unwrap(), "Eins. Zwei.");
    }
}
