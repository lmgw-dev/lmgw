//! Which form an off takes on one cloud model: a pure decision on what lmgw
//! knows about it (module doc of [`super`]).

use crate::capabilities::ReasoningCaps;
use crate::config::Protocol;

use super::{Basis, Lesson, Off};

/// What an off becomes on a Gemini model nothing more is known about: its
/// lowest thinking level. Gemini's catalog says only *whether* a model thinks
/// (`thinking: true`), never whether it can stop or which levels it takes,
/// and the current models cannot stop: `gemini-flash-lite-latest` refuses
/// `thinkingBudget: 0` and `gemini-flash-latest` takes it and thinks anyway
/// (probed 2026-10-04). A model whose least is above `minimal` says so in
/// its refusal, which names the level, and is retried one level up
/// ([`super::refusal`]).
pub(super) const GEMINI_LOWEST: &str = "minimal";

/// The form and its basis: the model's capabilities when they settle it,
/// then what the provider said on an earlier request, then the protocol's
/// default. A lesson stands in for missing facts, and corrects facts only
/// where the provider refused exactly the form they give.
pub(super) fn decide(
    protocol: Protocol,
    facts: Option<&ReasoningCaps>,
    learned: Option<&Lesson>,
) -> (Off, Basis) {
    let taught = |l: &Lesson| {
        (
            l.instead.clone(),
            Basis::Learned {
                fallback: l.fallback,
            },
        )
    };
    if let Some((off, why)) = facts.and_then(|r| from_facts(protocol, r)) {
        return match learned.filter(|l| l.refused == off) {
            Some(l) => taught(l),
            None => (off, Basis::Facts(why)),
        };
    }
    if let Some(l) = learned {
        return taught(l);
    }
    let off = match protocol {
        Protocol::Gemini => Off::Lowest(GEMINI_LOWEST.to_string()),
        Protocol::Openai | Protocol::Anthropic | Protocol::LlamaCpp => Off::Control,
    };
    (off, Basis::Default)
}

/// Why the protocol default is what it is, for the log line when it is not
/// the protocol's own off.
pub(super) fn default_reason(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Gemini => {
            "Gemini's catalog does not say whether this model can stop thinking, and current \
             Gemini models cannot — minimal is the least they take"
        }
        Protocol::Openai | Protocol::Anthropic | Protocol::LlamaCpp => {
            "nothing is known about the model"
        }
    }
}

/// The capabilities' answer, when they give one. `None`: they say the model
/// can be controlled but not whether that reaches "off" (a bare `toggle`, or
/// Anthropic's effort levels), or there are none.
fn from_facts(protocol: Protocol, r: &ReasoningCaps) -> Option<(Off, &'static str)> {
    if r.kind == "fixed" {
        return Some((
            Off::Omitted,
            "its capabilities say no request changes its reasoning (kind fixed)",
        ));
    }
    let none_level = r.levels.iter().any(|l| l.eq_ignore_ascii_case("none"));
    if r.can_disable == Some(true) || none_level {
        return Some((
            Off::Control,
            "its capabilities say a request can switch reasoning off",
        ));
    }
    if r.enabled == Some(false) {
        return Some((
            Off::Omitted,
            "its capabilities say it does not reason unless asked to",
        ));
    }
    // `levels` run least → most (§2.1).
    let lowest = r.levels.first();
    if r.can_disable == Some(false) {
        return Some(match lowest {
            Some(l) => (
                Off::Lowest(l.clone()),
                "its capabilities say it cannot switch reasoning off, and this is its lowest level",
            ),
            None => (
                Off::Omitted,
                "its capabilities say it cannot switch reasoning off, and list no level to lower \
                 it to",
            ),
        });
    }
    // A level list is what the effort control takes. On the OpenAI protocol
    // that is the control the off is a value of (`reasoning_effort`), and on
    // Gemini the off is already sent as a level, so a list without `none` is
    // the model's own answer. Anthropic's off is a thinking type of its own,
    // which its effort levels say nothing about.
    match lowest {
        Some(l) if protocol != Protocol::Anthropic => Some((
            Off::Lowest(l.clone()),
            "its capabilities list the effort levels it takes, and none of them is off",
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(kind: &str, levels: &[&str], can_disable: Option<bool>) -> ReasoningCaps {
        ReasoningCaps {
            kind: kind.to_string(),
            levels: levels.iter().map(|l| l.to_string()).collect(),
            can_disable,
            ..Default::default()
        }
    }

    fn lowest(l: &str) -> Off {
        Off::Lowest(l.to_string())
    }

    #[test]
    fn nothing_known_is_each_protocols_default() {
        assert_eq!(
            decide(Protocol::Openai, None, None),
            (Off::Control, Basis::Default)
        );
        assert_eq!(
            decide(Protocol::Anthropic, None, None),
            (Off::Control, Basis::Default)
        );
        assert_eq!(
            decide(Protocol::Gemini, None, None),
            (lowest("minimal"), Basis::Default)
        );
        // A bare toggle says a control exists, not whether it reaches "off".
        let toggle = caps("toggle", &[], None);
        for p in [Protocol::Openai, Protocol::Anthropic, Protocol::Gemini] {
            assert_eq!(decide(p, Some(&toggle), None).1, Basis::Default, "{p:?}");
        }
    }

    #[test]
    fn a_model_without_a_control_gets_none() {
        // Gemini's `thinking: false`, Anthropic's `thinking.supported: false`.
        let fixed = ReasoningCaps {
            enabled: Some(false),
            ..caps("fixed", &[], None)
        };
        for p in [Protocol::Openai, Protocol::Anthropic, Protocol::Gemini] {
            assert_eq!(decide(p, Some(&fixed), None).0, Off::Omitted, "{p:?}");
        }
        // Off by default already: nothing to send.
        let off_by_default = ReasoningCaps {
            enabled: Some(false),
            ..caps("toggle", &[], None)
        };
        assert_eq!(
            decide(Protocol::Openai, Some(&off_by_default), None).0,
            Off::Omitted
        );
        // Reasons, cannot stop, and nothing lower to ask for.
        let stuck = caps("toggle", &[], Some(false));
        assert_eq!(decide(Protocol::Openai, Some(&stuck), None).0, Off::Omitted);
    }

    #[test]
    fn a_model_that_can_stop_gets_the_protocols_off() {
        // Kilo's `gpt-5.4-nano`: levels, and `none` among its variants.
        let can = caps("levels", &["low", "medium", "high", "xhigh"], Some(true));
        let (off, basis) = decide(Protocol::Openai, Some(&can), None);
        assert_eq!(off, Off::Control);
        assert!(matches!(basis, Basis::Facts(_)));
        // An owner override that lists `none` as a level says the same.
        let listed = caps("levels", &["none", "low"], None);
        assert_eq!(
            decide(Protocol::Gemini, Some(&listed), None).0,
            Off::Control
        );
    }

    #[test]
    fn a_model_that_cannot_stop_gets_its_lowest_level() {
        // Kilo's `gpt-5-nano` and `gemini-3.1-pro-preview`: the levels its
        // catalog lists, none of them off.
        let nano = caps("levels", &["minimal", "low", "medium", "high"], None);
        assert_eq!(
            decide(Protocol::Openai, Some(&nano), None).0,
            lowest("minimal")
        );
        let pro = caps("levels", &["low", "medium", "high"], None);
        assert_eq!(decide(Protocol::Openai, Some(&pro), None).0, lowest("low"));
        assert_eq!(decide(Protocol::Gemini, Some(&pro), None).0, lowest("low"));
        // Said outright (an owner override), on any protocol.
        let stuck = caps("levels", &["low", "high"], Some(false));
        assert_eq!(
            decide(Protocol::Anthropic, Some(&stuck), None).0,
            lowest("low")
        );
    }

    #[test]
    fn anthropic_effort_levels_say_nothing_about_off() {
        // `claude-opus-4-8` in Anthropic's own catalog: effort levels, and
        // `thinking: {type: "disabled"}` is a type of its own.
        let opus = caps("levels", &["low", "medium", "high", "xhigh", "max"], None);
        assert_eq!(
            decide(Protocol::Anthropic, Some(&opus), None),
            (Off::Control, Basis::Default)
        );
    }

    #[test]
    fn what_was_learned_stands_in_for_missing_facts_only() {
        let lesson = |refused: Off, instead: Off, fallback: bool| Lesson {
            refused,
            instead,
            fallback,
        };
        let omitted = lesson(Off::Control, Off::Omitted, false);
        assert_eq!(
            decide(Protocol::Openai, None, Some(&omitted)),
            (Off::Omitted, Basis::Learned { fallback: false })
        );
        let toggle = caps("toggle", &[], None);
        let climbed = lesson(lowest("minimal"), lowest("low"), false);
        assert_eq!(
            decide(Protocol::Gemini, Some(&toggle), Some(&climbed)),
            (lowest("low"), Basis::Learned { fallback: false })
        );
        // Facts that settle it win over an older lesson about another form.
        let can = caps("levels", &["low"], Some(true));
        let learned_low = lesson(lowest("minimal"), lowest("low"), false);
        assert_eq!(
            decide(Protocol::Openai, Some(&can), Some(&learned_low)).0,
            Off::Control
        );
        // …but not over the provider refusing exactly the form they give.
        let refused_off = lesson(Off::Control, Off::Omitted, true);
        assert_eq!(
            decide(Protocol::Openai, Some(&can), Some(&refused_off)),
            (Off::Omitted, Basis::Learned { fallback: true })
        );
    }
}
