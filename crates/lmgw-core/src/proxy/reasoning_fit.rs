//! What a reasoning **off** becomes on one cloud model (model-capabilities
//! design §5.6).
//!
//! The egress table (§5.3) has one spelling of "off" per protocol —
//! `reasoning_effort: "none"`, `thinking: {type: "disabled"}`,
//! `thinkingBudget: 0` — and not every model takes it. A model that does not
//! reason refuses the control outright (`gpt-4.1-nano`: "Unrecognized request
//! argument supplied: reasoning_effort"), and one that reasons but cannot stop
//! refuses the value (`gpt-5-nano` takes `minimal` at the least; current
//! Gemini models take a thinking level, and `gemini-flash-lite-latest` answers
//! `thinkingBudget: 0` with a bare "invalid argument"). The Chat asks for off
//! on its own — every voice turn whose thread leaves reasoning alone (chat-voice
//! design §8.5), and realtime the same (§7.6) — so a refusal there broke a
//! conversation the owner never configured reasoning for.
//!
//! So an off on a cloud route is fitted to the model before it goes out:
//!
//! - a model that does not reason, or has no control to take, gets **no
//!   reasoning control** ([`Off::Omitted`]);
//! - a model that reasons and cannot stop gets **its lowest level**
//!   ([`Off::Lowest`]);
//! - a model that can stop gets the protocol's own off ([`Off::Control`]).
//!
//! Decided from what lmgw knows ([`rule::decide`]): the model's capabilities
//! as `/v1/models` derives them (the upstream catalog, an owner override on
//! an alias of it), then what the provider itself said on an earlier request
//! ([`Learned`]), then the protocol's default.
//!
//! **No off ends in an error** (the owner's ruling of 2026-10-04: a model
//! that cannot run without reasoning runs with it — whether it may be used,
//! for live mode say, is the user's decision). A `400` (or `422`) to a
//! request that carried an off is retried: **once** with what the refusal's
//! message says the model takes, when it names the control
//! ([`refusal::after_refusal`]), and as the last resort with **no reasoning
//! control at all**, so the model reasons as it does by default
//! ([`refusal::next_after_refusal`]). Every refused attempt keeps its own
//! request row saying what was retried with, and the form that answered is
//! remembered for the route so it is not asked again ([`send::send_chat`]).
//! A refusal lmgw can tell is about something else (the prompt does not fit
//! the context) stands at once; any other one that is not about the off is
//! refused again without it, and that answer stands.
//!
//! Nothing here is silent: every off that goes out in another form than asked
//! is an INFO line, and the request reports `enabled` among its ignored
//! controls (`x-lmgw-reasoning-ignored`, the Chat's `reasoning_ignored`). A
//! caller that reads the answer — the Chat — also tells whether the model
//! reasoned although the off went out ([`Fitted::observe`]), which a local
//! template that cannot stop does, and says it in one sentence
//! ([`Fitted::note`]).
//!
//! Only "off" is fitted. An explicit level, budget or "on" goes out as asked,
//! and a model that refuses it answers with its own error — a request that
//! wants thinking from a model without it should hear so, not be served
//! something else. Local llama-server rows are not fitted: their off is the
//! template's own `enable_thinking` (§5.3), which llama-server does not
//! refuse — a template that does not read it just thinks, and that is
//! observed and reported.

mod refusal;
mod rule;
mod send;
#[cfg(test)]
mod tests;

pub(crate) use send::{send_chat, RowAs};

use std::collections::HashMap;
use std::sync::Mutex;

use crate::capabilities::{self, ReasoningCaps};
use crate::config::{Protocol, Route, UpstreamKind};
use crate::ir::{Params, ReasoningControl};
use crate::state::SharedState;

/// The form an off takes on one route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Off {
    /// The protocol's own off (§5.3): OpenAI `reasoning_effort: "none"`,
    /// Anthropic `thinking: {type: "disabled"}`, Gemini `thinkingBudget: 0`.
    Control,
    /// No reasoning keys at all: the model has no control to take.
    Omitted,
    /// The model's lowest effort level: it reasons and cannot stop.
    Lowest(String),
}

impl Off {
    /// Write this form into `params` (whose control is an off).
    fn apply(&self, params: &mut Params) {
        match self {
            Self::Control => {}
            Self::Omitted => params.reasoning = None,
            Self::Lowest(level) => {
                params.reasoning = Some(ReasoningControl {
                    enabled: Some(true),
                    effort: Some(level.clone()),
                    budget_tokens: None,
                });
            }
        }
    }

    /// What goes on the wire, in `protocol`'s words — for the log lines.
    fn describe(&self, protocol: Protocol) -> String {
        match (self, protocol) {
            (Self::Omitted, _) => "no reasoning control".into(),
            (Self::Control, Protocol::Openai) => "reasoning_effort: \"none\"".into(),
            (Self::Control, Protocol::LlamaCpp) => {
                "chat_template_kwargs: {enable_thinking: false}".into()
            }
            (Self::Control, Protocol::Anthropic) => "thinking: {type: \"disabled\"}".into(),
            (Self::Control, Protocol::Gemini) => "thinkingBudget: 0".into(),
            (Self::Lowest(l), Protocol::Openai | Protocol::LlamaCpp) => {
                format!("reasoning_effort: \"{l}\"")
            }
            (Self::Lowest(l), Protocol::Anthropic) => format!("output_config.effort: \"{l}\""),
            (Self::Lowest(l), Protocol::Gemini) => format!("thinkingLevel: \"{l}\""),
        }
    }
}

/// Where a route's [`Off`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The model's capabilities decided; the clause says why.
    Facts(&'static str),
    /// The provider refused an off on this route — on this request or an
    /// earlier one. `fallback`: it refused every form, and the route sends no
    /// control (the last resort, [`refusal::Retry::Fallback`]).
    Learned { fallback: bool },
    /// Nothing is known about the model: the protocol's default.
    Default,
    /// A local llama-server row: the template's own off (§5.3), not fitted.
    Local,
}

/// An off as it goes out on one send — or nothing, when the request asked
/// for no off.
#[derive(Debug, Clone, Default)]
pub(crate) struct Fitted {
    off: Option<(Off, Basis)>,
    /// The answer carried reasoning although the off went out
    /// ([`Self::observe`]).
    reasoned: bool,
}

impl Fitted {
    /// `enabled` when the off this request asked for did not go out as the
    /// protocol's own off, or the model reasoned anyway — one of the controls
    /// the route did not honour.
    pub(crate) fn ignored(&self) -> Option<&'static str> {
        match &self.off {
            Some((off, _)) if *off != Off::Control || self.reasoned => Some("enabled"),
            _ => None,
        }
    }

    /// The off that went out, when a refusal of it may still be retried: a
    /// cloud route that sent some control (module doc).
    fn retryable(&self) -> Option<&Off> {
        match &self.off {
            Some((_, Basis::Local)) | Some((Off::Omitted, _)) | None => None,
            Some((off, _)) => Some(off),
        }
    }

    /// The form that goes out now on a cloud route, and whether it is the
    /// last resort.
    fn sent(&self) -> Option<(Off, bool)> {
        match &self.off {
            Some((_, Basis::Local)) | None => None,
            Some((off, Basis::Learned { fallback })) => Some((off.clone(), *fallback)),
            Some((off, _)) => Some((off.clone(), false)),
        }
    }

    /// The form came from what the provider said before.
    fn learned(&self) -> bool {
        matches!(&self.off, Some((_, Basis::Learned { .. })))
    }

    /// The send was retried with `instead` after a refusal.
    fn retried(&mut self, instead: Off, how: refusal::Retry) {
        let fallback = how == refusal::Retry::Fallback;
        self.off = Some((instead, Basis::Learned { fallback }));
    }

    /// Add [`Self::ignored`] to a request's list of ignored controls, once.
    pub(crate) fn report(&self, ignored: &mut Vec<&'static str>) {
        if let Some(field) = self.ignored() {
            if !ignored.contains(&field) {
                ignored.push(field);
            }
        }
    }

    /// Whether the answer to `route` carried reasoning: one that did although
    /// an off went out is an INFO line, and is reported as ignored from now
    /// on ([`Self::ignored`]). A model at its lowest level was expected to.
    pub(crate) fn observe(&mut self, route: &Route, reasoned: bool) {
        let Some((off, _)) = &self.off else {
            return;
        };
        if !reasoned || self.reasoned {
            return;
        }
        self.reasoned = true;
        if matches!(off, Off::Lowest(_)) {
            return;
        }
        let sent = match &self.off {
            Some((_, Basis::Local)) => {
                "chat_template_kwargs.enable_thinking: false (its template does not switch \
                 thinking off)"
                    .to_string()
            }
            _ => off.describe(route.upstream.protocol),
        };
        tracing::info!(
            upstream = %route.upstream.name,
            model = %route.upstream_model,
            "reasoning off was sent as {sent}, and the model reasoned anyway; the reasoning is \
             kept with the answer and reported as ignored"
        );
    }

    /// What the status line says about the off on `model` (the alias the
    /// user sees), when the model reasoned although off was asked or must
    /// have: at its lowest level, after refusing every off, or seen to.
    /// `None` when the off took, or the model takes no control and showed no
    /// reasoning (it does not reason, as far as anything tells).
    pub(crate) fn note(&self, model: &str) -> Option<String> {
        let (off, basis) = self.off.as_ref()?;
        match (off, basis) {
            (Off::Lowest(level), _) => Some(format!(
                "{model} cannot switch reasoning off; it reasons at its lowest level ({level})"
            )),
            (Off::Omitted, Basis::Learned { fallback: true }) => Some(format!(
                "{model} refused every way lmgw has to switch reasoning off; it reasons as it \
                 does by default"
            )),
            _ if self.reasoned => Some(format!(
                "{model} did not switch reasoning off; it reasoned anyway"
            )),
            _ => None,
        }
    }
}

/// Fit the off in `params` (the send's final params: every tier merged) to
/// `route`. Leaves anything else alone: a request without an off, and every
/// route that is not a cloud one ([`UpstreamKind::Generic`]) — whose off is
/// still recorded, so what its answer shows can be observed.
pub(crate) async fn fit(state: &SharedState, route: &Route, params: &mut Params) -> Fitted {
    if params.reasoning_control().enabled != Some(false) {
        return Fitted::default();
    }
    if route.upstream.kind != UpstreamKind::Generic {
        return Fitted {
            off: Some((Off::Control, Basis::Local)),
            reasoned: false,
        };
    }
    let facts = facts(state, route).await;
    let learned = state.reasoning_learned.get(route);
    let (off, basis) = rule::decide(route.upstream.protocol, facts.as_ref(), learned.as_ref());
    if off != Off::Control {
        let why = match &basis {
            Basis::Facts(why) => (*why).to_string(),
            Basis::Learned { fallback: false } => {
                "the provider refused an earlier off on this model, and this is what it took \
                 instead"
                    .to_string()
            }
            Basis::Learned { fallback: true } => {
                "the provider refused every form of off on this model before, so it reasons as \
                 it does by default"
                    .to_string()
            }
            Basis::Default => rule::default_reason(route.upstream.protocol).to_string(),
            Basis::Local => unreachable!("a local route is not fitted"),
        };
        tracing::info!(
            upstream = %route.upstream.name,
            model = %route.upstream_model,
            "reasoning off goes out as {} instead of {}: {why}",
            off.describe(route.upstream.protocol),
            Off::Control.describe(route.upstream.protocol),
        );
        off.apply(params);
    }
    Fitted {
        off: Some((off, basis)),
        reasoned: false,
    }
}

/// What an off becomes on a model with these capabilities, as a sentence for
/// its `/v1/models` notes — the rule [`fit`] applies, before anything a
/// refusal may teach it.
pub(crate) fn off_note(protocol: Protocol, facts: Option<&ReasoningCaps>) -> String {
    let (off, basis) = rule::decide(protocol, facts, None);
    let sent = off.describe(protocol);
    match (&off, basis) {
        (Off::Omitted, Basis::Facts(why)) => {
            format!("x-lmgw-reasoning: off sends no reasoning control, since {why}.")
        }
        (Off::Control, Basis::Facts(_)) => {
            format!("x-lmgw-reasoning: off is sent upstream as {sent}.")
        }
        (Off::Lowest(_), Basis::Facts(why)) => {
            format!("x-lmgw-reasoning: off is sent upstream as {sent}, since {why}.")
        }
        _ => {
            let lead = match off {
                Off::Lowest(_) => format!(
                    "x-lmgw-reasoning: off is sent upstream as {sent}: {}",
                    rule::default_reason(protocol)
                ),
                _ => format!("x-lmgw-reasoning: off is sent upstream as {sent}"),
            };
            format!(
                "{lead}; should the provider refuse it, lmgw retries with what the refusal says \
                 the model takes and, failing that, with no reasoning control (the model then \
                 reasons as it does by default), and keeps what answered for the model."
            )
        }
    }
}

/// What `/v1/models` says about how `route`'s model reasons: the upstream
/// catalog's entry (cached, the same lookup the listing makes), with the
/// owner's override of an alias onto exactly this model over it. Several
/// aliases onto the model whose overrides differ say nothing — which of them
/// this send came through is not known here.
async fn facts(state: &SharedState, route: &Route) -> Option<ReasoningCaps> {
    let protocol = route.upstream.protocol;
    let listed = match crate::catalog::upstream_models(state, &route.upstream).await {
        Ok(models) => models.into_iter().find(|m| m.id == route.upstream_model),
        Err(e) => {
            tracing::debug!(
                "catalog of '{}' unavailable for the reasoning fit: {e}",
                route.upstream.name
            );
            None
        }
    };
    let derived = listed
        .map(|m| capabilities::for_catalog(&m, protocol, &route.upstream.name))
        .unwrap_or_default();
    let derived = match owner_override(state, route) {
        Some(o) => {
            capabilities::apply_owner_override(derived.clone(), &o, protocol).unwrap_or(derived)
        }
        None => derived,
    };
    derived.capabilities?.reasoning
}

fn owner_override(state: &SharedState, route: &Route) -> Option<serde_json::Value> {
    let snap = state.snapshot();
    let mut all = snap
        .aliases
        .values()
        .filter(|a| {
            a.enabled
                && a.upstream_id == route.upstream.id
                && a.upstream_model_id == route.upstream_model
        })
        .filter_map(|a| a.capabilities_override.clone());
    let first = all.next()?;
    all.all(|o| o == first).then_some(first)
}

/// What a refusal taught about one route: the form that was `refused` (the
/// first this route sent), what answered `instead`, and whether that was the
/// last resort.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lesson {
    refused: Off,
    instead: Off,
    fallback: bool,
}

/// What providers said about their models' off controls, per route — the
/// form a refusal showed works ([`send::send_chat`]). Kept for the process's
/// life; keyed by the upstream's base URL as well, so an upstream pointed
/// elsewhere starts over.
#[derive(Debug, Default)]
pub struct Learned(Mutex<HashMap<(i64, String, String), Lesson>>);

impl Learned {
    fn key(route: &Route) -> (i64, String, String) {
        (
            route.upstream.id,
            route.upstream.base_url.clone(),
            route.upstream_model.clone(),
        )
    }

    fn get(&self, route: &Route) -> Option<Lesson> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&Self::key(route)).cloned()
    }

    fn remember(&self, route: &Route, lesson: Lesson) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(Self::key(route), lesson);
    }
}
