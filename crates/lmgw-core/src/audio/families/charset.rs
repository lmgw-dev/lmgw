//! Which text-to-speech engines refuse a character their package's
//! vocabulary lacks, read off audio.cpp's source (main at 75d0294,
//! 2026-10-08).
//!
//! Only **Supertonic** does. Its tokenizer rewrites a few characters, NFKD-
//! normalises the text and looks every codepoint up in the package's
//! `unicode_indexer`; one without an entry throws ("Supertonic unicode
//! indexer has no entry for codepoint 8222", `supertonic/tokenizer_text.cpp`
//! `encode`), and the request fails with a 500. The engines that also map
//! text characters to ids skip one they lack instead: MagpieTTS's Arabic
//! character tokenizers (`encode_char_tokens`), F5-TTS (id 0,
//! `f5_tts/synthesize.cpp`), ZipVoice and Piper (`emilia_tokenizer.cpp`,
//! `piper_tts/frontend.cpp`), and VoxCPM2 falls back to bytes
//! (`bpe_initial_pieces`). The rest tokenize with a BPE or SentencePiece
//! vocabulary, or phonemize first.

/// A family's character vocabulary: where its package keeps it, and what
/// its engine does to the text before the lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CharVocabulary {
    /// The spec's source key naming the vocabulary file (`unicode_indexer`,
    /// `model:config/unicode_indexer.json` in Supertonic 3's spec): which
    /// file of the package it is, the package's own spec says.
    pub resource: &'static str,
    /// The characters the engine rewrites before its lookup, and what into.
    /// They are never looked up as they are: a request may carry one
    /// whether the vocabulary has it or not, as long as it has what the
    /// engine writes instead (checked when the profile is computed,
    /// `crate::audio::charset::CharVocab::from_indexer`).
    pub rewrites: &'static [(char, &'static str)],
    /// What the engine writes into the text besides, whatever the request
    /// says: the outputs of its rewrites of longer strings, and the stop
    /// and the `<lang>…</lang>` wrapper around every text. A vocabulary
    /// without one of them fails every request (a note of the profile then, not a problem).
    pub adds: &'static [&'static str],
    /// The engine's own NFKD: what it decomposes before its lookup.
    pub nfkd: Nfkd,
}

/// An engine's compatibility decomposition, from a table generated into
/// it at one Unicode version: a codepoint the table has becomes its
/// decomposition, any other one is looked up as it is.
/// `unicode-normalization` decomposes at a newer version (pinned in
/// `crate::audio::charset`'s tests), so lmgw asks this table which
/// codepoints the engine decomposes and the crate only what into: a
/// decomposition never changes once its codepoint is assigned (Unicode's
/// normalization stability policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Nfkd {
    /// The Unicode version the table was generated with.
    pub unicode: (u8, u8, u8),
    /// The codepoints it decomposes, as sorted inclusive ranges.
    pub decomposed: &'static [(u32, u32)],
}

impl Nfkd {
    /// The engine decomposes `c`.
    pub fn decomposes(&self, c: char) -> bool {
        let cp = u32::from(c);
        self.decomposed
            .binary_search_by(|&(lo, hi)| {
                if hi < cp {
                    std::cmp::Ordering::Less
                } else if lo > cp {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
}

/// audio.cpp's NFKD table (`src/framework/text/unicode_nfkd_data.inc`),
/// which Supertonic's tokenizer applies (`normalize_nfkd_codepoints`).
mod nfkd;

/// The character vocabulary of `family`'s engine, when it refuses a
/// character the vocabulary lacks ([module doc](self)). `None`: the engine
/// takes any character, and a request's `input` is sent as it came.
pub(crate) fn char_vocabulary(family: &str) -> Option<CharVocabulary> {
    match family {
        // `preprocess`: `–`, `‑`, `—` to `-`; `_`, `[`, `]`, `|`, `/`, `#`,
        // `→`, `←` to a space; `“`, `”` to `"`; `‘`, `’`, `´`, `` ` `` to
        // `'`; `@` to ` at `; `e.g.,` to `for example, ` and `i.e.,` to
        // `that is, `; `♥`, `☆`, `♡`, `©`, `\` removed; runs of whitespace
        // (`std::regex` `\s` over bytes: space, tab, the line breaks,
        // vertical tab and form feed) to one space; a `.` after a text that
        // ends in no stop, and `<de>…</de>` around it.
        "supertonic" => Some(CharVocabulary {
            resource: "unicode_indexer",
            rewrites: &[
                ('\u{2013}', "-"),
                ('\u{2011}', "-"),
                ('\u{2014}', "-"),
                ('_', " "),
                ('\u{201C}', "\""),
                ('\u{201D}', "\""),
                ('\u{2018}', "'"),
                ('\u{2019}', "'"),
                ('\u{00B4}', "'"),
                ('`', "'"),
                ('[', " "),
                (']', " "),
                ('|', " "),
                ('/', " "),
                ('#', " "),
                ('\u{2192}', " "),
                ('\u{2190}', " "),
                ('@', " at "),
                ('\u{2665}', ""),
                ('\u{2606}', ""),
                ('\u{2661}', ""),
                ('\u{00A9}', ""),
                ('\\', ""),
                ('\t', " "),
                ('\n', " "),
                ('\u{0B}', " "),
                ('\u{0C}', " "),
                ('\r', " "),
            ],
            adds: &["for example, ", "that is, ", ".", "<", "/", ">"],
            nfkd: Nfkd {
                unicode: nfkd::UNICODE,
                decomposed: nfkd::DECOMPOSED,
            },
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supertonic_refuses_a_character_it_lacks() {
        let v = char_vocabulary("supertonic").unwrap();
        assert_eq!(v.resource, "unicode_indexer");
        let rewrites: Vec<char> = v.rewrites.iter().map(|(c, _)| *c).collect();
        // The German opening quote („, U+201E) is not among the engine's own
        // rewrites: a reply that uses it fails unless lmgw fits it.
        assert!(!rewrites.contains(&'\u{201E}'));
        assert!(rewrites.contains(&'\u{201C}') && rewrites.contains(&'@'));
        assert_eq!(rewrites.len(), 28);
        for family in [
            "kokoro_tts",
            "magpie_tts",
            "pocket_tts",
            "qwen3_tts",
            "f5_tts",
        ] {
            assert_eq!(char_vocabulary(family), None, "{family}");
        }
    }

    #[test]
    fn the_engine_decomposes_only_what_its_unicode_15_1_table_has() {
        let n = char_vocabulary("supertonic").unwrap().nfkd;
        assert_eq!(n.unicode, (15, 1, 0));
        let total: u32 = n.decomposed.iter().map(|(lo, hi)| hi - lo + 1).sum();
        assert_eq!(total, 17_029);
        assert!(n.decomposed.windows(2).all(|w| w[0].1 + 1 < w[1].0));
        // ä, the ellipsis, a no-break space, a Hangul syllable, the last
        // CJK compatibility ideograph.
        for c in ['ä', '…', '\u{00A0}', '\u{AC00}', '\u{2FA1D}'] {
            assert!(n.decomposes(c), "{c:?}");
        }
        // Plain ASCII, ß (no decomposition), and what Unicode 16 and 17
        // added: an outlined A, the modifier letter capital S.
        for c in ['a', '"', 'ß', '\u{1CCD6}', '\u{A7F1}'] {
            assert!(!n.decomposes(c), "{c:?}");
        }
    }
}
