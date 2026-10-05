//! What a provider's refusal of an off says the model takes instead — pure,
//! on the refusal's message (module doc of [`super`]).
//!
//! The messages this reads, as the providers wrote them (probed 2026-10-04):
//!
//! - OpenAI, a model without reasoning: "Unrecognized request argument
//!   supplied: reasoning_effort" — the control itself is refused;
//! - OpenAI, a model that cannot stop: "Unsupported value: 'reasoning_effort'
//!   does not support 'none' with this model. Supported values are:
//!   'minimal', 'low', 'medium', and 'high'." — the list is the answer;
//! - Gemini, a level below the model's least: "Thinking level MINIMAL is not
//!   supported for this model. Please retry with other thinking level.";
//! - Gemini 2.5, which has no levels at all: "Thinking level is not supported
//!   for this model." — its off is the budget form.

use crate::catalog::CANONICAL_LEVELS;
use crate::config::Protocol;

use super::Off;

/// How a retry after a refused off was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Retry {
    /// What the refusal's message says the model takes ([`after_refusal`]).
    Guided,
    /// The last resort: no reasoning control at all, so the model runs with
    /// reasoning as it does by default (the owner's ruling of 2026-10-04: a
    /// model that cannot run without reasoning runs with it, not into an
    /// error).
    Fallback,
}

/// The next form to send after `sent` was refused with `msg` (a `400` or
/// `422`): the guided form while `guided_left` and the message names the
/// control and says what instead, else no control at all — whatever the
/// message says, because only the retry can tell whether the off was what
/// was refused: a refusal about something else is refused again, and that
/// answer stands. `None` once no control went out: nothing of the off is
/// left to drop.
pub(super) fn next_after_refusal(
    protocol: Protocol,
    sent: &Off,
    msg: &str,
    guided_left: bool,
) -> Option<(Off, Retry)> {
    if *sent == Off::Omitted {
        return None;
    }
    if guided_left {
        if let Some(instead) = after_refusal(protocol, sent, msg) {
            return Some((instead, Retry::Guided));
        }
    }
    Some((Off::Omitted, Retry::Fallback))
}

/// Whether a refusal's message is about the prompt not fitting the model's
/// context — the one refusal common enough, and plainly about something
/// else, that retrying it without the off would only send the prompt again.
pub(super) fn about_the_context(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "context size",
        "maximum context",
        "prompt is too long",
    ]
    .iter()
    .any(|n| m.contains(n))
}

/// What to send instead of `sent` after a refusal whose message is `msg` —
/// `None` when the message does not name the reasoning control (the refusal
/// is about something else and stands), or nothing different is left to
/// try.
pub(super) fn after_refusal(protocol: Protocol, sent: &Off, msg: &str) -> Option<Off> {
    if *sent == Off::Omitted || !names_control(protocol, msg) {
        return None;
    }
    let instead = if let Some(level) = lowest_supported(msg) {
        Off::Lowest(level)
    } else if let Off::Lowest(level) = sent {
        if names_word(msg, level) {
            // That level is below the model's least: the next one up.
            next_level(level).map_or(Off::Omitted, |l| Off::Lowest(l.to_string()))
        } else if protocol == Protocol::Gemini {
            // The level control itself is refused: Gemini's other spelling
            // of off, the budget, is what such a model (2.5) takes.
            Off::Control
        } else {
            Off::Omitted
        }
    } else {
        Off::Omitted
    };
    (instead != *sent).then_some(instead)
}

/// Whether `msg` is about the reasoning control `protocol` sends.
fn names_control(protocol: Protocol, msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    match protocol {
        Protocol::Openai => ["reasoning_effort", "reasoning effort", "reasoning.effort"]
            .iter()
            .any(|n| m.contains(n)),
        // `thinkingConfig`, `thinkingLevel`, `thinkingBudget`, "thinking
        // level", and Anthropic's `thinking` / `output_config.effort`.
        Protocol::Gemini => m.contains("thinking"),
        Protocol::Anthropic => m.contains("thinking") || m.contains("effort"),
    }
}

/// The least of the values a "Supported values are: …" list names, in the
/// canonical order (a value outside it ranks last, in the order given);
/// `none` is not a level.
fn lowest_supported(msg: &str) -> Option<String> {
    let lower = msg.to_ascii_lowercase();
    let at = lower.find("supported values are")?;
    let list = &msg[at..];
    let mut values = Vec::new();
    let mut rest = list;
    while let Some(open) = rest.find(['\'', '"']) {
        let quote = rest[open..].chars().next()?;
        let after = &rest[open + 1..];
        let Some(close) = after.find(quote) else {
            break;
        };
        let v = after[..close].trim();
        if !v.is_empty() && !v.eq_ignore_ascii_case("none") {
            values.push(v.to_ascii_lowercase());
        }
        rest = &after[close + 1..];
    }
    let rank = |v: &String| {
        CANONICAL_LEVELS
            .iter()
            .position(|c| *c == v.as_str())
            .unwrap_or(usize::MAX)
    };
    values
        .iter()
        .enumerate()
        .min_by_key(|(i, v)| (rank(v), *i))
        .map(|(_, v)| v.clone())
}

/// `word` in `msg` on its own, case aside (`MINIMAL` in "Thinking level
/// MINIMAL is not supported", not `low` in "allowed").
fn names_word(msg: &str, word: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    let w = word.to_ascii_lowercase();
    let edge = |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
    m.match_indices(&w)
        .any(|(i, _)| edge(m[..i].chars().next_back()) && edge(m[i + w.len()..].chars().next()))
}

/// The canonical level above `level`, `None` at the top or for a level
/// outside the canonical order.
fn next_level(level: &str) -> Option<&'static str> {
    let i = CANONICAL_LEVELS
        .iter()
        .position(|c| c.eq_ignore_ascii_case(level))?;
    CANONICAL_LEVELS.get(i + 1).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lowest(l: &str) -> Off {
        Off::Lowest(l.to_string())
    }

    const OPENAI_NO_PARAM: &str =
        "upstream error (400): Unrecognized request argument supplied: reasoning_effort";
    const OPENAI_NO_NONE: &str = "upstream error (400): Unsupported value: 'reasoning_effort' \
        does not support 'none' with this model. Supported values are: 'minimal', 'low', \
        'medium', and 'high'.";
    const GEMINI_NO_MINIMAL: &str = "upstream error (400): Thinking level MINIMAL is not \
        supported for this model. Please retry with other thinking level.";
    const GEMINI_NO_LEVELS: &str =
        "upstream error (400): Thinking level is not supported for this model.";

    #[test]
    fn openai_without_the_control_gets_none() {
        assert_eq!(
            after_refusal(Protocol::Openai, &Off::Control, OPENAI_NO_PARAM),
            Some(Off::Omitted)
        );
    }

    #[test]
    fn openai_listing_its_values_gets_the_least() {
        assert_eq!(
            after_refusal(Protocol::Openai, &Off::Control, OPENAI_NO_NONE),
            Some(lowest("minimal"))
        );
        // o3: no `minimal` either.
        let o3 = "Unsupported value: 'reasoning_effort' does not support 'none' with this \
                  model. Supported values are: 'high', 'low', and 'medium'.";
        assert_eq!(
            after_refusal(Protocol::Openai, &Off::Control, o3),
            Some(lowest("low"))
        );
    }

    #[test]
    fn gemini_below_its_least_level_goes_one_up() {
        assert_eq!(
            after_refusal(Protocol::Gemini, &lowest("minimal"), GEMINI_NO_MINIMAL),
            Some(lowest("low"))
        );
        // The top of the order has nothing above it.
        let top = "Thinking level MAX is not supported for this model.";
        assert_eq!(
            after_refusal(Protocol::Gemini, &lowest("max"), top),
            Some(Off::Omitted)
        );
    }

    #[test]
    fn gemini_without_levels_gets_its_budget_off() {
        assert_eq!(
            after_refusal(Protocol::Gemini, &lowest("minimal"), GEMINI_NO_LEVELS),
            Some(Off::Control)
        );
    }

    #[test]
    fn anthropic_refusing_disabled_gets_none() {
        let msg = "thinking.type: Input tag 'disabled' found using 'type' does not match any \
                   of the expected tags";
        assert_eq!(
            after_refusal(Protocol::Anthropic, &Off::Control, msg),
            Some(Off::Omitted)
        );
    }

    #[test]
    fn a_refusal_about_something_else_stands() {
        // `gemini-flash-lite-latest` on `thinkingBudget: 0`: names nothing.
        let bare = "upstream error (400): Request contains an invalid argument.";
        assert_eq!(
            after_refusal(Protocol::Gemini, &lowest("minimal"), bare),
            None
        );
        let other = "Unsupported parameter: 'max_tokens' is not supported with this model.";
        assert_eq!(after_refusal(Protocol::Openai, &Off::Control, other), None);
        // Nothing is left to drop once no control went out.
        assert_eq!(
            after_refusal(Protocol::Openai, &Off::Omitted, OPENAI_NO_PARAM),
            None
        );
    }

    #[test]
    fn the_same_form_is_never_retried() {
        // Gemini refusing its budget off with the level named nowhere: the
        // other spelling is the one just refused.
        assert_eq!(
            after_refusal(Protocol::Gemini, &Off::Control, GEMINI_NO_LEVELS),
            Some(Off::Omitted)
        );
        let again = "Unsupported value: 'reasoning_effort' does not support 'minimal'. \
                     Supported values are: 'minimal'.";
        assert_eq!(
            after_refusal(Protocol::Openai, &lowest("minimal"), again),
            None
        );
    }

    #[test]
    fn the_last_resort_is_no_control_whatever_the_refusal_says() {
        use Retry::{Fallback, Guided};
        // Guided first, while the one guided retry is left.
        assert_eq!(
            next_after_refusal(Protocol::Openai, &Off::Control, OPENAI_NO_NONE, true),
            Some((lowest("minimal"), Guided))
        );
        assert_eq!(
            next_after_refusal(Protocol::Openai, &Off::Control, OPENAI_NO_PARAM, true),
            Some((Off::Omitted, Guided))
        );
        // The guided form refused as well: no control.
        assert_eq!(
            next_after_refusal(Protocol::Openai, &lowest("minimal"), OPENAI_NO_NONE, false),
            Some((Off::Omitted, Fallback))
        );
        // A refusal that names nothing (`gemini-flash-lite-latest` on
        // `thinkingBudget: 0`): straight to no control — the retry tells
        // whether the off was what it refused.
        let bare = "upstream error (400): Request contains an invalid argument.";
        assert_eq!(
            next_after_refusal(Protocol::Gemini, &Off::Control, bare, true),
            Some((Off::Omitted, Fallback))
        );
        assert_eq!(
            next_after_refusal(Protocol::Gemini, &lowest("minimal"), bare, true),
            Some((Off::Omitted, Fallback))
        );
        // Nothing of the off is left once no control went out.
        for guided in [true, false] {
            assert_eq!(
                next_after_refusal(Protocol::Openai, &Off::Omitted, OPENAI_NO_PARAM, guided),
                None
            );
        }
    }

    #[test]
    fn a_context_refusal_is_told_apart() {
        assert!(about_the_context(
            "This model's maximum context length is 128000 tokens. However, your messages \
             resulted in 130000 tokens."
        ));
        assert!(about_the_context(
            "prompt is too long: 210000 tokens > 200000 maximum"
        ));
        assert!(about_the_context(
            "the request exceeds the available context size, try increasing it"
        ));
        assert!(!about_the_context(OPENAI_NO_NONE));
        assert!(!about_the_context("Request contains an invalid argument."));
    }

    #[test]
    fn words_are_matched_whole() {
        assert!(names_word("Thinking level MINIMAL is not", "minimal"));
        assert!(!names_word("not allowed here", "low"));
        assert!(names_word("'low'", "low"));
        assert_eq!(lowest_supported("no list here"), None);
        assert_eq!(
            lowest_supported("Supported values are: \"medium\", \"turbo\""),
            Some("medium".to_string())
        );
    }
}
