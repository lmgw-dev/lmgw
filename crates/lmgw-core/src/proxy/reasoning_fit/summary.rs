//! The reasoning summary a route refused (openai egress design §7.2.2): the
//! OpenAI platform's Responses wire asks for `reasoning.summary: "auto"` on
//! every send (decided, §7.2.5). A summary is opt-in, some organizations
//! must be verified before they get one, and a model without reasoning may
//! refuse the field. A `400` that names it is learned per route
//! ([`super::Learned`], the second kind of lesson beside the off's), the send
//! is made once more without the summary, and the route is not asked again:
//! an INFO line says so, and the refused attempt keeps its own row.
//!
//! Matched by OpenAI's error shape: a `param` of `reasoning.summary`, or a
//! message naming `reasoning.summary` or reasoning summaries, and, when the
//! send's `reasoning` held the summary alone, one refusing `reasoning`
//! itself (`param: reasoning`, or OpenAI's "Unsupported parameter:
//! 'reasoning'" wording). A refusal reported against any other parameter is
//! never the summary's: `input`'s refusal of a replayed reasoning item
//! quotes 'reasoning' as the item's type.
//! The bodies are hand-written: a probe got no summary refusal
//! (gpt-4o-mini, a model without reasoning, took `summary: "auto"` and
//! answered), so none was captured.

use crate::config::{Protocol, Route};
use crate::egress::openai::responses::{wire_for, Wire};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Params};

/// Whether the send of `ir` with `params` on `route` asks for lmgw's own
/// summary, the one the lesson may take back: an `openai` route, on the
/// Responses wire, whose client said nothing of a summary
/// (`Params::reasoning_summary`; a client's own kind is its to have
/// refused), that did not refuse one.
pub(super) fn asked(route: &Route, ir: &ChatRequest, params: &Params) -> bool {
    route.upstream.protocol == Protocol::Openai
        && params.reasoning_summary.is_none()
        && !params.reasoning_summary_refused
        && wire_for(ir, params) == Wire::Responses
}

/// Whether the send's `reasoning` object held the summary only, no effort
/// (what the Responses body writes, `egress::openai::responses::body`): a
/// refusal of `reasoning` as a whole is then a refusal of the summary.
pub(super) fn only(params: &Params) -> bool {
    let c = params.reasoning_control();
    c.effort.is_none() && c.enabled != Some(false)
}

/// Whether `e`, a refused send's error, is about the summary. `only`: the
/// send's `reasoning` held the summary alone ([`only`]), so a refusal that
/// names `reasoning` itself (a model without reasoning, should OpenAI refuse
/// the object there; a probe saw gpt-4o-mini take it) is one.
pub(super) fn refused(e: &GatewayError, only: bool) -> bool {
    let param = match e {
        GatewayError::UpstreamRelayed(r) => r.param.as_deref(),
        _ => None,
    };
    match param {
        Some("reasoning.summary") => return true,
        Some("reasoning") if only => return true,
        // A refusal OpenAI reports against another parameter is that
        // parameter's, whatever its message quotes: `input`'s "Item 'rs_…'
        // of type 'reasoning' was provided without its required following
        // item." is about a replayed reasoning item (§7.3), never the
        // summary.
        Some(p) if p != "reasoning" && !p.starts_with("reasoning.") => return false,
        _ => {}
    }
    e.upstream_message()
        .is_some_and(|m| names_the_summary(m) || (only && refuses_reasoning(m)))
}

/// Whether a refusal's message refuses the `reasoning` parameter itself, in
/// OpenAI's wording for an unsupported parameter ("Unsupported parameter:
/// 'reasoning' is not supported with this model."), not one that merely
/// quotes the word (an item "of type 'reasoning'").
fn refuses_reasoning(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("unsupported parameter: 'reasoning'") || m.contains("'reasoning' is not supported")
}

/// Whether a refusal's message names the summary.
fn names_the_summary(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("reasoning.summary") || m.contains("reasoning summar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Relayed;

    fn relayed(param: Option<&str>, message: &str) -> GatewayError {
        GatewayError::UpstreamRelayed(Box::new(Relayed {
            status: 400,
            error_type: Some("invalid_request_error".into()),
            code: Some("unsupported_parameter".into()),
            param: param.map(String::from),
            message: message.into(),
        }))
    }

    #[test]
    fn a_refusal_naming_the_summary_is_read_as_one() {
        // Beside an effort or not.
        for only in [true, false] {
            assert!(refused(
                &relayed(Some("reasoning.summary"), "Unsupported parameter."),
                only
            ));
            assert!(refused(
                &relayed(
                    None,
                    "Your organization must be verified to generate reasoning summaries."
                ),
                only
            ));
            assert!(refused(
                &GatewayError::Upstream {
                    status: 400,
                    provider_type: None,
                    message: "Unsupported parameter: 'reasoning.summary' is not supported with \
                              this model."
                        .into(),
                },
                only
            ));
        }
    }

    #[test]
    fn any_other_refusal_is_not() {
        assert!(!refused(
            &relayed(
                Some("reasoning.effort"),
                "Unsupported value: 'reasoning.effort' does not support 'none' with this model."
            ),
            true
        ));
        assert!(!refused(&GatewayError::Timeout, true));
    }

    /// A refusal of `reasoning` as a whole is the summary's when the send's
    /// `reasoning` held the summary alone, and only then: beside an effort
    /// it may be the effort's.
    #[test]
    fn a_refusal_of_reasoning_itself_is_the_summary_s_when_it_was_all_that_was_sent() {
        let whole = relayed(
            Some("reasoning"),
            "Unsupported parameter: 'reasoning' is not supported with this model.",
        );
        assert!(refused(&whole, true));
        assert!(!refused(&whole, false));
        let worded = GatewayError::Upstream {
            status: 400,
            provider_type: None,
            message: "Unsupported parameter: 'reasoning' is not supported with this model.".into(),
        };
        assert!(refused(&worded, true));
        assert!(!refused(&worded, false));
        // OpenAI's refusal of a replayed reasoning item names `input`, and
        // quotes 'reasoning' as the item's type: never a summary refusal.
        let item = GatewayError::UpstreamRelayed(Box::new(Relayed {
            status: 400,
            error_type: Some("invalid_request_error".into()),
            code: None,
            param: Some("input".into()),
            message: "Item 'rs_0abc' of type 'reasoning' was provided without its required \
                      following item."
                .into(),
        }));
        assert!(!refused(&item, true));
        assert!(!refused(&item, false));
        // Unrelayed, its words alone are no refusal of `reasoning` either.
        let unrelayed = GatewayError::Upstream {
            status: 400,
            provider_type: None,
            message: "Item 'rs_0abc' of type 'reasoning' was provided without its required \
                      following item."
                .into(),
        };
        assert!(!refused(&unrelayed, true));
        let mut p = Params::default();
        assert!(only(&p), "no effort: the summary alone");
        p.reasoning = Some(crate::ir::ReasoningControl {
            effort: Some("low".into()),
            ..Default::default()
        });
        assert!(!only(&p));
    }
}
