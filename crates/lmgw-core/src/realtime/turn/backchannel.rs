//! Which transcripts are a backchannel (realtime design §6.4, the barge-in
//! word check): "mhm", "ja genau", "okay okay" — speech that tells the
//! speaker to go on, not to stop.
//!
//! The list is the owner's, visible and editable
//! (`realtime.backchannel_words`); this only matches against it. A
//! transcript is a backchannel when it says nothing at all, or when its
//! words are a sequence of list entries — an entry may be several words
//! ("ach so", "i see"), and entries may repeat ("ja ja"). Both sides are
//! normalized the same way: lowercase, punctuation gone, and a hyphen or an
//! apostrophe joins rather than splits ("Mm-hmm." is the entry "mm-hmm").
//! Anything else — one word off the list — is words: the cut.
//!
//! A word whose letters are all outside the session's scripts
//! (`realtime.barge_in_check_scripts`, `super::scripts`) is no word: ASR
//! models write noise as "嗯。" or "Угу.", so a transcript of only such
//! words is empty (E4). A token without letters — a number — is a word
//! (B4 review M3): "2." or "15:30" cut, and "5 mm" is no hum.
//!
//! **A hum is a backchannel** whatever its length (E6): a word made only of
//! the letters h and m — "hm", "mhmm", "hmmmm", "hmhm" — counts as a list
//! entry. qwen3-asr wrote a yawn as "Hmm." and then a degenerate
//! "Hmmmm…" repetition; no list can hold every length. "Hm, stopp" still
//! cuts: "stopp" is a word.

use super::scripts::counts_as_word;

/// Whether `transcript` is empty or nothing but entries of `words` (module
/// doc), every script counting as words.
pub fn is_backchannel(transcript: &str, words: &[String]) -> bool {
    is_backchannel_in(transcript, words, &[])
}

/// [`is_backchannel`], only words with a letter in `scripts` counting as
/// words — an empty list: every script (module doc).
pub fn is_backchannel_in(transcript: &str, words: &[String], scripts: &[String]) -> bool {
    let tokens = words_in(transcript, scripts);
    if tokens.is_empty() {
        return true;
    }
    let entries: Vec<Vec<String>> = words
        .iter()
        .map(|w| normalize(w))
        .filter(|e| !e.is_empty())
        .collect();
    // fits[i]: tokens[i..] is a sequence of entries (a hum is one).
    let n = tokens.len();
    let mut fits = vec![false; n + 1];
    fits[n] = true;
    for i in (0..n).rev() {
        fits[i] = (is_hum(&tokens[i]) && fits[i + 1])
            || entries
                .iter()
                .any(|e| tokens[i..].starts_with(e) && fits[i + e.len()]);
    }
    fits[0]
}

/// Whether `transcript` has any word that counts in `scripts` (module
/// doc): `false` for one the check heard nothing in.
pub fn has_words_in(transcript: &str, scripts: &[String]) -> bool {
    !words_in(transcript, scripts).is_empty()
}

/// The words of `transcript` that count in `scripts`, normalized.
fn words_in(transcript: &str, scripts: &[String]) -> Vec<String> {
    let mut tokens = normalize(transcript);
    tokens.retain(|t| counts_as_word(t, scripts));
    tokens
}

/// A word made only of the letters h and m (module doc).
fn is_hum(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| matches!(c, 'h' | 'm'))
}

/// `text` as lowercase words, punctuation dropped, hyphens and apostrophes
/// joining.
fn normalize(text: &str) -> Vec<String> {
    let mut joined = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            joined.extend(c.to_lowercase());
        } else if matches!(c, '-' | '‐' | '‑' | '\'' | '’') {
            // A joiner: "mm-hmm", "uh-huh", "that's".
        } else {
            joined.push(' ');
        }
    }
    joined.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<String> {
        crate::config::RealtimeSettings::default().backchannel_words
    }

    #[test]
    fn backchannels_in_both_languages_are_backchannels() {
        for t in [
            "",
            "  ",
            "...",
            "Mhm.",
            "Mm-hmm.",
            "mm hmm",
            "Ja, genau.",
            "Ja ja.",
            "Okay, okay.",
            "Ach so!",
            "Uh-huh.",
            "I see.",
            "Yeah, right.",
            "Haha",
            "OK.",
            "Gut.",
        ] {
            assert!(is_backchannel(t, &list()), "{t:?}");
        }
    }

    #[test]
    fn anything_else_is_words() {
        for t in [
            "Stopp.",
            "Stop!",
            "Mhm, aber warte mal.",
            "Ja, aber",
            "Okay, und morgen?",
            "Nein.",
            "Wait.",
            "ach",
            "I",
            "seeing",
            "Yes, please stop.",
        ] {
            assert!(!is_backchannel(t, &list()), "{t:?}");
        }
    }

    #[test]
    fn what_the_asr_models_heard_for_backchannels_is_one() {
        // E4: the offline replay of the owner's round-1 recordings.
        for t in [
            "Uh huh.",
            "Mhmm.",
            "Mmh.",
            "Hm hm.",
            "Hm-hm.",
            "Mm hm.",
            "Mmhm.",
            "Hmhm.",
            "So.",
            "Ach so.",
            "Achso.",
            "Jaja.",
            "O.K.",
            "Okey.",
            "Klar.",
            "Alles klar.",
            "Super!",
        ] {
            assert!(is_backchannel(t, &list()), "{t:?}");
        }
        let latin = vec!["Latin".to_string()];
        for noise in ["嗯。", "咳。", "Угу.", "Mhm 嗯"] {
            assert!(is_backchannel_in(noise, &list(), &latin), "{noise:?}");
            assert!(
                !is_backchannel(noise, &list()),
                "{noise:?}: every script counts"
            );
        }
        assert!(!is_backchannel_in("Stopp 嗯", &list(), &latin));
    }

    #[test]
    fn a_number_is_a_word_and_cuts() {
        // B4 review M3: "Zwei!" during playback, written as a digit, was no
        // word under the scripts rule and was lost; "5 mm" became a hum.
        let latin = vec!["Latin".to_string()];
        for t in ["2.", "15:30", "5 mm", "Um 3", "42 嗯"] {
            assert!(!is_backchannel_in(t, &list(), &latin), "{t:?}");
            assert!(has_words_in(t, &latin), "{t:?}");
        }
        for t in ["", "...", "嗯。", "Угу."] {
            assert!(!has_words_in(t, &latin), "{t:?}");
        }
        assert!(has_words_in("Mhm.", &latin), "a backchannel has words");
    }

    #[test]
    fn a_hum_of_any_length_is_a_backchannel_and_words_still_cut() {
        // E6: what qwen3-asr wrote for a yawn, and its degenerate repeats.
        for t in [
            "Hmm.",
            "Hmmmm…",
            "Mhmmm.",
            "Hmhm hm",
            "hmmmmmmmmmmmm",
            "Mh!",
            "Hm, ja.",
        ] {
            assert!(is_backchannel(t, &list()), "{t:?}");
        }
        for t in ["Hm, stopp.", "Hmm, warte.", "Ham", "Hmm, no."] {
            assert!(!is_backchannel(t, &list()), "{t:?}");
        }
        // The rule needs no list at all.
        assert!(is_backchannel("Hmmmm", &[]));
        // Round 2 of the owner's recordings (E6).
        for t in ["Okee.", "Uh.", "Huh?", "Uh hmm.", "A so.", "Ah so.", "Ah!"] {
            assert!(is_backchannel(t, &list()), "{t:?}");
        }
    }

    #[test]
    fn the_list_is_the_owner_s() {
        let only_ok = vec!["ok".to_string()];
        assert!(is_backchannel("OK!", &only_ok));
        assert!(!is_backchannel("Ja.", &only_ok));
        // Entries normalize as transcripts do; blank ones match nothing.
        let odd = vec!["  Mm-Hmm ".to_string(), "".into(), "!!".into()];
        assert!(is_backchannel("mm-hmm", &odd));
        assert!(!is_backchannel("hello", &odd));
    }
}
