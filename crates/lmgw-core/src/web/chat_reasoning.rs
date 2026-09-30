//! A Chat thread's reasoning overrides: the `x-lmgw-reasoning*` headers'
//! three values, kept with the thread so a conversation can try a setting —
//! thinking off, another effort, a budget — without the alias being edited.
//!
//! They go out at the header tier: the thread's control is the request's own
//! [`Params::reasoning`](crate::ir::Params::reasoning), which the route's
//! defaults only fill in around ([`Params::with_defaults`](crate::ir::Params::with_defaults)),
//! exactly as the headers do for an API client. The same contradictions the
//! headers refuse are refused here, when the thread is saved rather than at
//! every send.

use crate::config::Route;
use crate::ir::{ChatRequest, ReasoningControl};
use crate::store::ChatThread;

/// The thread's overrides as a control, `None` when it sets none.
pub(super) fn control(t: &ChatThread) -> Option<ReasoningControl> {
    let c = ReasoningControl {
        enabled: t.reasoning_enabled,
        effort: t.reasoning_effort.clone(),
        budget_tokens: t.reasoning_budget,
    };
    (!c.is_empty()).then_some(c)
}

/// Normalise and check a thread's three values before they are stored: an
/// effort is trimmed and a blank one is no effort, a budget is never
/// negative, and the halves must agree — `on` with an effort of `none` or a
/// budget of 0, or `off` with a real effort or a positive budget, is one
/// setting contradicting another, refused by name like the headers refuse it
/// (`server::reasoning_from_headers`) rather than half-applied.
pub(super) fn check(t: &mut ChatThread) -> Result<(), String> {
    t.reasoning_effort = t
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    if let Some(b) = t.reasoning_budget.filter(|b| *b < 0) {
        return Err(format!(
            "reasoning budget: expected a non-negative number of tokens, got {b}"
        ));
    }
    let effort_none = t
        .reasoning_effort
        .as_deref()
        .is_some_and(|e| e.eq_ignore_ascii_case("none"));
    match t.reasoning_enabled {
        Some(true) if effort_none => {
            Err("reasoning 'on' contradicts an effort of 'none' — pick a level, or clear it".into())
        }
        Some(true) if t.reasoning_budget == Some(0) => {
            Err("reasoning 'on' contradicts a budget of 0 — raise it, or clear it".into())
        }
        Some(false) if t.reasoning_effort.is_some() && !effort_none => Err(format!(
            "reasoning 'off' contradicts an effort of '{}' — clear the effort, or switch \
             reasoning back to the model's default",
            t.reasoning_effort.as_deref().unwrap_or_default()
        )),
        Some(false) if t.reasoning_budget.is_some_and(|b| b > 0) => Err(
            "reasoning 'off' contradicts a positive budget — clear the budget, or switch \
             reasoning back to the model's default"
                .into(),
        ),
        _ => Ok(()),
    }
}

/// Which of the thread's overrides the route that answers does not send —
/// the chat's `x-lmgw-reasoning-ignored`, reported with the turn so a tried
/// setting that never reached the model says so.
///
/// Judged like the header path judges it: on the control that actually goes
/// out, the thread's merged over the route's own defaults — an alias budget
/// beside the thread's effort is what makes an Anthropic route drop the
/// effort, and an alias effort beside the thread's "on" is what lets a
/// generic OpenAI route express it. Only fields the thread set are reported:
/// an alias default the route drops is not something the thread asked for.
pub(super) fn ignored(ir: &ChatRequest, route: &Route) -> Vec<&'static str> {
    let Some(own) = &ir.params.reasoning else {
        return Vec::new();
    };
    let sent = ir
        .params
        .clone()
        .with_defaults(&route.param_defaults)
        .reasoning_control();
    crate::proxy::reasoning_ignored(
        route.upstream.protocol,
        route.upstream.kind,
        &sent,
        crate::egress::openai::has_reasoning_object(ir),
    )
    .into_iter()
    .filter(|field| match *field {
        "enabled" => own.enabled.is_some(),
        "effort" => own.effort.is_some(),
        "budget" => own.budget_tokens.is_some(),
        _ => true,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(enabled: Option<bool>, effort: Option<&str>, budget: Option<i64>) -> ChatThread {
        ChatThread {
            id: 1,
            title: String::new(),
            model_alias: "m".into(),
            system_prompt: String::new(),
            temperature: None,
            max_tokens: None,
            kind: "chat".into(),
            mcp_tools: Vec::new(),
            reasoning_enabled: enabled,
            reasoning_effort: effort.map(str::to_string),
            reasoning_budget: budget,
            agent_id: None,
            pinned: false,
            archived_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_set_is_no_control() {
        assert_eq!(control(&thread(None, None, None)), None);
        let c = control(&thread(Some(false), None, None)).unwrap();
        assert_eq!(c.enabled, Some(false));
    }

    #[test]
    fn a_blank_effort_is_no_effort() {
        let mut t = thread(None, Some("  "), None);
        check(&mut t).unwrap();
        assert_eq!(t.reasoning_effort, None);
        let mut t = thread(None, Some(" high "), None);
        check(&mut t).unwrap();
        assert_eq!(t.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn contradictions_are_refused_both_ways() {
        for (enabled, effort, budget) in [
            (Some(true), Some("none"), None),
            (Some(true), None, Some(0)),
            (Some(false), Some("high"), None),
            (Some(false), None, Some(512)),
            (None, None, Some(-1)),
        ] {
            let mut t = thread(enabled, effort, budget);
            assert!(check(&mut t).is_err(), "{enabled:?} {effort:?} {budget:?}");
        }
    }

    #[test]
    fn agreeing_halves_pass() {
        for (enabled, effort, budget) in [
            (Some(true), Some("high"), Some(2048)),
            (Some(false), Some("none"), Some(0)),
            (None, Some("none"), None),
            (None, None, Some(0)),
            (Some(false), None, None),
        ] {
            let mut t = thread(enabled, effort, budget);
            assert!(check(&mut t).is_ok(), "{enabled:?} {effort:?} {budget:?}");
        }
    }
}
