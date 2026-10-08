//! An input with nothing left to say once the characters its engine cannot
//! say are fitted out (review TC-1).
//!
//! Supertonic refuses an empty text ("Supertonic requires --text input",
//! `supertonic/session.cpp` `validate_request`) and says a blip for a blank
//! one, so a reply's lone `😊` clause, which shaping
//! ([`crate::audio::charset::fit`]) empties, failed the voice turn all the
//! same. It is `empty_input` instead, like an input of nothing but inline
//! tags: `POST /v1/audio/speech` answers 400 naming the characters, and a
//! realtime answer or a Chat read-aloud skips the clause and goes on.

use crate::audio::charset::{named, CharVocab};
use crate::audio::shape::{ShapeChange, ShapeReport};
use crate::error::GatewayError;

/// `Err` (`empty_input`) when nothing is left of `text`, the input as
/// shaped, to speak. On a row with a `vocab` that is whenever the engine
/// would write it blank (`blank_once_written`: empty, white space, `♥`
/// written as nothing, `#` as a space), whether or not shaping changed
/// anything. On a row without one it is a text shaping replaced or dropped
/// characters of (`report`) and left empty or blank; a text that came blank
/// is the engine's own to judge there ([`super::refuse_speech`] has already
/// refused an empty one on every row).
pub fn refuse_unsayable(
    text: Option<&str>,
    report: &ShapeReport,
    model: &str,
    vocab: Option<&CharVocab>,
) -> Result<(), GatewayError> {
    let blank = |t: &str| t.trim().is_empty() || vocab.is_some_and(|v| v.blank_once_written(t));
    let changed = report.changes.iter().find_map(|c| match c {
        ShapeChange::Chars { replaced, dropped } => Some((replaced, dropped)),
        _ => None,
    });
    let refuse = match changed {
        Some(_) => !text.is_some_and(|t| !blank(t)),
        None => vocab.is_some() && text.is_some_and(blank),
    };
    if !refuse {
        return Ok(());
    }
    let message = match changed {
        Some((replaced, dropped)) => {
            let cps: Vec<u32> = dropped.iter().chain(replaced).copied().collect();
            format!(
                "input holds only characters '{model}' cannot say ({}): its engine has no entry \
                 for them in its package's vocabulary, so nothing is left to speak",
                named(&cps)
            )
        }
        None if text.is_some_and(|t| t.trim().is_empty()) => {
            format!("input is empty: '{model}' has nothing to speak")
        }
        None => format!(
            "input holds only characters '{model}' writes as nothing or as white space: nothing \
             is left to speak"
        ),
    };
    Err(GatewayError::InvalidRequest {
        code: "empty_input",
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(replaced: &[u32], dropped: &[u32]) -> ShapeReport {
        ShapeReport {
            changes: vec![ShapeChange::Chars {
                replaced: replaced.to_vec(),
                dropped: dropped.to_vec(),
            }],
        }
    }

    fn code(r: Result<(), GatewayError>) -> Option<&'static str> {
        match r {
            Err(GatewayError::InvalidRequest { code, .. }) => Some(code),
            _ => None,
        }
    }

    #[test]
    fn an_input_shaping_emptied_or_blanked_is_empty_input() {
        let lone = chars(&[], &[0x1F60A]);
        let e = refuse_unsayable(Some(""), &lone, "st", None).unwrap_err();
        assert_eq!(
            e.to_string(),
            "input holds only characters 'st' cannot say (U+1F60A \u{1F60A}): its engine has \
             no entry for them in its package's vocabulary, so nothing is left to speak"
        );
        assert_eq!(
            code(refuse_unsayable(Some(" \n "), &lone, "st", None)),
            Some("empty_input")
        );
        // A line separator it lacks became a space: nothing left either.
        let blank = chars(&[0x2028], &[0x1F44D, 0x1F3FD]);
        let e = refuse_unsayable(Some("  "), &blank, "st", None).unwrap_err();
        assert!(
            e.to_string()
                .contains("(U+1F44D \u{1F44D}, U+1F3FD \u{1F3FD}, U+2028)"),
            "{e}"
        );
    }

    fn real() -> CharVocab {
        let raw =
            include_str!("../../../tests/fixtures/audio/supertonic3_unicode_indexer_trimmed.json");
        let engine = crate::audio::families::char_vocabulary("supertonic").unwrap();
        CharVocab::from_indexer("config/unicode_indexer.json", raw.as_bytes(), &engine).unwrap()
    }

    #[test]
    fn a_text_the_engine_s_own_rewrites_blank_is_empty_input() {
        let v = real();
        let dropped = chars(&[], &[0x1F60A]);
        // `♥😊` fitted to `♥` (the engine writes nothing for it), a keycap
        // `#\u{FE0F}\u{20E3}` fitted to `#` (a space), `→ 😊` to `→ `.
        for fitted in ["\u{2665}", "#", "\u{2192} ", "_ [|] \\"] {
            assert_eq!(
                code(refuse_unsayable(Some(fitted), &dropped, "st", Some(&v))),
                Some("empty_input"),
                "{fitted:?}"
            );
        }
        // Said text with them still goes, and so does a text they did not
        // come from.
        for fitted in ["\u{2665} Danke", "a#", "#1"] {
            assert_eq!(
                code(refuse_unsayable(Some(fitted), &dropped, "st", Some(&v))),
                None,
                "{fitted:?}"
            );
        }
        // Nothing fitted: a lone `♥` is blank all the same.
        assert_eq!(
            code(refuse_unsayable(
                Some("\u{2665}"),
                &ShapeReport::default(),
                "st",
                Some(&v)
            )),
            Some("empty_input")
        );
        // Without the vocabulary the rewrites are unknown.
        assert_eq!(
            code(refuse_unsayable(Some("\u{2665}"), &dropped, "st", None)),
            None
        );
    }

    #[test]
    fn an_empty_input_of_a_row_with_a_vocabulary_is_empty_input() {
        let e =
            refuse_unsayable(Some(""), &ShapeReport::default(), "st", Some(&real())).unwrap_err();
        assert_eq!(e.to_string(), "input is empty: 'st' has nothing to speak");
    }

    #[test]
    fn a_blank_input_of_a_row_with_a_vocabulary_is_empty_input_too() {
        for blank in [" ", "\u{2665}"] {
            assert_eq!(
                code(refuse_unsayable(
                    Some(blank),
                    &ShapeReport::default(),
                    "st",
                    Some(&real())
                )),
                Some("empty_input"),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn something_left_or_nothing_shaped_passes() {
        let lone = chars(&[], &[0x1F60A]);
        assert_eq!(
            code(refuse_unsayable(Some("Gern. "), &lone, "st", None)),
            None
        );
        // Blank as it came on a row without a vocabulary, or shaped for its
        // tags only: not this check's.
        assert_eq!(
            code(refuse_unsayable(
                Some(""),
                &ShapeReport::default(),
                "st",
                None
            )),
            None
        );
        let tags = ShapeReport {
            changes: vec![ShapeChange::Tags {
                mapped: 0,
                stripped: 1,
            }],
        };
        assert_eq!(code(refuse_unsayable(Some(" "), &tags, "st", None)), None);
    }
}
