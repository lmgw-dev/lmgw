//! A fitted chat send, with the one retry a refused off may earn (module doc
//! of [`super`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::{CountInput, FallbackReason, Sent, TurnLease};
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, Params, Usage};
use crate::state::SharedState;
use crate::telemetry::RequestClass;
use crate::vram::LocalHold;

use super::super::{record, record_in_process, InProcessLog, KeyRef, LogParams, RequestCtx};
use super::quota::Handover;
use super::refusal::Carried;
use super::{params, Fitted, OffRetry, Spelling};

/// Whom a refused attempt's own request row is written as — the request's
/// own identity and label, as its final row has them.
pub(crate) enum RowAs<'a> {
    /// A public API request (`/v1/chat/completions`, `/v1/messages`).
    Public {
        proto: ClientProto,
        ctx: &'a RequestCtx,
        alias: &'a str,
        fallback: Option<FallbackReason>,
    },
    /// An in-process caller (the Chat, the tool loop, realtime, sampling…).
    InProcess {
        key: KeyRef,
        ingress_proto: &'a str,
        alias: &'a str,
        fallback: Option<FallbackReason>,
    },
}

/// [`crate::gate::send_gated_marked`] for a chat send whose params went
/// through [`super::fit`], with `build` rendering the request for the params
/// it is handed. `hand` is what the request carries to be handed on at the
/// send — an exhausted quota, an unreachable upstream ([`super::quota`]);
/// `None` keeps every answer as it came.
///
/// A `400` to an `openai` send on the Responses wire that refuses its
/// reasoning summary is learned for the route and sent once more without
/// it, its own row saying so ([`super::summary`]); that send counts on its
/// own, beside the off's retries below.
///
/// A `400` (or `422`) to a cloud send whose words refuse one of the
/// parameters it carried (`temperature`, `top_p`, … — [`super::params`]) is
/// sent once more without exactly that parameter, its own row saying which
/// and why; one more per further parameter refused, each at most once. When
/// the retry answers, the parameters are remembered for the route and left
/// out of its later sends, which `fitted` reports
/// ([`Fitted::params_refused`], `x-lmgw-params-refused`).
///
/// When a cloud route sent an off in some form and the provider refuses the
/// request with a `400` (or `422`), the send is made again
/// ([`refusal::next_after_refusal`]): once with what the refusal's message
/// says the model takes, when it names the control, and as the last resort
/// with no reasoning control at all — at most three sends, every one on
/// record:
/// - each refused attempt is its own request row, its message saying what
///   was retried with;
/// - an INFO line says it, and `fitted` reports it (`enabled` ignored);
/// - an answer that is not an error makes the form that answered what this
///   route gets from now on ([`super::Learned`]), so the provider is not
///   asked again — unless a refusal it retried was not about the control
///   alone (`super::off_retry`, [`carried`]): a request's tools, tool choice,
///   format or own keys named beside the control are that request's, and
///   its retry teaches the route nothing. A bare refusal is held against the
///   client's own keys only, not function tools, a tool choice or a format.
///
/// A refusal lmgw can tell is about something else — the prompt does not fit
/// the context — is handed back as it came, its body intact; any other is
/// tried once more without the off, and when that is refused too, that
/// answer stands. A local route never retries: its off is not fitted.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_chat<F>(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    lease: &mut TurnLease,
    ir: &ChatRequest,
    params: &Params,
    stream: bool,
    deadline: Option<Instant>,
    timeout: Option<Duration>,
    prompt_sent: Option<&AtomicBool>,
    fitted: &mut Fitted,
    rows: RowAs<'_>,
    hand: Option<&Handover>,
    build: F,
) -> Result<Sent, GatewayError>
where
    F: Fn(&Route, &Params) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    // An upstream remembered out of quota hands a hop with a fallback on
    // before anything is sent (openai egress design §14.4, §14.6).
    if let Some(r) = super::quota::before(state, hold, route, hand, &rows).await {
        return r;
    }
    let protocol = route.upstream.protocol;
    let mut off_retry = OffRetry::open(state, route, fitted, Spelling::Chat);
    let mut current = params.clone();
    // A route that refused a reasoning summary is not asked for one again
    // (`super::summary`).
    let mut summary_refused = state.reasoning_learned.summary_refused(route);
    current.reasoning_summary_refused = summary_refused;
    // Nor a parameter its model refused (`super::params`): left out, and
    // said so.
    let mut left_out = params::carried(&params::on_record(state, route), params);
    for s in &left_out {
        s.clear(&mut current);
    }
    if !left_out.is_empty() {
        params::say_left_out(route, &left_out);
    }
    fitted.params_refused = left_out.clone();
    let mut params_retried = false;
    loop {
        let started = Instant::now();
        // Boxed: 111 KB of poll frame in a debug build, on the turn -> gate chain.
        let sent = Box::pin(crate::gate::send_gated_marked(
            state,
            hold,
            route,
            lease,
            CountInput::Chat {
                ir,
                params: &current,
                stream,
            },
            deadline,
            timeout,
            |r| build(r, &current),
            prompt_sent,
        ))
        .await;
        let sent = crate::llama_facts::observe(state, hold, route, sent).await;
        // An exhausted quota or an unreachable upstream on a cloud send
        // (§14.4): handed to the hop's fallback, or its error with a note.
        let sent = super::quota::after(
            state,
            hold,
            route,
            hand,
            &rows,
            ir,
            started,
            stream,
            prompt_sent,
            sent,
        )
        .await?;
        let Sent::Upstream(resp) = sent else {
            return Ok(sent);
        };
        let status = resp.status();
        if status.is_success() && params_retried {
            params::remember(state, route, &left_out);
        }
        if status.is_success() {
            off_retry.answered(state, route, fitted);
        }
        let off = hold.is_none() && fitted.retryable().is_some();
        let summary = status.as_u16() == 400 && super::summary::asked(route, ir, &current);
        let param = hold.is_none() && params::any_set(&current);
        if !matches!(status.as_u16(), 400 | 422) || (!off && !summary && !param) {
            return Ok(Sent::Upstream(resp));
        }
        let (version, headers) = (resp.version(), resp.headers().clone());
        let bytes = resp.bytes().await.map_err(GatewayError::from)?;
        // Read as the send site reads it (structured-output design §9): a
        // classified structured-output refusal is not `Upstream`, so it is
        // never retried without the off.
        let refused = crate::egress::chat_error(
            crate::egress::for_protocol(protocol),
            status.as_u16(),
            &headers,
            &bytes,
            ir.format_ask(),
        );
        let message = refused.to_string();
        // A parameter the model refuses is left out of one more send, and
        // that one alone (`super::params`).
        if let Some(gone) = param.then(|| params::refused(&refused, &current)).flatten() {
            params::say_retrying(route, gone, &message);
            if let Some(p) = prompt_sent {
                p.store(false, Ordering::Relaxed);
            }
            let note = params::refused_note(&message, gone);
            record_refused(
                state,
                rows.again(),
                route,
                started,
                stream,
                status.as_u16(),
                (refused.kind(), note),
            )
            .await;
            gone.clear(&mut current);
            left_out.insert(gone);
            fitted.params_refused = left_out.clone();
            params_retried = true;
            continue;
        }
        // A refused summary is asked for no more on this route, and the
        // send goes once more without it (`super::summary`).
        if summary && super::summary::refused(&refused, super::summary::only(&current)) {
            state.reasoning_learned.remember_summary_refused(route);
            tracing::info!(
                upstream = %route.upstream.name,
                model = %route.upstream_model,
                "the provider refused a reasoning summary ({message}); retrying without one, \
                 and asking for none on this model from now on"
            );
            if let Some(p) = prompt_sent {
                p.store(false, Ordering::Relaxed);
            }
            let note = format!(
                "{message} — lmgw asked for a reasoning summary; it retried without one, and \
                 asks for none on this model from now on"
            );
            record_refused(
                state,
                rows.again(),
                route,
                started,
                stream,
                status.as_u16(),
                (refused.kind(), note),
            )
            .await;
            summary_refused = true;
            current.reasoning_summary_refused = true;
            continue;
        }
        // The off's retry (`super::off_retry`): once with what the refusal
        // names, else with no control; a refusal about something else lmgw
        // can tell, or with nothing of the off left to drop, stands.
        let retried = off
            .then(|| off_retry.after(route, fitted, &refused, &carried(ir)))
            .flatten();
        let Some(super::Retried { instead, note }) = retried else {
            return Ok(Sent::Upstream(crate::gate::rebuild(
                status, version, headers, bytes,
            )));
        };
        // The refused attempt ran nothing upstream.
        if let Some(p) = prompt_sent {
            p.store(false, Ordering::Relaxed);
        }
        record_refused(
            state,
            rows.again(),
            route,
            started,
            stream,
            status.as_u16(),
            (refused.kind(), note),
        )
        .await;

        current = params.clone();
        current.reasoning_summary_refused = summary_refused;
        instead.apply(&mut current);
        for s in &left_out {
            s.clear(&mut current);
        }
    }
}

/// What a chat request carries besides the control and its messages that a
/// refusal of its off could be about, for whether its lesson is kept
/// ([`OffRetry::after`]): its tools, tool choice and response format, by
/// the words a provider names them with (ordinary: a refusal naming one is
/// about it, one naming nothing is not held against them), and the client's
/// own top-level keys (`web_search_options`, `verbosity`, …), which block a
/// bare refusal's lesson.
fn carried(ir: &ChatRequest) -> Carried {
    let mut out = Carried::default();
    if !ir.tools.is_empty() {
        out.ordinary
            .extend(["tools", "functions"].map(String::from));
    }
    if ir.tool_choice.is_some() {
        out.ordinary.push("tool_choice".into());
    }
    if ir.response_format.is_some() {
        out.ordinary
            .extend(["response_format", "json_schema"].map(String::from));
    }
    out.blocking.extend(ir.passthrough.keys().cloned());
    out
}

impl RowAs<'_> {
    /// The name the request asked for.
    pub(super) fn alias(&self) -> &str {
        match self {
            RowAs::Public { alias, .. } | RowAs::InProcess { alias, .. } => alias,
        }
    }

    /// Whether the request came in on `/v1/messages`, whose exhausted quota
    /// is Anthropic's `402`.
    pub(super) fn anthropic(&self) -> bool {
        matches!(
            self,
            RowAs::Public {
                proto: ClientProto::AnthropicMessages,
                ..
            }
        )
    }

    /// The same identity again, for one more refused attempt's row.
    pub(super) fn again(&self) -> RowAs<'_> {
        match self {
            RowAs::Public {
                proto,
                ctx,
                alias,
                fallback,
            } => RowAs::Public {
                proto: *proto,
                ctx,
                alias,
                fallback: *fallback,
            },
            RowAs::InProcess {
                key,
                ingress_proto,
                alias,
                fallback,
            } => RowAs::InProcess {
                key: key.clone(),
                ingress_proto,
                alias,
                fallback: *fallback,
            },
        }
    }
}

/// The refused attempt's own row, written like the request's final one. Its
/// own in-flight count too: the request's is closed by its final row.
pub(super) async fn record_refused(
    state: &SharedState,
    rows: RowAs<'_>,
    route: &Route,
    started: Instant,
    streamed: bool,
    status: u16,
    error: (&str, String),
) {
    state.telemetry.request_started();
    match rows {
        RowAs::Public {
            proto,
            ctx,
            alias,
            fallback,
        } => {
            record(
                LogParams {
                    state,
                    proto,
                    ctx,
                    alias: alias.to_string(),
                    route: Some(route),
                    started,
                    streamed,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback,
                    rung: None,
                    degraded: None,
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                status,
                None,
                Usage::default(),
                Some(error),
            )
            .await;
        }
        RowAs::InProcess {
            key,
            ingress_proto,
            alias,
            fallback,
        } => {
            record_in_process(
                InProcessLog {
                    key,
                    ingress_proto,
                    alias,
                    route,
                    started,
                    streamed,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback,
                    rung: None,
                    degraded: None,
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                status,
                None,
                Usage::default(),
                Some(error),
                state,
            )
            .await;
        }
    }
}
