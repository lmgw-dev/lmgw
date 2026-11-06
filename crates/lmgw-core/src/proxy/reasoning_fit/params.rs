//! A sampling parameter a model refuses (api-compat design §9, the reasoning
//! off's mechanic extended, model-capabilities design §5.6): current models
//! refuse what older ones took — Anthropic "`temperature` is deprecated for
//! this model.", OpenAI "Unsupported parameter: 'temperature' is not
//! supported with this model." and "Unsupported value: 'temperature' does
//! not support 0 with this model. Only the default (1) value is supported."
//!
//! A `400` (or `422`) whose words name exactly one parameter the send
//! carried, in one of those wordings, is retried once without exactly that
//! parameter — the others stay — its refused attempt a request row of its
//! own whose note says which parameter was left out and why. When the retry
//! answers, the parameter is remembered for the route ([`super::Learned`],
//! per upstream and model) and left out of its later sends, each of which
//! says so in `x-lmgw-params-refused` and an INFO line. Nothing is left out
//! without a refusal on record: a model that takes a parameter keeps getting
//! it. Wording this module does not know is no refusal of a parameter: it
//! goes to the off's retry, or stands.
//!
//! Only the sampling parameters lmgw models are read ([`Sampler`]): a
//! client's own passthrough keys go as they came, and a refusal of one
//! stands (not yet). `stop` is not one of them: leaving it out changes what
//! the answer says, not how it is sampled, and the owner's ruling of
//! 2026-10-24 (openai egress design §7.2.5) has OpenAI's own refusal of it
//! reach the client; whether `stop` joins the modelled parameters is not
//! decided yet.
//!
//! Every route that sends a model's parameters retries the same way: the
//! chat sends ([`super::send_chat`]) on the [`Params`] they render, and the
//! native `/v1/responses` passthrough (`responses/native/refused_params.rs`),
//! which forwards the client's body, on that body's top-level keys of the
//! same names ([`refused_in_body`], [`Sampler::remove_from`]). Both read and
//! teach the one [`super::Learned`] memory, with the same matcher, notes and
//! log lines.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::Params;
use crate::state::SharedState;

/// A modelled parameter a refusal may name, in the header's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Sampler {
    Temperature,
    TopP,
    TopK,
    MinP,
    RepeatPenalty,
    PresencePenalty,
    FrequencyPenalty,
    Seed,
}

impl Sampler {
    const ALL: [Sampler; 8] = [
        Self::Temperature,
        Self::TopP,
        Self::TopK,
        Self::MinP,
        Self::RepeatPenalty,
        Self::PresencePenalty,
        Self::FrequencyPenalty,
        Self::Seed,
    ];

    /// The parameter's name as the OpenAI wire (and `x-lmgw-params-refused`)
    /// spells it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::TopK => "top_k",
            Self::MinP => "min_p",
            Self::RepeatPenalty => "repeat_penalty",
            Self::PresencePenalty => "presence_penalty",
            Self::FrequencyPenalty => "frequency_penalty",
            Self::Seed => "seed",
        }
    }

    /// The parameter a provider's word for it names.
    fn named(word: &str) -> Option<Self> {
        let word = word
            .trim()
            .trim_matches(|c| c == '`' || c == '\'' || c == '"');
        Self::ALL.into_iter().find(|s| s.name() == word)
    }

    pub(crate) fn is_set(self, p: &Params) -> bool {
        match self {
            Self::Temperature => p.temperature.is_some(),
            Self::TopP => p.top_p.is_some(),
            Self::TopK => p.top_k.is_some(),
            Self::MinP => p.min_p.is_some(),
            Self::RepeatPenalty => p.repeat_penalty.is_some(),
            Self::PresencePenalty => p.presence_penalty.is_some(),
            Self::FrequencyPenalty => p.frequency_penalty.is_some(),
            Self::Seed => p.seed.is_some(),
        }
    }

    /// Whether a forwarded JSON body carries the parameter: a top-level key
    /// of its name whose value is not `null` (what the ingress reads as
    /// unset).
    pub(crate) fn in_body(self, body: &Value) -> bool {
        body.get(self.name()).is_some_and(|v| !v.is_null())
    }

    /// Take the parameter's top-level key out of a forwarded JSON body.
    pub(crate) fn remove_from(self, body: &mut Value) {
        if let Some(o) = body.as_object_mut() {
            o.remove(self.name());
        }
    }

    pub(crate) fn clear(self, p: &mut Params) {
        match self {
            Self::Temperature => p.temperature = None,
            Self::TopP => p.top_p = None,
            Self::TopK => p.top_k = None,
            Self::MinP => p.min_p = None,
            Self::RepeatPenalty => p.repeat_penalty = None,
            Self::PresencePenalty => p.presence_penalty = None,
            Self::FrequencyPenalty => p.frequency_penalty = None,
            Self::Seed => p.seed = None,
        }
    }
}

/// Whether `params` carry any parameter a refusal could name.
pub(super) fn any_set(params: &Params) -> bool {
    Sampler::ALL.into_iter().any(|s| s.is_set(params))
}

/// Of `on_record` (what a route refused before), those `params` carry, which
/// its sends leave out.
pub(super) fn carried(on_record: &BTreeSet<Sampler>, params: &Params) -> BTreeSet<Sampler> {
    on_record
        .iter()
        .copied()
        .filter(|s| s.is_set(params))
        .collect()
}

/// The parameter `e` (a refused send's error) refuses, when its words are
/// one of the known wordings (module doc) and it is one `sent` carried.
/// OpenAI's relayed error names it in `param` too, with the code
/// `unsupported_parameter` or `unsupported_value`.
pub(super) fn refused(e: &GatewayError, sent: &Params) -> Option<Sampler> {
    refused_where(e, |s| s.is_set(sent))
}

/// Of `on_record`, those a forwarded JSON `body` carries
/// ([`Sampler::in_body`]), which its sends leave out.
pub(crate) fn carried_in_body(on_record: &BTreeSet<Sampler>, body: &Value) -> BTreeSet<Sampler> {
    on_record
        .iter()
        .copied()
        .filter(|s| s.in_body(body))
        .collect()
}

/// Whether a forwarded JSON body carries any parameter a refusal could name.
pub(crate) fn any_in_body(body: &Value) -> bool {
    Sampler::ALL.into_iter().any(|s| s.in_body(body))
}

/// [`refused`] for a forwarded JSON body: the parameter `e` refuses, when it
/// is one `body` carried as a top-level key ([`Sampler::in_body`]).
pub(crate) fn refused_in_body(e: &GatewayError, body: &Value) -> Option<Sampler> {
    refused_where(e, |s| s.in_body(body))
}

/// The parameter `e` refuses in a known wording, when `carried` says the
/// send had it.
fn refused_where(e: &GatewayError, carried: impl Fn(Sampler) -> bool) -> Option<Sampler> {
    let by_param = match e {
        GatewayError::UpstreamRelayed(r)
            if matches!(
                r.code.as_deref(),
                Some("unsupported_parameter" | "unsupported_value")
            ) =>
        {
            r.param.as_deref().and_then(Sampler::named)
        }
        _ => None,
    };
    let named = by_param.or_else(|| e.upstream_message().and_then(named_in));
    named.filter(|s| carried(*s))
}

/// The parameter a refusal's message names in a known wording.
fn named_in(message: &str) -> Option<Sampler> {
    // OpenAI: "Unsupported parameter: 'X' is not supported with this
    // model." and "Unsupported value: 'X' does not support … with this
    // model. …"
    for lead in ["Unsupported parameter: '", "Unsupported value: '"] {
        if let Some(rest) = message.split_once(lead).map(|(_, r)| r) {
            let (word, tail) = rest.split_once('\'')?;
            let tail = tail.trim_start();
            let worded = tail.starts_with("is not supported with this model")
                || (tail.starts_with("does not support ") && tail.contains("with this model"));
            return worded.then(|| Sampler::named(word)).flatten();
        }
    }
    // Anthropic: "`X` is deprecated for this model."
    let (head, _) = message.split_once(" is deprecated for this model")?;
    let word = head.rsplit(char::is_whitespace).next()?;
    Sampler::named(word)
}

/// The names of `set`, in the header's order.
pub(crate) fn names(set: &BTreeSet<Sampler>) -> Vec<&'static str> {
    set.iter().map(|s| s.name()).collect()
}

/// What `route`'s model refused before, by its memory ([`super::Learned`]).
pub(crate) fn on_record(state: &SharedState, route: &Route) -> BTreeSet<Sampler> {
    state.reasoning_learned.params_refused(route)
}

/// The INFO line of a send to `route` that leaves out `set` (not empty),
/// which its model refused on an earlier request.
pub(crate) fn say_left_out(route: &Route, set: &BTreeSet<Sampler>) {
    tracing::info!(
        upstream = %route.upstream.name,
        model = %route.upstream_model,
        "{} left out of the request: the model refused it before",
        names(set).join(", "),
    );
}

/// The INFO line of a refusal of `gone` (`message`, the provider's words)
/// that is retried once without it.
pub(crate) fn say_retrying(route: &Route, gone: Sampler, message: &str) {
    tracing::info!(
        upstream = %route.upstream.name,
        model = %route.upstream_model,
        "the provider refused {} ({message}); retrying once without it",
        gone.name(),
    );
}

/// The refused attempt's row note: the provider's `message`, and what lmgw
/// did about `gone`.
pub(crate) fn refused_note(message: &str, gone: Sampler) -> String {
    format!(
        "{message} — the model refused {}; lmgw retried once without it, and leaves it out of \
         this model's requests when it answers",
        gone.name()
    )
}

/// The retry without `set` answered: remembered for `route` and left out of
/// its later sends, an INFO line saying so.
pub(crate) fn remember(state: &SharedState, route: &Route, set: &BTreeSet<Sampler>) {
    state.reasoning_learned.remember_params_refused(route, set);
    tracing::info!(
        upstream = %route.upstream.name,
        model = %route.upstream_model,
        "{} left out of this model's requests from now on: it refused them",
        names(set).join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Relayed;

    fn sent() -> Params {
        Params {
            temperature: Some(0.0),
            top_p: Some(0.9),
            stop: vec!["5".into()],
            ..Default::default()
        }
    }

    fn upstream(message: &str) -> GatewayError {
        GatewayError::Upstream {
            status: 400,
            provider_type: Some("invalid_request_error".into()),
            message: message.into(),
        }
    }

    #[test]
    fn the_known_wordings_name_their_parameter() {
        let p = sent();
        assert_eq!(
            refused(&upstream("`temperature` is deprecated for this model."), &p),
            Some(Sampler::Temperature)
        );
        assert_eq!(
            refused(&upstream("`top_p` is deprecated for this model."), &p),
            Some(Sampler::TopP)
        );
        assert_eq!(
            refused(
                &upstream("Unsupported parameter: 'top_p' is not supported with this model."),
                &p
            ),
            Some(Sampler::TopP)
        );
        assert_eq!(
            refused(
                &upstream(
                    "Unsupported value: 'temperature' does not support 0 with this model. Only \
                     the default (1) value is supported."
                ),
                &p
            ),
            Some(Sampler::Temperature)
        );
        let relayed = GatewayError::UpstreamRelayed(Box::new(Relayed {
            status: 400,
            error_type: Some("invalid_request_error".into()),
            code: Some("unsupported_parameter".into()),
            param: Some("top_p".into()),
            message: "Some new wording.".into(),
        }));
        assert_eq!(refused(&relayed, &p), Some(Sampler::TopP));
    }

    #[test]
    fn anything_else_is_no_parameter_refusal() {
        let p = sent();
        // `stop` changes the answer, not its sampling: OpenAI's refusal
        // reaches the client (module doc).
        assert_eq!(
            refused(
                &upstream("Unsupported parameter: 'stop' is not supported with this model."),
                &p
            ),
            None
        );
        // Not sent: nothing to leave out.
        assert_eq!(
            refused(&upstream("`top_k` is deprecated for this model."), &p),
            None
        );
        // The off's domain, and parameters lmgw does not model.
        assert_eq!(
            refused(
                &upstream(
                    "Unsupported value: 'reasoning_effort' does not support 'none' with this \
                     model."
                ),
                &p
            ),
            None
        );
        assert_eq!(
            refused(
                &upstream("Unsupported parameter: 'max_tokens' is not supported with this model."),
                &p
            ),
            None
        );
        // Unknown wording.
        assert_eq!(
            refused(&upstream("temperature must be between 0 and 1"), &p),
            None
        );
        assert_eq!(
            refused(
                &upstream("`temperature` and `top_p` cannot both be specified for this model."),
                &p
            ),
            None
        );
        assert_eq!(refused(&GatewayError::Timeout, &p), None);
    }

    /// The native passthrough's reading: a forwarded body's top-level keys.
    #[test]
    fn a_forwarded_body_is_read_and_cut_by_its_top_level_keys() {
        let mut body = serde_json::json!({"model": "m", "temperature": 0.5, "top_p": null,
            "text": {"temperature": 1}, "stop": ["5"]});
        let refusal = |w: &str| {
            upstream(&format!(
                "Unsupported parameter: '{w}' is not supported with this model."
            ))
        };
        assert!(any_in_body(&body));
        assert_eq!(
            refused_in_body(&refusal("temperature"), &body),
            Some(Sampler::Temperature)
        );
        // A `null` is unset, as the ingress reads it; `stop` is not modelled.
        assert_eq!(refused_in_body(&refusal("top_p"), &body), None);
        assert_eq!(refused_in_body(&refusal("stop"), &body), None);
        let on_record = BTreeSet::from([Sampler::Temperature, Sampler::TopP, Sampler::Seed]);
        assert_eq!(
            carried_in_body(&on_record, &body),
            BTreeSet::from([Sampler::Temperature])
        );
        Sampler::Temperature.remove_from(&mut body);
        assert_eq!(
            body,
            serde_json::json!({"model": "m", "top_p": null, "text": {"temperature": 1},
                "stop": ["5"]}),
            "only the top-level key goes"
        );
        assert!(!any_in_body(&body));
    }
}
