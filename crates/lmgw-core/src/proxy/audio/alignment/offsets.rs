//! Where each aligned word sits in the transcript the client submitted.
//!
//! Families spell their words differently — CrisperWhisper keeps the comma
//! on `Nuova,`, Qwen3's aligner strips it, and a model may fold case — so
//! the match compares letters and digits only, case-insensitively, and the
//! same rule runs for every family:
//!
//! - both sides are NFC-normalised and fully case-folded ([`stream`]), so
//!   `für` typed as `u` plus a combining diaeresis finds the precomposed `ü`
//!   a model reports, `STRASSE` finds `Straße`, and `ΟΔΟΣ` finds `οδος`;
//! - both sides are cut into tokens, the runs of letters and digits, and the
//!   two token lists are diffed ([`anchors`]). A model word whose tokens
//!   equal neighbouring tokens of the transcript sits exactly there: the
//!   words are matched as a sequence, so a word the transcript lacks, or one
//!   spelled out differently (`Dr.` against `doctor`), costs only itself and
//!   a later repeat of its text is not mistaken for it;
//! - the words the diff did not place are matched letter by letter inside
//!   the stretch of transcript between the anchored words around them
//!   ([`gap`]), which finds the ones a model split or merged (`can't` as
//!   `can` and `t`) and the words of a script without spaces, where the
//!   whole sentence is one token;
//! - the span runs from the word's first matched letter or digit to its
//!   last, so punctuation the word carries does not widen it, and the
//!   punctuation between words belongs to none;
//! - a word the transcript does not hold gets no span;
//! - offsets count Unicode scalar values (`char`s) of the text as submitted,
//!   half-open: `[start, end)`. Case folding can change the length of what is
//!   compared (`ß` is two letters), never the offsets: every compared
//!   letter points back at the chars it came from.

mod anchors;
mod gap;
mod stream;

use stream::Stream;

/// The `[start, end)` char span of each of `words` in `text`, `None` for a
/// word that has no letters or digits or is not found (module doc).
pub(super) fn locate(text: &str, words: &[&str]) -> Vec<Option<(usize, usize)>> {
    let hay = Stream::new(text);
    let hay_tokens = hay.tokens();
    let streams: Vec<Stream> = words.iter().map(|w| Stream::new(w)).collect();
    let anchored = anchors::anchors(&hay, &hay_tokens, &streams);

    let mut out = vec![None; words.len()];
    // The first unit of the transcript no anchored word has used.
    let mut from = 0;
    let mut n = 0;
    while n < words.len() {
        if let Some(tokens) = &anchored[n] {
            let (first, last) = (&hay_tokens[tokens.start], &hay_tokens[tokens.end - 1]);
            out[n] = Some((hay.units[first.start].from, hay.units[last.end - 1].to));
            from = last.end;
            n += 1;
            continue;
        }
        // The words up to the next anchored one go to the stretch of
        // transcript before that anchor.
        let next = (n..words.len())
            .find(|&k| anchored[k].is_some())
            .unwrap_or(words.len());
        let to = anchored
            .get(next)
            .and_then(|a| a.as_ref())
            .map_or(hay.units.len(), |t| hay_tokens[t.start].start);
        let group: Vec<&Stream> = streams[n..next].iter().collect();
        out[n..next].copy_from_slice(&gap::locate_in_gap(&hay.units, from, to, &group));
        n = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use unicode_normalization::UnicodeNormalization;

    use super::*;

    /// The spans cut out of `text`, as strings.
    fn cut(text: &str, words: &[&str]) -> Vec<Option<String>> {
        let chars: Vec<char> = text.chars().collect();
        locate(text, words)
            .into_iter()
            .map(|s| s.map(|(a, b)| chars[a..b].iter().collect()))
            .collect()
    }

    const TEXT: &str = "Throughout the Vita Nuova, there is a strain.";

    #[test]
    fn a_word_with_its_punctuation_and_one_without_cover_the_same_span() {
        let kept = locate(TEXT, &["Throughout", "the", "Vita", "Nuova,", "there"]);
        let stripped = locate(TEXT, &["Throughout", "the", "Vita", "Nuova", "there"]);
        assert_eq!(kept, stripped);
        assert_eq!(kept[3], Some((20, 25)));
        assert_eq!(
            cut(TEXT, &["Nuova,", "strain"]),
            [Some("Nuova".into()), Some("strain".into())]
        );
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(
            cut(TEXT, &["THROUGHOUT", "vita", "NUOVA"]),
            [
                Some("Throughout".into()),
                Some("Vita".into()),
                Some("Nuova".into())
            ]
        );
    }

    #[test]
    fn umlauts_match_precomposed_and_decomposed() {
        let precomposed = "Das ist für dich, Müller.";
        let decomposed = "Das ist fu\u{308}r dich, Mu\u{308}ller.";
        for text in [precomposed, decomposed] {
            for word in ["für", "fu\u{308}r"] {
                let span = locate(text, &["Das", "ist", word, "dich", "Müller."]);
                let cuts = cut(text, &["Das", "ist", word, "dich", "Müller."]);
                assert!(span.iter().all(Option::is_some), "{text:?} {word:?}");
                // The span of the word is the whole run, mark included.
                assert_eq!(
                    cuts[2].as_deref().map(|s| s.nfc().collect::<String>()),
                    Some("für".into())
                );
                assert_eq!(
                    cuts[4].as_deref().map(|s| s.nfc().collect::<String>()),
                    Some("Müller".into())
                );
            }
        }
    }

    #[test]
    fn offsets_count_chars_not_bytes() {
        // `ü` is two bytes, the combining diaeresis another two.
        let span = locate("für Ünal und dich", &["Ünal", "dich"]);
        assert_eq!(span, [Some((4, 8)), Some((13, 17))]);
        let span = locate("fu\u{308}r dich", &["dich"]);
        assert_eq!(span, [Some((5, 9))], "the mark is a char of its own");
    }

    #[test]
    fn a_word_the_transcript_lacks_gets_no_span_and_the_next_one_goes_on() {
        // "banana" is nowhere; "is" and "strain" are found from where "there"
        // ended.
        assert_eq!(
            cut(TEXT, &["there", "banana", "is", "strain"]),
            [
                Some("there".into()),
                None,
                Some("is".into()),
                Some("strain".into())
            ]
        );
    }

    #[test]
    fn a_word_that_is_only_punctuation_has_no_span() {
        assert_eq!(locate(TEXT, &["—", "the"])[0], None);
        assert!(locate(TEXT, &["—", "the"])[1].is_some());
    }

    #[test]
    fn a_repeated_word_is_found_where_the_last_match_ended() {
        let text = "no, no, no way";
        assert_eq!(
            locate(text, &["no", "no", "no", "way", "no"]),
            [
                Some((0, 2)),
                Some((4, 6)),
                Some((8, 10)),
                Some((11, 14)),
                None
            ]
        );
    }

    #[test]
    fn a_word_is_not_looked_for_before_the_previous_one() {
        // Out of order: "the" comes before "Throughout" in the answer, so it
        // is searched for after it and finds no later "the".
        let got = locate("Throughout the sea", &["Throughout", "the", "Throughout"]);
        assert_eq!(got, [Some((0, 10)), Some((11, 14)), None]);
    }

    #[test]
    fn text_without_script_spaces_matches_by_letters() {
        let text = "今日は、いい天気です。";
        assert_eq!(
            locate(text, &["今日は", "いい", "天気", "です"]),
            [Some((0, 3)), Some((4, 6)), Some((6, 8)), Some((8, 10))]
        );
    }

    #[test]
    fn an_empty_transcript_holds_nothing() {
        assert_eq!(locate("", &["a"]), [None]);
        assert_eq!(locate("", &[]), []);
    }

    #[test]
    fn a_false_match_far_ahead_does_not_swallow_the_words_between() {
        // "doctor" is spoken where the transcript has "Dr."; the later
        // "doctor" must not be taken for it.
        let text = "Dr. Who arrived. The doctor said hi.";
        let words = ["doctor", "Who", "arrived", "The", "doctor", "said", "hi"];
        assert_eq!(
            cut(text, &words),
            [
                None,
                Some("Who".into()),
                Some("arrived".into()),
                Some("The".into()),
                Some("doctor".into()),
                Some("said".into()),
                Some("hi".into())
            ]
        );
        assert_eq!(locate(text, &words)[4], Some((21, 27)));
    }

    #[test]
    fn a_one_letter_word_does_not_land_inside_a_later_word() {
        // "a" is nowhere in the transcript's own words; the banana holds
        // three of them.
        let text = "The banana split is yellow";
        assert_eq!(
            cut(text, &["The", "a", "is", "yellow"]),
            [
                Some("The".into()),
                None,
                Some("is".into()),
                Some("yellow".into())
            ]
        );
        // The same with nothing anchored after it.
        assert_eq!(cut("The banana", &["The", "a"]), [Some("The".into()), None]);
        let twice = locate("What I did", &["I", "I"]);
        assert_eq!(twice.iter().flatten().count(), 1, "one I in the transcript");
    }

    #[test]
    fn a_word_missing_in_the_middle_costs_only_itself() {
        assert_eq!(
            cut("one two three four", &["one", "three", "four"]),
            [
                Some("one".into()),
                Some("three".into()),
                Some("four".into())
            ]
        );
        assert_eq!(
            cut("one two three four", &["one", "zero", "three", "four"]),
            [
                Some("one".into()),
                None,
                Some("three".into()),
                Some("four".into())
            ]
        );
    }

    #[test]
    fn a_split_or_merged_word_is_found_in_its_gap() {
        // The model split a word the transcript keeps whole...
        assert_eq!(
            cut("I cant stay", &["I", "can", "t", "stay"]),
            [
                Some("I".into()),
                Some("can".into()),
                Some("t".into()),
                Some("stay".into())
            ]
        );
        // ...or kept whole one the transcript splits.
        let text = "I can't stay";
        assert_eq!(
            cut(text, &["I", "cant", "stay"]),
            [Some("I".into()), Some("can't".into()), Some("stay".into())]
        );
        // Words the diff does equal keep their exact token spans.
        assert_eq!(
            cut(text, &["I", "can't", "stay"]),
            [Some("I".into()), Some("can't".into()), Some("stay".into())]
        );
    }

    #[test]
    fn unspaced_script_with_a_missing_word_still_matches() {
        let text = "今日は、いい天気です。";
        assert_eq!(
            locate(text, &["今日は", "天気", "です"]),
            [Some((0, 3)), Some((6, 8)), Some((8, 10))]
        );
    }

    #[test]
    fn full_case_folding_meets_sharp_s_and_final_sigma() {
        assert_eq!(
            cut("Die STRASSE ist lang", &["Straße"])[0],
            Some("STRASSE".into())
        );
        assert_eq!(
            cut("Die Straße ist lang", &["STRASSE"])[0],
            Some("Straße".into())
        );
        assert_eq!(cut("die strasse", &["Straße"])[0], Some("strasse".into()));
        // ς, σ and Σ are one letter.
        for text in ["ΟΔΟΣ", "οδος", "οδοσ"] {
            for word in ["ΟΔΟΣ", "οδος", "οδοσ"] {
                assert_eq!(locate(text, &[word]), [Some((0, 4))], "{text} {word}");
            }
        }
        // İ meets its own decomposed spelling, and the plain i.
        assert_eq!(locate("İstanbul", &["i\u{307}stanbul"]), [Some((0, 8))]);
        assert_eq!(locate("i\u{307}stanbul", &["İSTANBUL"]), [Some((0, 9))]);
        assert_eq!(locate("İstanbul", &["Istanbul"]), [Some((0, 8))]);
    }

    #[test]
    fn folding_that_changes_the_length_keeps_offsets_in_the_original() {
        // "ß" is one char of the text and two letters compared: the words
        // after it keep their positions, and a word may end between the two
        // letters it folds to without moving the span off the char.
        let text = "Straße und STRASSE ok";
        assert_eq!(
            locate(text, &["Strasse", "und", "straße", "ok"]),
            [Some((0, 6)), Some((7, 10)), Some((11, 18)), Some((19, 21))]
        );
        assert_eq!(locate("Straße", &["Stra"]), [Some((0, 4))]);
        assert_eq!(locate("Straße", &["ss"]), [Some((4, 5))]);
        assert_eq!(locate("Straße", &["Straßen"]), [None]);
    }

    #[test]
    fn a_long_transcript_that_matches_stays_fast() {
        // 200k words, one in 200 dropped from the answer and one in 997
        // replaced: the diff of near-identical sequences costs about their
        // length plus the square of the edits (this is a debug build; the
        // bound is for a loaded machine, a release build takes a fraction).
        let transcript: String = (0..200_000)
            .map(|i| format!("w{i}{}", if i % 11 == 10 { ". " } else { " " }))
            .collect();
        let words: Vec<String> = (0..200_000)
            .filter(|i| i % 200 != 3)
            .map(|i| {
                if i % 997 == 0 {
                    "other".into()
                } else {
                    format!("w{i},")
                }
            })
            .collect();
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let started = std::time::Instant::now();
        let got = locate(&transcript, &refs);
        assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
        let others = words.iter().filter(|w| *w == "other").count();
        assert_eq!(got.iter().flatten().count(), words.len() - others);
        let chars: Vec<char> = transcript.chars().collect();
        let (from, to) = got[4].unwrap();
        assert_eq!(chars[from..to].iter().collect::<String>(), "w5");
    }
}
