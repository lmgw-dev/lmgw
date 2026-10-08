//! The characters a TTS engine can say, and an `input` fitted to them.
//!
//! Supertonic looks every codepoint of its text up in its package's
//! `unicode_indexer` and fails the whole request on one it has no entry
//! for: a German reply's `„` (U+201E), for one, fails the whole request with
//! "Supertonic unicode indexer has no entry for codepoint 8222". Which
//! engines do that, where their package keeps the vocabulary and what they
//! do to the text before the lookup is [`super::families::char_vocabulary`];
//! the vocabulary itself is read from the package the row serves
//! (`super::profile`), never assumed. Speech shaping ([`super::shape`])
//! then sends such a row only characters its engine says ([`fit`]), and
//! reports each one it replaced or dropped ([`named`]).

use std::collections::HashSet;

use serde_json::Value;
use unicode_normalization::char::decompose_compatible;

use super::families::{CharVocabulary, Nfkd};

mod fit;
pub use fit::{fit, replacements, Fitted};

mod named;
pub use named::named;

mod blank;

/// The characters an engine says ([module doc](self)).
#[derive(Clone, PartialEq, Eq)]
pub struct CharVocab {
    /// The package file it was read from (`config/unicode_indexer.json`).
    pub file: String,
    codepoints: HashSet<u32>,
    /// The characters the engine rewrites before its lookup into text this
    /// vocabulary has ([`CharVocabulary::rewrites`] without the ones whose
    /// output it lacks).
    rewrites: Vec<char>,
    /// The ones among `rewrites` that the engine writes as nothing or as
    /// white space ([`Self::blank_once_written`]).
    blank_rewrites: Vec<char>,
    /// What the engine decomposes before its lookup.
    nfkd: Nfkd,
}

impl std::fmt::Debug for CharVocab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CharVocab")
            .field("file", &self.file)
            .field("codepoints", &self.codepoints.len())
            .field("rewrites", &self.rewrites.len())
            .finish()
    }
}

impl CharVocab {
    /// The vocabulary from a Supertonic `unicode_indexer`, read as audio.cpp
    /// reads it (`supertonic/assets.cpp` `parse_unicode_indexer`): an array
    /// whose index is the codepoint, or an object keyed by the decimal
    /// codepoint, and a value of -1 (any negative) meaning no entry. `Err`
    /// for anything the engine itself would refuse to load. `engine` is
    /// what the family's engine does to the text before its lookup; a
    /// rewrite into text the vocabulary lacks does not count as said
    /// ([`Self::engine_misses`] says which).
    pub(crate) fn from_indexer(
        file: &str,
        raw: &[u8],
        engine: &CharVocabulary,
    ) -> Result<Self, String> {
        let root: Value = serde_json::from_slice(raw).map_err(|_| format!("{file}: not JSON"))?;
        let token = |v: &Value| {
            v.as_i64()
                .ok_or_else(|| format!("{file}: an entry is not an integer"))
        };
        let mut codepoints = HashSet::new();
        match &root {
            Value::Array(values) => {
                for (i, v) in values.iter().enumerate() {
                    if token(v)? >= 0 {
                        codepoints
                            .insert(u32::try_from(i).map_err(|_| format!("{file}: too long"))?);
                    }
                }
            }
            Value::Object(map) => {
                for (key, v) in map {
                    let cp: u32 = key
                        .parse()
                        .map_err(|_| format!("{file}: a key is not a codepoint: {key}"))?;
                    if token(v)? >= 0 {
                        codepoints.insert(cp);
                    }
                }
            }
            _ => return Err(format!("{file}: neither an array nor an object")),
        }
        if codepoints.is_empty() {
            return Err(format!("{file}: no entries"));
        }
        let mut v = Self {
            file: file.to_string(),
            codepoints,
            rewrites: Vec::new(),
            blank_rewrites: Vec::new(),
            nfkd: engine.nfkd,
        };
        v.rewrites = engine
            .rewrites
            .iter()
            .filter(|(_, out)| out.chars().all(|c| v.looks_up(c)))
            .map(|(c, _)| *c)
            .collect();
        v.blank_rewrites = engine
            .rewrites
            .iter()
            .filter(|(c, out)| out.trim().is_empty() && v.rewrites.contains(c))
            .map(|(c, _)| *c)
            .collect();
        Ok(v)
    }

    /// What `engine` writes into the text that this vocabulary lacks, for
    /// the profile's problems — at most two sentences: the rewrites whose
    /// output it lacks (those characters are then fitted like any other,
    /// not left to the engine), and the text the engine adds itself — its
    /// stop, the wrapper around the text, the code of each of `languages`
    /// in it — which no request can avoid.
    pub(crate) fn engine_misses(
        &self,
        engine: &CharVocabulary,
        languages: &[String],
    ) -> Vec<String> {
        let (mut rewrite_lacks, mut lacked) = (Vec::new(), Vec::new());
        let rewritten: Vec<u32> = engine
            .rewrites
            .iter()
            .filter(|(_, to)| self.lacking(to, &mut rewrite_lacks))
            .map(|(c, _)| u32::from(*c))
            .collect();
        let wrappers = languages.iter().map(|l| format!("<{l}>"));
        let added: Vec<String> = engine
            .adds
            .iter()
            .map(|a| a.to_string())
            .chain(wrappers)
            .filter(|a| self.lacking(a, &mut lacked))
            .map(|a| format!("{a:?}"))
            .collect();
        let mut out = Vec::new();
        if !rewritten.is_empty() {
            out.push(format!(
                "the engine rewrites {} before its lookup into text {} lacks ({}): lmgw fits \
                 them like characters it cannot say",
                named(&rewritten),
                self.file,
                named(&rewrite_lacks),
            ));
        }
        if !added.is_empty() {
            out.push(format!(
                "the engine adds {} to the text itself, and {} lacks {}: a request that gets one \
                 fails",
                added.join(", "),
                self.file,
                named(&lacked),
            ));
        }
        out
    }

    /// Whether the engine's lookup misses a character of `s`; each it
    /// misses is added to `into`, once.
    fn lacking(&self, s: &str, into: &mut Vec<u32>) -> bool {
        let mut any = false;
        for cp in s.chars().filter(|c| !self.looks_up(*c)).map(u32::from) {
            any = true;
            if !into.contains(&cp) {
                into.push(cp);
            }
        }
        any
    }

    /// How many codepoints have an entry.
    pub fn len(&self) -> usize {
        self.codepoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codepoints.is_empty()
    }

    /// The characters the engine rewrites before its lookup into text this
    /// vocabulary has: said whether it has them or not.
    pub fn rewrites(&self) -> &[char] {
        &self.rewrites
    }

    /// The engine says `c`: it rewrites `c` itself before its lookup into
    /// text the vocabulary has, or it looks `c` up and finds every codepoint
    /// ([`Self::looks_up`]).
    pub fn says(&self, c: char) -> bool {
        self.rewrites.contains(&c) || self.looks_up(c)
    }

    /// The engine's lookup of `c` after its rewrites: `c`'s compatibility
    /// decomposition when the engine's NFKD table has `c` (so `ä` is found
    /// when `a` and U+0308 are, and `…` when `.` is), else `c` itself — a
    /// codepoint newer than that table (an outlined `A`, U+1CCD6, Unicode
    /// 16) the engine looks up as it is, however `unicode-normalization`
    /// would decompose it.
    fn looks_up(&self, c: char) -> bool {
        if !self.nfkd.decomposes(c) {
            return self.codepoints.contains(&u32::from(c));
        }
        let mut all = true;
        decompose_compatible(c, |d| all &= self.codepoints.contains(&u32::from(d)));
        all
    }

    /// [`Self::says`] for every character of `s`.
    pub fn says_all(&self, s: &str) -> bool {
        s.chars().all(|c| self.says(c))
    }
}

#[cfg(test)]
mod tests;
