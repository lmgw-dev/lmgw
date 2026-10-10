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
use super::refusal::{self, Retry};
use super::{Fitted, Lesson};

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
/// it is handed.
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
///   asked again.
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
    build: F,
) -> Result<Sent, GatewayError>
where
    F: Fn(&Route, &Params) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    let protocol = route.upstream.protocol;
    // What a lesson learned here says was refused: the form this route's
    // decision gives without one — the one sent first, or, when that came
    // from an earlier lesson, what that lesson said was refused.
    let first = match fitted.learned() {
        true => state.reasoning_learned.get(route).map(|l| l.refused),
        false => fitted.retryable().cloned(),
    };
    let mut current = params.clone();
    let mut guided_left = true;
    let mut retried = false;
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
        let sent = crate::llama_facts::observe(state, hold, route, sent).await?;
        let Sent::Upstream(resp) = sent else {
            return Ok(sent);
        };
        let status = resp.status();
        if status.is_success() && retried {
            if let (Some(refused), Some((instead, fallback))) = (first.clone(), fitted.sent()) {
                let now = instead.describe(protocol);
                state.reasoning_learned.remember(
                    route,
                    Lesson {
                        refused,
                        instead,
                        fallback,
                    },
                );
                tracing::info!(
                    upstream = %route.upstream.name,
                    model = %route.upstream_model,
                    "reasoning off on this model goes out as {now} from now on"
                );
            }
        }
        let Some(off) = fitted.retryable().filter(|_| hold.is_none()).cloned() else {
            return Ok(Sent::Upstream(resp));
        };
        if !matches!(status.as_u16(), 400 | 422) {
            return Ok(Sent::Upstream(resp));
        }
        let (version, headers) = (resp.version(), resp.headers().clone());
        let bytes = resp.bytes().await.map_err(GatewayError::from)?;
        let refused = crate::egress::for_protocol(protocol).map_error(status.as_u16(), &bytes);
        let message = refused.to_string();
        // A refusal lmgw can tell is about something else — one it maps to an
        // error of its own (llama-server's context refusal), or a prompt too
        // long for the context — stands as it came: without the off it would
        // be refused again, the prompt sent twice for nothing.
        let elsewhere = !matches!(refused, GatewayError::Upstream { .. })
            || refusal::about_the_context(&message);
        let next = match elsewhere {
            true => None,
            false => refusal::next_after_refusal(protocol, &off, &message, guided_left),
        };
        let Some((instead, how)) = next else {
            return Ok(Sent::Upstream(crate::gate::rebuild(
                status, version, headers, bytes,
            )));
        };

        let (was, now) = (off.describe(protocol), instead.describe(protocol));
        let why = match how {
            Retry::Guided => "retrying once with what it says the model takes".to_string(),
            Retry::Fallback => "retrying with no reasoning control, so the model reasons as it \
                 does by default — the last resort"
                .to_string(),
        };
        tracing::info!(
            upstream = %route.upstream.name,
            model = %route.upstream_model,
            "reasoning off: the provider refused {was} ({message}); {why}: {now}"
        );
        // The refused attempt ran nothing upstream.
        if let Some(p) = prompt_sent {
            p.store(false, Ordering::Relaxed);
        }
        let note = match how {
            Retry::Guided => format!(
                "{message} — reasoning off was sent as {was}; lmgw retried once with {now}, and \
                 keeps that for this model when it answers"
            ),
            Retry::Fallback => format!(
                "{message} — reasoning off was sent as {was}; lmgw retried with {now} (the model \
                 then reasons as it does by default), and keeps that for this model when it \
                 answers"
            ),
        };
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
        instead.apply(&mut current);
        fitted.retried(instead, how);
        guided_left = false;
        retried = true;
    }
}

impl RowAs<'_> {
    /// The same identity again, for one more refused attempt's row.
    fn again(&self) -> RowAs<'_> {
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
async fn record_refused(
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
