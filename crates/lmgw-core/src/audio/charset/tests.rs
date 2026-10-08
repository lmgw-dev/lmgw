//! The vocabulary and [`fit`], on a trimmed copy of the real Supertonic 3
//! `unicode_indexer` (`tests/fixtures/audio/supertonic3_unicode_indexer_trimmed.json`:
//! the package's `config/unicode_indexer.json` below U+3000, in the object
//! form audio.cpp also reads, token ids as published) and on small
//! synthetic ones.

use unicode_normalization::char::decompose_compatible;

use super::*;
use crate::audio::families::char_vocabulary;

const TRIMMED: &str =
    include_str!("../../../tests/fixtures/audio/supertonic3_unicode_indexer_trimmed.json");

fn engine() -> CharVocabulary {
    char_vocabulary("supertonic").unwrap()
}

/// Supertonic's engine without its rewrites and additions: what a
/// synthetic vocabulary is read with.
fn bare() -> CharVocabulary {
    CharVocabulary {
        rewrites: &[],
        adds: &[],
        ..engine()
    }
}

fn supertonic() -> CharVocab {
    CharVocab::from_indexer("config/unicode_indexer.json", TRIMMED.as_bytes(), &engine()).unwrap()
}

/// The array form of a vocabulary of exactly `chars`.
fn table(chars: &str) -> Vec<u8> {
    let mut table = vec![-1i64; 0x3000];
    for (i, c) in chars.chars().enumerate() {
        table[c as usize] = i as i64;
    }
    serde_json::to_vec(&table).unwrap()
}

/// A vocabulary of exactly `chars`, with no rewrites.
fn only(chars: &str) -> CharVocab {
    CharVocab::from_indexer("t.json", &table(chars), &bare()).unwrap()
}

#[test]
fn the_real_vocabulary_lacks_the_german_opening_quote_and_has_the_ascii_one() {
    let v = supertonic();
    assert_eq!(v.len(), 661);
    // The root cause: U+201E is neither in the indexer nor rewritten by
    // the engine, so the engine threw for it.
    assert!(!v.says('„'));
    assert!(v.says('"') && v.says('\'') && v.says('-') && v.says('.'));
    // The engine rewrites these before its lookup, which has no entry for
    // them: said all the same.
    for c in ['“', '”', '‘', '’', '–', '—', '@', '#', '_', '[', '|', '\n'] {
        assert!(v.says(c), "{c:?}");
    }
    // NFKD first: ä is a and U+0308, … is three dots, a no-break space a
    // space.
    for c in [
        'ä', 'Ö', 'ß', 'é', '…', '\u{00A0}', '\u{202F}', '«', '»', '€',
    ] {
        assert!(v.says(c), "{c:?}");
    }
    // Nothing past the Basic Multilingual Plane is in an array of 65536.
    assert!(!v.says('😊'));
}

#[test]
fn a_german_reply_is_fitted_and_every_change_is_named() {
    let v = supertonic();
    let input = "„Gern“ – sagte er… „Bis bald!“ 😊";
    let f = fit(input, &v).unwrap();
    assert_eq!(f.text, "\"Gern“ – sagte er… \"Bis bald!“ ");
    assert_eq!(f.replaced, [0x201E], "each codepoint once");
    assert_eq!(f.dropped, [0x1F60A]);
    assert!(v.says_all(&f.text));

    // A text the engine says whole goes as it came.
    assert_eq!(fit("Schöne Grüße — bis gleich…", &v), None);
}

#[test]
fn quotes_dashes_and_spaces_become_their_plain_equivalents() {
    let v = only(" '\".-ab");
    let f = fit("„a“ ‚b‘ a–b a—b a−b a\u{2009}b a\u{2028}b", &v).unwrap();
    assert_eq!(f.text, "\"a\" 'b' a-b a-b a-b a\u{2009}b a b");
    assert_eq!(
        f.replaced,
        [0x201E, 0x201C, 0x201A, 0x2018, 0x2013, 0x2014, 0x2212, 0x2028]
    );
    assert!(f.dropped.is_empty());
    // A thin space is said as it is: NFKD makes it a space. A line
    // separator has no decomposition.
    assert!(v.says('\u{2009}') && !v.says('\u{2028}'));

    // The next best when the first is missing: no `"` here.
    let v = only(" '.a");
    assert_eq!(fit("„a“", &v).unwrap().text, "'a'");
}

#[test]
fn an_ellipsis_the_engine_cannot_say_is_dropped_never_failed() {
    // NFKD makes `…` three dots: with a dot it is said as it is.
    assert_eq!(fit("a…", &only("a.")), None);
    // Without one, nothing stands for it.
    let f = fit("a…", &only("a")).unwrap();
    assert_eq!(
        (f.text.as_str(), f.dropped.as_slice()),
        ("a", &[0x2026][..])
    );
}

#[test]
fn a_letter_whose_mark_is_missing_keeps_its_base_letter() {
    // ő is o and the double acute U+030B; ä is a and U+0308.
    let v = only("ao\u{0308} ");
    let f = fit("ő ä", &v).unwrap();
    assert_eq!(f.text, "o ä");
    assert_eq!(f.replaced, [0x0151]);
    // A base letter it lacks too: dropped.
    let f = fit("ű", &v).unwrap();
    assert_eq!((f.text.as_str(), f.dropped.as_slice()), ("", &[0x0171][..]));
}

#[test]
fn what_nothing_stands_for_is_dropped() {
    let v = supertonic();
    let f = fit("Ok 👍🏽 ✓ gut", &v).unwrap();
    assert_eq!(f.text, "Ok   gut");
    assert_eq!(f.dropped, [0x1F44D, 0x1F3FD, 0x2713]);
    assert!(f.replaced.is_empty());
}

#[test]
fn the_indexer_is_read_as_audio_cpp_reads_it() {
    let array = CharVocab::from_indexer("a", b"[-1, 0, -1, 7]", &bare()).unwrap();
    assert!(array.says('\u{1}') && array.says('\u{3}') && !array.says('\u{2}'));
    let object = CharVocab::from_indexer("o", br#"{"65": 0, "66": -1}"#, &bare()).unwrap();
    assert!(object.says('A') && !object.says('B'));
    assert_eq!(object.len(), 1);
    for bad in [
        &b"nope"[..],
        b"[]",
        b"[-1]",
        b"{\"x\": 1}",
        b"[\"a\"]",
        b"3",
    ] {
        assert!(
            CharVocab::from_indexer("b", bad, &bare()).is_err(),
            "{bad:?}"
        );
    }
    // Debug names the file and counts, not 661 codepoints.
    assert_eq!(
        format!("{:?}", supertonic()),
        "CharVocab { file: \"config/unicode_indexer.json\", codepoints: 661, rewrites: 28 }"
    );
}

#[test]
fn the_capital_sharp_s_is_said_as_the_small_one_else_as_ss() {
    let f = fit("GROẞ", &supertonic()).unwrap();
    assert_eq!(
        (f.text.as_str(), f.replaced.as_slice()),
        ("GROß", &[0x1E9E][..])
    );
    assert_eq!(fit("GROẞ", &only("GROS")).unwrap().text, "GROSS");
}

/// What `fit` puts in a character's place is all in [`replacements`],
/// which the live probe sends a real engine.
#[test]
fn every_replacement_is_one_the_live_probe_sends() {
    for c in (0..=0x10FFFF).filter_map(char::from_u32) {
        for e in fit::equivalents(c) {
            assert!(replacements().contains(e), "{c:?} -> {e:?}");
        }
    }
    let v = supertonic();
    for r in replacements() {
        assert!(v.says_all(r), "{r:?}");
    }
}

/// The crate's Unicode version. Its NFKD decomposes what Unicode added
/// after the engine's table (15.1) as well, which the engine looks up as
/// it is: lmgw asks the engine's table which codepoints it decomposes
/// (`families::Nfkd`). A bump of the crate fails here; the scan below then
/// says what the gap is now.
#[test]
fn unicode_normalization_is_pinned_at_unicode_17() {
    assert_eq!(unicode_normalization::UNICODE_VERSION, (17, 0, 0));
}

/// Every codepoint whose decomposition the crate and the engine disagree
/// on: all assigned after Unicode 15.1, and none of them is decomposed by
/// the engine. 37 of them decompose into letters Supertonic 3 has (the
/// modifier capital S, the outlined A to Z and 0 to 9): the crate's NFKD
/// would have kept them, and the engine threw for each (review TC-2).
#[test]
fn the_crate_decomposes_57_codepoints_the_engine_s_table_does_not() {
    let nfkd = engine().nfkd;
    let mut newer = Vec::new();
    for c in (0..=0x10FFFF).filter_map(char::from_u32) {
        let mut d = Vec::new();
        decompose_compatible(c, |x| d.push(x));
        let crate_decomposes = d != [c];
        if nfkd.decomposes(c) {
            assert!(
                crate_decomposes,
                "the engine decomposes {c:?}, the crate does not"
            );
        } else if crate_decomposes {
            newer.push(u32::from(c));
        }
    }
    let ranges = |cps: &[u32]| {
        let mut r: Vec<(u32, u32)> = Vec::new();
        for &cp in cps {
            match r.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => r.push((cp, cp)),
            }
        }
        r
    };
    assert_eq!(ranges(&newer), NEWER, "{newer:X?}");

    let v = supertonic();
    let kept: Vec<u32> = newer
        .iter()
        .filter(|&&cp| {
            let mut all = true;
            decompose_compatible(char::from_u32(cp).unwrap(), |d| all &= v.says(d));
            all
        })
        .copied()
        .collect();
    assert_eq!(kept.len(), 37, "{kept:X?}");
    // Never said as they are; their plain form goes instead.
    for &cp in &kept {
        assert!(!v.says(char::from_u32(cp).unwrap()), "U+{cp:04X}");
    }
    let f = fit("\u{1CCD6}\u{1CCF9} \u{A7F1}", &v).unwrap();
    assert_eq!(f.text, "A9 S");
    assert_eq!(f.replaced, [0x1CCD6, 0x1CCF9, 0xA7F1]);
}

/// What Unicode 16 and 17 added with a decomposition: the modifier capital
/// S (U+A7F1); Todhri, Tulu-Tigalari, Gurung Khema and Kirat Rai letters
/// and vowel signs; the outlined A to Z and 0 to 9.
const NEWER: &[(u32, u32)] = &[
    (0xA7F1, 0xA7F1),
    (0x105C9, 0x105C9),
    (0x105E4, 0x105E4),
    (0x11383, 0x11383),
    (0x11385, 0x11385),
    (0x1138E, 0x1138E),
    (0x11391, 0x11391),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113C8),
    (0x16121, 0x16128),
    (0x16D68, 0x16D6A),
    (0x1CCD6, 0x1CCF9),
];

#[test]
fn what_the_engine_writes_itself_is_checked_against_the_vocabulary() {
    // Every rewrite's output and every addition is in Supertonic 3's.
    let v = supertonic();
    let de = ["de".to_string(), "en".to_string()];
    assert_eq!(v.engine_misses(&engine(), &de), Vec::<String>::new());
    assert_eq!(v.rewrites().len(), 28);

    // A vocabulary without a space or a `t`: `@` (to " at ") and `_` (to
    // a space) are not left to the engine, and its stop is fine.
    let raw = table("abcdefghijklmnopqrsuvwxyz<>/.-\"',");
    let v = CharVocab::from_indexer("small.json", &raw, &engine()).unwrap();
    assert!(!v.says('@') && !v.says('_') && v.says('–') && v.says('“'));
    let misses = v.engine_misses(&engine(), &de);
    assert_eq!(
        misses,
        [
            "the engine rewrites U+005F _, U+005B [, U+005D ], U+007C |, U+002F /, U+0023 #, \
             U+2192 \u{2192}, U+2190 \u{2190}, U+0040 @, U+0009, U+000A, U+000B, U+000C, U+000D \
             before its lookup into text small.json lacks (U+0020, U+0074 t): lmgw fits them \
             like characters it cannot say",
            "the engine adds \"for example, \", \"that is, \" to the text itself, and \
             small.json lacks U+0020, U+0074 t: a request that gets one fails",
        ]
    );
    // A language whose code it lacks.
    let misses = v.engine_misses(&engine(), &["tr".to_string()]);
    assert!(
        misses[1].contains("\"<tr>\"")
            && misses[1].ends_with("lacks U+0020, U+0074 t: a request that gets one fails"),
        "{misses:?}"
    );
    // `@` is fitted like any other character it lacks: dropped here.
    let f = fit("a@b", &v).unwrap();
    assert_eq!((f.text.as_str(), f.dropped.as_slice()), ("ab", &[0x40][..]));
}
