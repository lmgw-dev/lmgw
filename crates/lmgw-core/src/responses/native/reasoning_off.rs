//! The reasoning off on the native passthrough (model-capabilities design
//! §5.6, the chat sends' mechanic in `proxy/reasoning_fit` on the forwarded
//! body): an off — `x-lmgw-reasoning: off`, an effort `none` from the body,
//! a header or the alias' default — is fitted to the model before the first
//! send, by the same rule and from the same per-route memory as on chat, so
//! a form one route learned on `/v1/chat/completions` is what it gets here
//! and the other way round.
//!
//! The Responses body has one control, `reasoning.effort`, and the forms
//! are written into it ([`write`]): the protocol's own off as
//! `reasoning.effort: "none"` (what the synthesized Responses wire sends for
//! an off, `egress/openai/responses/body.rs`), a model's lowest level as
//! that level, and no reasoning control as no `effort` key at all — the
//! client's other `reasoning` keys (its `summary`) stay, and an object left
//! empty goes.
//!
//! A `400` (or `422`) to a send that carried an off in some form is retried
//! as on chat ([`OffRetry`]): once with what the refusal names, as the last
//! resort with no control, a refusal lmgw can tell is about something else
//! standing at once, and one whose body never arrived too (no words to go
//! by). Each refused attempt is a row of its own whose note says what was
//! retried with. The form that answered is remembered for the route only
//! when every refusal retried was about the control alone: a native body
//! carries what chat never sends (hosted tools, `text`, `include`, …,
//! [`carried`]; function tools and a tool choice are not held against a
//! bare refusal), and "The following tools cannot be used with
//! reasoning.effort 'minimal': web_search" taught every later request of
//! the route, chat included, a level only this one needed. Such a refusal
//! is retried for its request, its note saying nothing is kept and why. A
//! stream's refusal is an HTTP status before its first byte, so its retry
//! is invisible on the wire. An off that went out in another form than
//! `"none"` is `enabled` in the answer's `x-lmgw-reasoning-ignored`.
//!
//! Only the off is fitted, as on chat: an explicit level goes out as asked,
//! and a refusal of it stands. Whether a model reasoned although the off
//! went out is not read from the answer here yet (the in-process Chat reads
//! it; the public routes do not). The retries come after the parameters'
//! (`refused_params.rs`) and before the hop's hand-on decisions
//! (`hand_on.rs`): a refused off is no exhausted quota.

use std::time::Instant;

use axum::http::StatusCode;
use serde_json::{json, Value};

use super::Forward;
use crate::config::Route;
use crate::error::GatewayError;
use crate::ir::ReasoningControl;
use crate::proxy::reasoning_fit::{self, Carried, Fitted, Off, OffRetry, Retried, Spelling};
use crate::state::SharedState;

/// The off in `control` (resolved) fitted to `route`'s model.
async fn fit(state: &SharedState, route: &Route, control: &ReasoningControl) -> Fitted {
    reasoning_fit::fit_off(state, route, control, Spelling::Responses).await
}

/// Write the resolved `control` into `body` — headers over body over alias
/// defaults (§5.2). `reasoning.effort` is the only control this API can
/// carry, so it is the only one written in; the rest are named in the
/// response's `x-lmgw-reasoning-ignored` by the caller. Writing it back is
/// not a no-op even when it came from the body: a header or an alias
/// default may have replaced the level the body asked for. An off goes in
/// the form `fitted` to the model, over an effort the body carried.
fn write_control(body: &mut Value, control: &ReasoningControl, fitted: &Fitted) {
    match fitted.form() {
        Some(off) => write(body, off),
        None => {
            if let Some(effort) = control.effort.as_deref() {
                write_effort(body, Some(effort));
            }
        }
    }
}

/// The top-level keys of a native body that name no feature a refusal of
/// the off could depend on: the model, the input and the control
/// themselves, and what only says how the answer is delivered, kept or
/// attributed.
const NO_FEATURE: [&str; 14] = [
    "model",
    "input",
    "reasoning",
    "stream",
    "stream_options",
    "store",
    "metadata",
    "instructions",
    "user",
    "previous_response_id",
    "conversation",
    "prompt_cache_key",
    "safety_identifier",
    "background",
];

/// What `body` carries besides the control and its input that a refusal of
/// the off could be about, for whether its lesson is kept
/// ([`OffRetry::after`]): each tool's type other than `function`
/// (`web_search`, `code_interpreter`, …), then every top-level key but
/// [`NO_FEATURE`]'s (and a `service_tier` of `auto`, which lmgw asks for
/// itself). Hosted types and keys block a bare refusal's lesson;
/// `tool_choice`, and `tools` when every tool is a function, are ordinary
/// ([`Carried`]).
fn carried(body: &Value) -> Carried {
    let Some(o) = body.as_object() else {
        return Carried::default();
    };
    // The tools' types first: the most telling name a refusal can give.
    let mut out = Carried::default();
    let blocking = &mut out.blocking;
    let tools = o
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for ty in tools.filter_map(|t| t.get("type").and_then(Value::as_str)) {
        if ty != "function" && !blocking.iter().any(|c| c == ty) {
            blocking.push(ty.to_string());
        }
    }
    let hosted = !out.blocking.is_empty();
    for (k, v) in o {
        if NO_FEATURE.contains(&k.as_str())
            || (k.as_str() == "service_tier" && v.as_str() == Some("auto"))
        {
            continue;
        }
        if k == "tool_choice" || (k == "tools" && !hosted) {
            out.ordinary.push(k.clone());
        } else {
            out.blocking.push(k.clone());
        }
    }
    out
}

/// Write `off` into a Responses body's `reasoning.effort`.
pub(super) fn write(body: &mut Value, off: &Off) {
    match off {
        // `BetweenTools` is decided only for an Anthropic route, never a
        // native Responses one: here it is the plain off.
        Off::Control | Off::BetweenTools => write_effort(body, Some("none")),
        Off::Lowest(level) => write_effort(body, Some(level)),
        Off::Omitted => write_effort(body, None),
    }
}

/// Set a Responses body's `reasoning.effort` to `effort`, or take the key
/// out (and a `reasoning` object it leaves empty) for `None`. The object's
/// other keys stay as the client sent them.
pub(super) fn write_effort(body: &mut Value, effort: Option<&str>) {
    let Some(b) = body.as_object_mut() else {
        return;
    };
    match (b.get_mut("reasoning"), effort) {
        (Some(Value::Object(o)), Some(e)) => {
            o.insert("effort".into(), json!(e));
        }
        (_, Some(e)) => {
            b.insert("reasoning".into(), json!({"effort": e}));
        }
        (Some(Value::Object(o)), None) => {
            o.remove("effort");
            if o.is_empty() {
                b.remove("reasoning");
            }
        }
        (_, None) => {}
    }
}

impl Forward {
    /// Fit the request's off to the model and write the resolved control
    /// into the body, before the first send: after the out-of-quota skip,
    /// so a hop that sends nothing reads no facts and logs no form.
    pub(super) async fn fit_control(&mut self) {
        self.fitted = fit(&self.state, &self.route, &self.control).await;
        self.off_retry =
            OffRetry::open(&self.state, &self.route, &self.fitted, Spelling::Responses);
        write_control(&mut self.body, &self.control, &self.fitted);
    }

    /// The upstream refused a send made at `attempt` with `status`, read as
    /// `e`: when that earns the off a retry, the attempt's own row is
    /// written, the next form written into the body, and `true` asks for one
    /// more send.
    pub(super) async fn retry_with_fitted_off(
        &mut self,
        status: StatusCode,
        e: &GatewayError,
        attempt: Instant,
    ) -> bool {
        if !matches!(status.as_u16(), 400 | 422) {
            return false;
        }
        let carried = carried(&self.body);
        let Some(Retried { instead, note }) =
            self.off_retry
                .after(&self.route, &mut self.fitted, e, &carried)
        else {
            return false;
        };
        self.record_refused(status, e, attempt, note).await;
        write(&mut self.body, &instead);
        true
    }

    /// A send answered: after a retry of the off, the form that answered is
    /// remembered for the route.
    pub(super) fn off_answered(&self) {
        self.off_retry
            .answered(&self.state, &self.route, &self.fitted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_body_carries_besides_the_control() {
        let plain = json!({"model": "m", "input": "hi", "stream": true, "store": false,
            "reasoning": {"effort": "none"}, "service_tier": "auto"});
        assert_eq!(carried(&plain), Carried::default());
        let tools = json!({"model": "m", "input": "hi", "service_tier": "flex",
            "tools": [{"type": "web_search"}, {"type": "function", "name": "f"},
                      {"type": "web_search"}]});
        let c = carried(&tools);
        assert_eq!(c.blocking, ["web_search", "service_tier", "tools"]);
        assert!(c.ordinary.is_empty());
        let functions = json!({"model": "m", "input": "hi", "tool_choice": "auto",
            "tools": [{"type": "function", "name": "f"}]});
        let c = carried(&functions);
        assert_eq!(c.ordinary, ["tool_choice", "tools"]);
        assert!(c.blocking.is_empty());
    }

    #[test]
    fn each_form_is_written_into_reasoning_effort() {
        let mut b = json!({"reasoning": {"effort": "high", "summary": "auto"}});
        write(&mut b, &Off::Control);
        assert_eq!(b["reasoning"], json!({"effort": "none", "summary": "auto"}));
        write(&mut b, &Off::Lowest("minimal".into()));
        assert_eq!(
            b["reasoning"],
            json!({"effort": "minimal", "summary": "auto"})
        );
        write(&mut b, &Off::Omitted);
        assert_eq!(
            b["reasoning"],
            json!({"summary": "auto"}),
            "the summary stays"
        );

        let mut b = json!({"input": "hi"});
        write(&mut b, &Off::Control);
        assert_eq!(b["reasoning"], json!({"effort": "none"}));
        write(&mut b, &Off::Omitted);
        assert!(b.get("reasoning").is_none(), "an emptied object goes: {b}");

        // A client's `reasoning` that is no object is not touched by an
        // omitted control.
        let mut b = json!({"reasoning": null});
        write(&mut b, &Off::Omitted);
        assert_eq!(b, json!({"reasoning": null}));
    }
}
