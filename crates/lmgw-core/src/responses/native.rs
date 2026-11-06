//! Native `/v1/responses` passthrough (§21, openai egress design §5.2): the
//! body forwarded to an upstream that implements the API itself, its answer
//! relayed untouched, and its row written once the answer has ended, with the
//! tokens and cost the upstream's Response object reported.
//!
//! **Metering.** A whole answer is read whole and its `usage` taken from the
//! body. A stream is relayed as it comes, and [`Meter`] reads each SSE event
//! on the way past without holding any of them back. It keeps the terminal
//! event's Response object (`response.completed`, `.incomplete`, `.failed`)
//! and an `error` event's message. The row is priced like every other one
//! ([`proxy::record_passthrough`]), so budgets and key windows see this spend
//! as they see the synthesized loop's. A Response whose `output` holds a
//! hosted tool's billable call (`web_search_call`, `code_interpreter_call`,
//! …) carries a fee its `usage` does not: that row is unpriced, its note
//! naming the calls (openai egress design §6.1).
//!
//! **Hang-ups.** Everything from the send on runs as a task of its own: the
//! whole answer's under [`crate::detached::detached`], the stream's relay
//! spawned like the chat relay's (`proxy/chat_stream.rs`). A client that goes
//! away therefore still gets its row.
//! - A whole answer: the forward waits for the headers and then the body,
//!   each bounded by the upstream's `request_timeout` where one is set, and
//!   reads on when its client hangs up (*changed in WP10*, openai egress
//!   design §14.9, as the chat unary read does, §13.4): the upstream works
//!   on it either way, so the row is the `499` with the body's usage,
//!   counted as answered, and the budget gets its cost.
//!
//! **Refused parameters** ([`refused_params`], api-compat design §9): a
//! `400` (or `422`) that refuses one modelled sampling parameter the body
//! carries (`temperature`, `top_p`, …) is sent once more without exactly
//! that top-level key, with the chat sends' matcher and per-route memory
//! (`proxy/reasoning_fit/params.rs`). The refused attempt gets its own row,
//! and later requests leave the parameter out before their first send. The
//! answer names what this forward's sends left out in
//! `x-lmgw-params-refused`. A stream's refusal comes before its first byte,
//! so its retry is invisible but for the header.
//!
//! **The reasoning off** ([`reasoning_off`], model-capabilities design
//! §5.6): fitted to the model as the chat sends fit it, with the same rule
//! and the same per-route memory (`proxy::reasoning_fit`), and written into
//! the body's `reasoning.effort` — `"none"` where the model takes the off,
//! its lowest level where it cannot stop, no `effort` key where it has no
//! control. The fit comes after the out-of-quota skip, so a hop that sends
//! nothing reads no facts from its upstream. A refusal of the off is
//! retried as on chat: once with what the refusal names, as the last resort
//! with no control, each refused attempt its own row, and the form that
//! answered remembered for the route — only when every refusal it retried
//! was about the control alone, not about a hosted tool or another key the
//! body carried beside it. An off that went out in another form than
//! `"none"` is `enabled` in `x-lmgw-reasoning-ignored`. An explicit level
//! goes out as asked, and a refusal of it stands, as on chat. The retries
//! compose with the parameters' in one loop ([`Forward::send`]): each
//! parameter is taken out at most once, and the off lowered at most twice
//! (guided, then no control), so it ends. A refusal whose body was not read
//! is retried by nothing. A stream whose client left after a `4xx` refusal
//! sends no retry: its one row is the `499` with nothing charged, since
//! nothing is out after the free refusal ([`retries`]). A refused reasoning
//! summary has no retry here yet (the chat sends have one,
//! `proxy/reasoning_fit/summary.rs`).
//!
//! **Handed on** ([`Native::HandOn`], §14.4): a hop with a fallback whose
//! upstream is remembered out of quota is not sent at all; one that answers
//! an exhausted quota, or cannot be connected to, writes its own row saying
//! where it went. The handler then serves the fallback (`super::serve`).
//! - A stream whose client leaves before the terminal event is cut at once,
//!   not drained, which closes the upstream connection and ends the
//!   generation; its row says `canceled` with whatever usage had arrived. A
//!   client that leaves after the terminal event leaves a whole row: the
//!   usage was in that event. A silence longer than `request_timeout`
//!   (keep-alives do not break one, `proxy::silence`) ends the relay as
//!   `timeout`; it, and a body that broke, end the client's stream with
//!   `response.failed` ([`failed`]).
//!
//! **Ends without usage.** A stream that ended with no terminal event and no
//! error, and a whole body with no usage to read, are marked
//! [`INCOMPLETE`]: an unpriced row that says why, rather than a clean
//! success no budget sees.

use std::collections::BTreeSet;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Route;
use crate::egress::openai_wire::{is_billable, read_response, ResponseReport, TERMINAL_EVENTS};
use crate::egress::{apply_bearer_auth, for_protocol};
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ingress::responses as wire;
use crate::ir::Usage;
use crate::pricing::{HostedCalls, Quantities};
use crate::proxy::reasoning_fit::params::Sampler;
use crate::proxy::reasoning_fit::{Fitted, OffRetry};
use crate::proxy::{self, RequestCtx, INCOMPLETE};
use crate::state::SharedState;
use crate::telemetry::RESPONSES_PROTO;

mod count;
mod failed;
mod hand_on;
mod reasoning_off;
mod refused_params;
mod retries;
pub(super) use hand_on::{HandedRow, HeldRow};

/// What a native forward came to.
pub(super) enum Native {
    /// The answer, or the error, its client gets. `fitted`: what this
    /// forward's own sends did with the reasoning off ([`reasoning_off`],
    /// `x-lmgw-reasoning-ignored`) and the sampling parameters they left out
    /// because the model refused them, now or on an earlier request
    /// (`x-lmgw-params-refused`, [`refused_params`]).
    Answered { resp: Response, fitted: Fitted },
    /// The hop could not answer for `reason` (an exhausted quota, an
    /// unreachable upstream): its fallback answers instead. `error` is what
    /// the hop would answer, for a fallback that cannot take the request
    /// after all. `row` is the sent attempt's own row (§14.7), which the
    /// handler writes once the walk handed the request on — never for a
    /// walk that ends, whose one row is the hop's error (review B5).
    HandOn {
        error: GatewayError,
        reason: FallbackReason,
        row: Option<Box<hand_on::HandedRow>>,
    },
}

impl Native {
    /// An answer whose sends left nothing out (or that made none).
    fn answered(resp: Response) -> Self {
        Self::Answered {
            resp,
            fitted: Fitted::default(),
        }
    }
}

/// Forward the body to an upstream that implements `/v1/responses` itself,
/// rewriting only the model name, the reasoning effort (an off fitted to the
/// model, [`reasoning_off`]) and any input item's
/// `call_id` that carries a Gemini thought signature (to its bare id). The
/// answer reaches the client untouched, so reasoning items, hosted-tool
/// events and anything the provider adds later arrive intact, which is the
/// whole reason for not synthesizing here. `degraded`: what the body's
/// content lost to a fallback that cannot see, for its row
/// (`request_logs.degraded`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn native_passthrough(
    state: &SharedState,
    ctx: &RequestCtx,
    (route, fallback): (&Route, Option<FallbackReason>),
    body: &Value,
    started: Instant,
    control: &crate::ir::ReasoningControl,
    degraded: Option<String>,
    tried: String,
) -> Native {
    // Set when this handler is dropped before the forward answered it: its
    // client went away (hyper drops the handler's future then).
    let (hangup, gone) = tokio::sync::watch::channel(false);
    let _hangup = Hangup(hangup);
    let alias = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // The hop this forward is for — the fallback it is, or the name asked
    // for — and its own fallback, which an exhausted quota or an
    // unreachable upstream hands it to (§14.4).
    let hop = route.fallback.as_deref().unwrap_or(&alias).to_string();
    // The whole items and unsealed reasoning items whose ids this caller
    // was given: the platform's own, which go as they are; the rest go
    // without their ids, or not at all (`super::loop_items`).
    let given = given_items(state, ctx, body).await;
    let snap = state.snapshot();
    let hop_fallback = snap
        .hop_key(&hop)
        .and_then(|k| snap.fallback_of(&k).map(String::from));
    let forward = Forward {
        state: state.clone(),
        ctx: ctx.clone(),
        route: route.clone(),
        fallback,
        alias,
        hop,
        hop_fallback,
        tried,
        streamed: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        ask: crate::ir::FormatAsk::of_type(
            body.pointer("/text/format/type").and_then(Value::as_str),
        ),
        body: upstream_body(route, body, &given),
        control: control.clone(),
        started,
        degraded,
        gone,
        // `store: false` keeps nothing upstream, so there is nothing to
        // fetch again (openai egress design §6).
        stored: body.get("store").and_then(Value::as_bool) != Some(false),
        requested_tier: body
            .get("service_tier")
            .and_then(Value::as_str)
            .map(String::from),
        left_out: Default::default(),
        params_retried: false,
        // Fitted in `run`, once the hop is not skipped (§5.6).
        fitted: Fitted::default(),
        off_retry: OffRetry::none(),
    };
    match crate::detached::detached("a native /v1/responses forward", forward.run()).await {
        Ok(native) => native,
        Err(refusal) => {
            let e = GatewayError::Internal(refusal.message);
            Native::answered(
                (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response(),
            )
        }
    }
}

/// Which of the items `body` names whose fate is the caller's record
/// (`super::loop_items::goes_as_given`) an answer given to this caller
/// carried. A lookup that fails is a WARN and none: the items then go
/// without their ids, or not at all, which names nothing of anyone's.
async fn given_items(
    state: &SharedState,
    ctx: &RequestCtx,
    body: &Value,
) -> std::collections::HashSet<String> {
    let ids = super::loop_items::given_candidates(body);
    crate::store::recorded_item_ids(&state.db, &ids, super::reader(ctx))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("looking up the input's items: {e}; they go without their ids");
            Default::default()
        })
}

/// The body the upstream gets, but for the reasoning control, which
/// [`Forward::run`] writes in once it has fitted the off
/// ([`reasoning_off::write_control`]); `given`: the ids this caller was
/// given ([`given_items`]).
fn upstream_body(route: &Route, body: &Value, given: &std::collections::HashSet<String>) -> Value {
    let mut upstream_body = body.clone();
    upstream_body["model"] = Value::String(route.upstream_model.clone());
    // A loop transcript's message and call items carry ids lmgw made up
    // (WP8 review W-R1): they go without them; its reasoning items without
    // encrypted content are left out (WP8 verify V-F1).
    super::loop_items::forward_as_given(&mut upstream_body, given);
    // An earlier step answered by Gemini (a fallback, an alias switched
    // mid-conversation) left its thought signatures in the call ids; they
    // stay with Gemini, as on every synthesized send (gateway design §7.1).
    crate::ir::wire_call_ids_in_responses_body(&mut upstream_body);
    // A reasoning item lmgw's own loop answered with goes as the platform
    // wrote it, to its row only (openai egress design §7.3.2).
    super::seals::unwrap_for(
        &mut upstream_body,
        &crate::ir::SealScope::of(&route.upstream),
    );
    // The platform names the serving tier only to a request that asked for
    // one; a flagged compatible row shares this forward and gets nothing.
    if route.upstream.protocol == crate::config::Protocol::Openai {
        crate::egress::openai::ask_for_the_tier(&mut upstream_body);
    }
    upstream_body
}

/// Marks its forward's client as gone when dropped. A stream's handler is
/// dropped once it has handed over the body, so only the whole answer's
/// forward reads it; the stream relay hears its client through its channel.
struct Hangup(tokio::sync::watch::Sender<bool>);

impl Drop for Hangup {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// [`INCOMPLETE`]'s message on a stream.
const STREAM_INCOMPLETE: &str = crate::pricing::STREAM_NO_USAGE;

/// [`INCOMPLETE`]'s message on a whole body.
const BODY_INCOMPLETE: &str = crate::pricing::ANSWER_NO_USAGE;

/// [`INCOMPLETE`]'s message, and the price note, of a stream whose terminal
/// event reported no usage.
const TERMINAL_NO_USAGE: &str = "the upstream's terminal event reported no usage";

/// The price note of a stream its client left before the terminal event.
const STREAM_CANCELED: &str = crate::pricing::CLIENT_LEFT_NO_USAGE;

/// How a bounded wait on the upstream ended.
enum Waited<T> {
    Done(T),
    TimedOut,
    /// The client hung up first.
    Gone,
}

/// One forward, owned by the task that runs it.
struct Forward {
    state: SharedState,
    ctx: RequestCtx,
    route: Route,
    fallback: Option<FallbackReason>,
    /// The alias the client asked for.
    alias: String,
    /// The hop this forward is for: the fallback it is, or `alias`.
    hop: String,
    /// The hop's own fallback, as configured.
    hop_fallback: Option<String>,
    /// The chain the request came along, as a refusal names it (" (tried:
    /// A → B)"; empty before the first fallback, §14.8).
    tried: String,
    streamed: bool,
    /// What `text.format` asks, so an error classifier can read a refusal
    /// (structured-output design §9, §11.8).
    ask: crate::ir::FormatAsk,
    body: Value,
    /// The resolved reasoning control, written into [`Self::body`] once the
    /// off is fitted ([`Self::fit_control`]).
    control: crate::ir::ReasoningControl,
    started: Instant,
    degraded: Option<String>,
    /// Whether the handler was dropped: [`Hangup`].
    gone: tokio::sync::watch::Receiver<bool>,
    /// Whether the upstream keeps the response: its id is then remembered
    /// ([`Self::remember`]).
    stored: bool,
    /// The body's `service_tier`, what the row's price note names beside
    /// the tier that served it (openai egress design §5.6).
    requested_tier: Option<String>,
    /// The sampling parameters taken out of [`Self::body`] because the
    /// model refused them, on this request or an earlier one
    /// ([`refused_params`]).
    left_out: BTreeSet<Sampler>,
    /// Whether one of them was refused on this request: remembered for the
    /// route once a send answers.
    params_retried: bool,
    /// The reasoning off as this forward's sends carry it
    /// ([`reasoning_off`]); no off when the request asked for none.
    fitted: Fitted,
    /// The retries a refusal of that off earns on this request.
    off_retry: OffRetry,
}

/// The row's error for an answer its client did not stay for.
fn canceled() -> Option<(String, String)> {
    Some(("canceled".into(), "client disconnected".into()))
}

/// The row's error for a whole answer whose client left before it was sent.
fn disconnected(why: &str) -> Option<(String, String)> {
    Some((proxy::CLIENT_DISCONNECTED.to_string(), why.to_string()))
}

/// How a forward's sends ended without a 2xx ([`Forward::send`]).
enum Ended {
    /// The client hung up first.
    Gone,
    TimedOut,
    /// The send failed before an answer.
    SendFailed(reqwest::Error),
    /// The upstream refused with this status, read as this error, after
    /// any retry a refused parameter or off earned ([`refused_params`],
    /// [`reasoning_off`]).
    Refused(StatusCode, GatewayError),
    /// A stream's client left after the upstream refused a send with a
    /// `4xx` — while its error was read, or before a retry it would have
    /// earned was decided — so nothing that was sent is being worked on
    /// ([`retries`]). The status, and the refusal's words once read.
    LeftAfterRefusal(StatusCode, Option<String>),
}

impl Forward {
    async fn run(mut self) -> Native {
        // A hop on an upstream remembered out of quota, with a fallback:
        // nothing is sent and no row written (§14.6).
        if let Some(skipped) = self.remembered() {
            return skipped;
        }
        // The off fitted to the model, now that something will be sent.
        self.fit_control().await;
        // What the model refused before goes out of the body first.
        self.leave_out_on_record();
        // In flight from the send on; the row written at the end closes it.
        self.state.telemetry.request_started();
        let sent = self.send().await;
        let mut fitted = self.fitted.clone();
        fitted.set_params_refused(self.left_out.clone());
        let native = match sent {
            Ok(resp) => {
                self.state.upstream_quota.clear(
                    self.route.upstream.id,
                    &self.route.upstream.name,
                    "a request to it succeeded",
                );
                Native::answered(self.answered(resp).await)
            }
            Err(Ended::Gone) => Native::answered(self.unanswered().await),
            Err(Ended::LeftAfterRefusal(status, words)) => {
                Native::answered(self.left_after_refusal(status, words).await)
            }
            Err(Ended::TimedOut) => Native::answered(self.failed(GatewayError::Timeout).await),
            Err(Ended::SendFailed(e)) => self.send_failed(e).await,
            // An exhausted quota is handed on here, after the parameter's
            // and the off's retries: a refused parameter or off is no quota.
            Err(Ended::Refused(status, e)) => self.refused(status, e).await,
        };
        match native {
            Native::Answered { resp, .. } => Native::Answered { resp, fitted },
            // A fallback answers under its own headers: what this hop left
            // out, or sent its off as, says nothing of it.
            hand_on @ Native::HandOn { .. } => hand_on,
        }
    }

    /// Send the body until the upstream answers a 2xx, or something else
    /// ends it: a refusal of a sampling parameter the body carries is sent
    /// once more without it ([`refused_params`]), a refusal of the off it
    /// carries once more in the form the refusal leaves
    /// ([`reasoning_off`]), every other one is the forward's
    /// [`Ended::Refused`]. Each retry takes one parameter out (once per
    /// parameter) or lowers the off (at most twice: once guided, once with
    /// no control), so the loop ends. A refusal whose body was not read
    /// (empty, or its read timed out) earns no retry: there are no words to
    /// go by.
    async fn send(&mut self) -> Result<reqwest::Response, Ended> {
        let url = format!("{}/responses", self.route.upstream.base());
        loop {
            let attempt = Instant::now();
            let rb = apply_bearer_auth(
                self.state.http.post(&url).json(&self.body),
                &self.route.upstream,
            );
            let resp = match self.wait(rb.send()).await {
                Waited::Gone => return Err(Ended::Gone),
                Waited::TimedOut => return Err(Ended::TimedOut),
                Waited::Done(Err(e)) => return Err(Ended::SendFailed(e)),
                Waited::Done(Ok(r)) => r,
            };
            let status = resp.status();
            if status.is_success() {
                self.learned();
                self.off_answered();
                return Ok(resp);
            }
            let answer_headers = resp.headers().clone();
            let bytes = match self.wait(resp.bytes()).await {
                // A refusal: the upstream took no prompt to work on. A 5xx
                // may have been worked on, and is the plain hang-up, whose
                // prompt is the budgets' lower bound.
                Waited::Gone if status.is_client_error() => {
                    return Err(Ended::LeftAfterRefusal(status, None))
                }
                Waited::Gone => return Err(Ended::Gone),
                Waited::Done(b) => b.unwrap_or_default(),
                Waited::TimedOut => Bytes::new(),
            };
            // `map_error`'s backstop can return `GatewayError::ContextExceeded`
            // with an empty `model` — it has no route to name one. Fill it in
            // the same way `gate::fit::attribute` does at every other local
            // chat site (review finding 6): this is native passthrough, but
            // the model still overflowed *this* row's context.
            let e = crate::gate::attribute(
                crate::egress::chat_error(
                    for_protocol(self.route.upstream.protocol),
                    status.as_u16(),
                    &answer_headers,
                    &bytes,
                    self.ask,
                ),
                &self.route,
            );
            // An unread refusal has no words to retry by (above).
            if bytes.is_empty() || !matches!(status.as_u16(), 400 | 422) {
                return Err(Ended::Refused(status, e));
            }
            // A stream's client that left meanwhile is sent no retry, and
            // the refused attempt writes no row saying one went out: it was
            // free, and nothing else went out ([`retries`]).
            if self.left() {
                return Err(Ended::LeftAfterRefusal(status, Some(e.log_text())));
            }
            let retry = self.retry_without_refused(status, &e, attempt).await
                || self.retry_with_fitted_off(status, &e, attempt).await;
            if !retry {
                return Err(Ended::Refused(status, e));
            }
        }
    }

    /// The upstream's 2xx: the stream relayed, or the whole answer read.
    async fn answered(self, resp: reqwest::Response) -> Response {
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(if self.streamed {
                "text/event-stream"
            } else {
                "application/json"
            })
            .to_string();
        let ttfb = self.started.elapsed().as_millis() as i64;
        let builder = Response::builder()
            .status(StatusCode::OK)
            // The upstream's own answer, relayed: a test's compatibility
            // check reads it as the fake upstream's (`egress::watch`).
            .extension(crate::egress::watch::Relayed)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no");
        if self.streamed {
            return builder.body(self.relay(resp, ttfb)).unwrap();
        }

        let bytes = match self.wait(resp.bytes()).await {
            Waited::Gone => return self.unanswered().await,
            Waited::TimedOut => return self.failed(GatewayError::Timeout).await,
            Waited::Done(Err(e)) => return self.failed(GatewayError::from(e)).await,
            Waited::Done(Ok(b)) => b,
        };
        let body_json = serde_json::from_slice::<Value>(&bytes).ok();
        let report = body_json.as_ref().map(read_response).unwrap_or_default();
        if let Some(id) = &report.id {
            self.remember(id).await;
            self.remember_items(id, &report.items).await;
        }
        // A body that arrived after its client left is the `499` with its
        // usage (`proxy/unanswered.rs`), counted as answered: it was. One
        // that says the upstream failed is that failure all the same.
        let (status, error) = if let Some(m) = report.failed.clone() {
            (StatusCode::OK.as_u16(), Some(("upstream".to_string(), m)))
        } else if *self.gone.borrow() {
            (
                proxy::CLIENT_CLOSED_STATUS,
                disconnected("client disconnected before the answer was sent"),
            )
        } else if report.usage == Usage::default() {
            (
                StatusCode::OK.as_u16(),
                Some((INCOMPLETE.to_string(), BODY_INCOMPLETE.to_string())),
            )
        } else {
            (StatusCode::OK.as_u16(), None)
        };
        // An answer without usage is unpriced for that reason, and its row
        // says so in the price note as well as in its error (§5.2). On the
        // platform a local count of what was sent and answered bounds it
        // (openai egress design §13.4).
        let no_usage = !report.usage.reported();
        let note = no_usage.then_some(BODY_INCOMPLETE);
        let local_count = match no_usage {
            true => {
                let output = body_json.as_ref().map(count::output_text);
                self.local_count(output.unwrap_or_default()).await
            }
            false => None,
        };
        self.record(
            status,
            Some(ttfb),
            error,
            Some(&report),
            &report.hosted_calls,
            note,
            local_count,
        )
        .await;
        builder.body(Body::from(bytes)).unwrap()
    }

    /// `work`, bounded by the upstream's `request_timeout` where one is set.
    /// A stream's is given up on as soon as the client hangs up — dropping
    /// `work` then is what closes the upstream connection; a whole answer's
    /// reads on (module doc, *changed in WP10*).
    async fn wait<T>(&self, work: impl std::future::Future<Output = T>) -> Waited<T> {
        let bounded = async {
            match self.route.upstream.request_timeout() {
                Some(d) => tokio::time::timeout(d, work).await.ok(),
                None => Some(work.await),
            }
        };
        if !self.streamed {
            return match bounded.await {
                Some(v) => Waited::Done(v),
                None => Waited::TimedOut,
            };
        }
        let mut gone = self.gone.clone();
        tokio::select! {
            biased;
            done = bounded => match done {
                Some(v) => Waited::Done(v),
                None => Waited::TimedOut,
            },
            // Err: the sender is gone, which only its drop does, and that
            // marks the client gone first.
            _ = gone.wait_for(|g| *g) => Waited::Gone,
        }
    }

    /// The row of an answer whose client left before the upstream answered
    /// (a whole one, or a stream before its 2xx): the plain `499`, with
    /// unknown usage, saying so, and on the platform the prompt's local
    /// count as the budgets' lower bound.
    async fn unanswered(self) -> Response {
        let error = disconnected(
            "client disconnected before the upstream answered; the upstream reported no usage",
        );
        // The request went out, and the upstream may be working on it: on
        // the platform its prompt, counted locally, is the budgets' lower
        // bound (openai egress design §13.4, after the WP9 review).
        let local_count = self.local_count(String::new()).await;
        let reported = proxy::Reported {
            service_tier: crate::pricing::ServedTier::of(
                self.route.upstream.protocol,
                None,
                self.requested_tier.clone(),
            ),
            price_note: local_count.map(|_| crate::pricing::CLIENT_LEFT_UNANSWERED.to_string()),
            local_count,
            ..Default::default()
        };
        proxy::record_passthrough(
            &self.state,
            &self.ctx,
            RESPONSES_PROTO,
            &self.alias,
            &self.route,
            self.fallback,
            self.started,
            None,
            false,
            proxy::CLIENT_CLOSED_STATUS,
            error,
            self.degraded.clone(),
            &reported,
            Quantities::default(),
        )
        .await;
        // Nobody reads it.
        StatusCode::from_u16(proxy::CLIENT_CLOSED_STATUS)
            .unwrap_or(StatusCode::BAD_REQUEST)
            .into_response()
    }

    /// The stream's body: the upstream's bytes as they come, relayed by a
    /// task of its own that writes the row when the stream has ended.
    fn relay(self, resp: reqwest::Response, ttfb: i64) -> Body {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
        tokio::spawn(async move {
            // A device's connection gets `TCP_USER_TIMEOUT` for as long as the
            // stream runs, and its accept-time timers back after it (device
            // link design §4.3).
            let _timers = crate::server::device_listener::chat_stream_timers(
                self.ctx.conn.as_ref(),
                &self.state.snapshot().settings,
            );
            let mut upstream = resp.bytes_stream();
            // Only the platform's cut stream is counted locally (§13.4):
            // elsewhere the deltas pass unparsed.
            let mut meter =
                Meter::counting(self.route.upstream.protocol == crate::config::Protocol::Openai);
            // The response's id once read: what its items are recorded under.
            let mut response_id: Option<String> = None;
            let mut error: Option<(String, String)> = None;
            // Each silence past the headers is bounded by the upstream's
            // timeout, as on the chat relay (`proxy::silence`).
            let mut silence = proxy::silence::Silence::new(self.route.upstream.request_timeout());
            // Whether the upstream ended the body itself.
            let mut ended = false;
            loop {
                let next = silence.bound(upstream.next());
                let next = tokio::select! {
                    biased;
                    // The client's end is gone (hyper read the connection's
                    // EOF): heard here, not only at the next event that fails
                    // to send, which a long reasoning phase puts minutes away.
                    // Cut, not drained (openai egress design §6).
                    () = tx.closed() => {
                        if !meter.finished() {
                            error = canceled();
                        }
                        break;
                    }
                    next = next => next,
                };
                match next {
                    // A silence longer than the upstream's timeout, as the
                    // chat relay reads one (`drive_upstream_until`).
                    None => {
                        if !meter.finished() {
                            error = Some((proxy::silence::KIND.into(), silence.message()));
                        }
                        break;
                    }
                    Some(None) => {
                        ended = true;
                        break;
                    }
                    Some(Some(Ok(b))) => {
                        silence.hears(!b.is_empty(), &meter.feed(&b));
                        // Before the event that names it reaches the client,
                        // so a client that fetches it at once finds it.
                        if let Some(id) = meter.take_id() {
                            self.remember(&id).await;
                            response_id = Some(id);
                        }
                        // Likewise its items: a client may name them in its
                        // next request as soon as it has them.
                        let items = meter.take_items();
                        if let Some(id) = &response_id {
                            self.remember_items(id, &items).await;
                        }
                        if tx.send(Ok(b)).await.is_err() {
                            if !meter.finished() {
                                error = canceled();
                            }
                            break;
                        }
                    }
                    // After the terminal event the answer is whole: what
                    // broke later does not undo it.
                    Some(Some(Err(e))) => {
                        if !meter.finished() {
                            error = Some(("transport".into(), e.to_string()));
                        }
                        break;
                    }
                }
            }
            // Closing the upstream connection is what ends a generation its
            // client left.
            drop(upstream);
            // An upstream that fell silent or broke before the terminal
            // event: its client is told so in the API's own terms
            // ([`Meter::failed_event`]) before the stream ends. One that
            // already said so with an `error` event is not told twice, and
            // its row says what the upstream said (a `response.failed` is a
            // terminal event: nothing is cut after it).
            let cut = error.as_ref().filter(|(kind, _)| kind != "canceled");
            if let Some(msg) = cut.map(|(_, msg)| msg.clone()) {
                match meter.error() {
                    Some(said) => error = Some(("upstream".into(), said)),
                    None => {
                        let _ = tx.send(Ok(Bytes::from(meter.failed_event(&msg)))).await;
                    }
                }
            }
            if ended {
                meter.finish();
            }
            if error.is_none() {
                error = meter.error().map(|m| ("upstream".to_string(), m));
            }
            if error.is_none() && !meter.finished() {
                error = Some((INCOMPLETE.to_string(), STREAM_INCOMPLETE.to_string()));
            }
            // A terminal event whose `usage` is null (a `response.failed`,
            // an `incomplete` one) reported none either.
            if error.is_none() && !meter.usage_reported() {
                error = Some((INCOMPLETE.to_string(), TERMINAL_NO_USAGE.to_string()));
            }
            // A stream cut before its terminal event never reported usage,
            // nor did one whose terminal event had none: the row's price
            // note says why it is unpriced (§5.2).
            let note = (!meter.usage_reported()).then(|| match error.as_ref() {
                Some((kind, _)) if kind == "canceled" => STREAM_CANCELED,
                _ if meter.finished() => TERMINAL_NO_USAGE,
                _ => STREAM_INCOMPLETE,
            });
            // OpenAI does not store a response cut before its terminal event
            // (a GET answers 404): the record and its items go, so a later
            // fetch gets lmgw's own not_found.
            if !meter.finished() {
                if let Some(id) = &response_id {
                    if let Err(e) = crate::store::delete_native_response(&self.state.db, id).await {
                        tracing::warn!("forgetting cut native response {id}: {e}");
                    }
                }
            }
            // Its usage is gone for good: a local count bounds what the
            // budgets see of it (openai egress design §13.4), whether the
            // stream was cut or its terminal event reported none.
            let local_count = match meter.usage_reported() {
                true => None,
                false => self.local_count(meter.relayed().to_string()).await,
            };
            self.record(
                StatusCode::OK.as_u16(),
                Some(ttfb),
                error,
                meter.report.as_ref(),
                &meter.hosted_calls(),
                note,
                local_count,
            )
            .await;
        });
        Body::from_stream(ReceiverStream::new(rx))
    }

    /// Remember the upstream's response `id`, so `GET`, `DELETE` and
    /// `input_items` for it reach the upstream that made it
    /// (`responses/retrieve.rs`). A `store: false` answer is recorded as not
    /// stored: never fetched again, only what its items are recorded under
    /// ([`Self::remember_items`]). A failure is a log line: the answer
    /// itself is fine.
    async fn remember(&self, id: &str) {
        // An empty id names nothing to fetch again, and would be one record
        // shared by every answer that carries it.
        if id.is_empty() {
            return;
        }
        let record = crate::store::NativeResponse {
            id: id.to_string(),
            upstream_id: self.route.upstream.id,
            key_id: self.ctx.principal.key_id(),
            model: self.alias.clone(),
            upstream_model: self.route.upstream_model.clone(),
            stored: self.stored,
        };
        if let Err(e) = crate::store::insert_native_response(&self.state.db, &record).await {
            tracing::warn!("remembering native response {id}: {e}");
        }
    }

    /// Record the output item ids of the response `id` under this request's
    /// key: the items a client key may name in a later request
    /// (`responses/references.rs`, migration 0093). A failure is a log
    /// line, as [`Self::remember`]'s is.
    async fn remember_items(&self, id: &str, items: &[crate::ir::UpstreamItem]) {
        if id.is_empty() || items.is_empty() {
            return;
        }
        let key = self.ctx.principal.key_id();
        if let Err(e) =
            crate::store::insert_native_response_items(&self.state.db, id, key, items).await
        {
            tracing::warn!("remembering the items of native response {id}: {e}");
        }
    }

    /// lmgw's own count of a stream cut before its terminal event (openai
    /// egress design §13.4): the body as it was sent and `relayed`, the
    /// output text the client was relayed, each counted once with the
    /// platform's tokenizer, on a blocking thread. `None` on a flagged
    /// `openai_compatible` row: lmgw knows no tokenizer for what an
    /// aggregator serves, so its cut stream has no floor yet.
    async fn local_count(&self, relayed: String) -> Option<crate::pricing::LocalCount> {
        if self.route.upstream.protocol != crate::config::Protocol::Openai {
            return None;
        }
        crate::egress::openai::count_locally(
            self.route.upstream_model.clone(),
            count::prompt_text(&self.body),
            relayed,
        )
        .await
    }

    /// What a Response object reported, for the row: its usage, the tier
    /// that served it on an `openai` row, the billable hosted calls `hosted`,
    /// `note` as the price note and lmgw's own `local_count` of an answer
    /// whose usage never came. `None` when no Response object was read.
    fn reported(
        &self,
        report: Option<&ResponseReport>,
        hosted: &HostedCalls,
        note: Option<&str>,
        local_count: Option<crate::pricing::LocalCount>,
    ) -> proxy::Reported {
        proxy::Reported {
            usage: report.map(|r| r.usage).unwrap_or_default(),
            service_tier: crate::pricing::ServedTier::of(
                self.route.upstream.protocol,
                report.and_then(|r| r.service_tier.clone()),
                self.requested_tier.clone(),
            ),
            price_note: note.map(str::to_string),
            // Hosted tools' fees are in no `usage`: a call that bills one
            // leaves the row unpriced, with a note (§6.1).
            hosted_calls: hosted.clone(),
            local_count,
        }
    }

    /// The row of an answer that passed the upstream's 2xx: `status` is the
    /// one its client was sent, or the `499` of one that left. `note` is the
    /// row's price note, and `local_count` lmgw's own count of an answer
    /// whose usage never came ([`Self::local_count`]).
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        status: u16,
        ttfb: Option<i64>,
        error: Option<(String, String)>,
        report: Option<&ResponseReport>,
        hosted: &HostedCalls,
        note: Option<&str>,
        local_count: Option<crate::pricing::LocalCount>,
    ) {
        proxy::record_passthrough(
            &self.state,
            &self.ctx,
            RESPONSES_PROTO,
            &self.alias,
            &self.route,
            self.fallback,
            self.started,
            ttfb,
            self.streamed,
            status,
            error,
            self.degraded.clone(),
            &self.reported(report, hosted, note, local_count),
            // Past the upstream's 2xx: answered, a client gone mid-stream
            // (`canceled`) and a whole answer whose client left (`499`)
            // included (billable-units §4.5), explicitly so that the
            // writer's default does not drop the request for either.
            Quantities::answered(),
        )
        .await;
    }

    /// The row and the answer of a forward that failed before its stream.
    async fn failed(self, e: GatewayError) -> Response {
        let status = e.http_status();
        let payload = wire::serialize_error(&e);
        proxy::record_passthrough(
            &self.state,
            &self.ctx,
            RESPONSES_PROTO,
            &self.alias,
            &self.route,
            self.fallback,
            self.started,
            None,
            false,
            status.as_u16(),
            Some((e.kind().to_string(), e.log_text())),
            self.degraded,
            &proxy::Reported::default(),
            Quantities::default(),
        )
        .await;
        let mut resp = (status, axum::Json(payload)).into_response();
        e.stamp_rate_hints(&mut resp);
        resp
    }
}

/// The stream event that opens a response and names its id.
const CREATED: &str = "response.created";

/// The stream event that carries one finished output item.
const ITEM_DONE: &str = "response.output_item.done";

/// What a relayed Responses stream reported, read event by event as it goes
/// past (openai egress design §5.2).
#[derive(Default)]
pub(super) struct Meter {
    sse: crate::sse::SseDecoder,
    /// The terminal event's Response object, read.
    report: Option<ResponseReport>,
    /// An `error` event's message.
    error: Option<String>,
    /// The response's id, once read and not yet taken ([`Self::take_id`]).
    id: Option<String>,
    /// Whether an id was read at all: only the first counts.
    id_seen: bool,
    /// Output item ids read and not yet taken ([`Self::take_items`]).
    items: Vec<crate::ir::UpstreamItem>,
    /// The billable hosted calls among the items that passed
    /// ([`ITEM_DONE`]): what a stream cut before its terminal event made.
    passed_calls: HostedCalls,
    /// The output text the deltas relayed ([`count::DELTA_EVENTS`]), what
    /// a stream cut before its terminal event is counted by (openai egress
    /// design §13.4). Counted once at the end, never per delta: counting
    /// fragments one by one overcounts.
    relayed: String,
    /// Whether the deltas are read for [`Self::relayed`] at all: only where
    /// a local count can be taken (an `openai` row).
    count_text: bool,
    /// `response.created`'s Response object: what a stream failed under it
    /// ends with, marked failed ([`Self::failed_event`]).
    opened: Option<Value>,
    /// The output items finished so far, with their `output_index`: the
    /// failed Response's `output`.
    done_items: Vec<(Option<u64>, Value)>,
    /// The events read so far.
    events: u64,
    /// The last event's data, read for its `sequence_number` only when the
    /// stream fails ([`Self::failed_event`]): most events pass unparsed.
    last_data: Option<String>,
}

impl Meter {
    /// A meter that keeps the relayed text when `count_text` is set, and
    /// passes the delta events over unparsed otherwise.
    pub(super) fn counting(count_text: bool) -> Self {
        Self {
            count_text,
            ..Self::default()
        }
    }

    /// Read the events `chunk` completes; they are returned, for the relay's
    /// silence clock (`proxy::silence`).
    pub(super) fn feed(&mut self, chunk: &[u8]) -> Vec<crate::sse::SseEvent> {
        let events = self.sse.feed(chunk);
        self.events += events.len() as u64;
        if let Some(last) = events.last() {
            self.last_data = Some(last.data.clone());
        }
        for ev in &events {
            // OpenAI names every event on its `event:` line, so the many
            // delta events are passed over unparsed; an event without one
            // is read for its `type`.
            if let Some(name) = ev.event.as_deref() {
                if name != "error"
                    && name != CREATED
                    && name != ITEM_DONE
                    && !TERMINAL_EVENTS.contains(&name)
                    && !(self.count_text && count::DELTA_EVENTS.contains(&name))
                {
                    continue;
                }
            }
            let Ok(data) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            let id = data.pointer("/response/id").and_then(Value::as_str);
            if let Some(id) = id.filter(|_| !self.id_seen) {
                self.id = Some(id.to_string());
                self.id_seen = true;
            }
            match data.get("type").and_then(Value::as_str) {
                Some(t) if TERMINAL_EVENTS.contains(&t) => {
                    if let Some(response) = data.get("response") {
                        let report = read_response(response);
                        self.items.extend(report.items.iter().cloned());
                        self.report = Some(report);
                    }
                }
                Some(ITEM_DONE) => {
                    let Some(item) = data.get("item") else {
                        continue;
                    };
                    let index = data.get("output_index").and_then(Value::as_u64);
                    self.done_items.push((index, item.clone()));
                    // With its encrypted content's hash (WP8 review S-R3).
                    if let Some(recorded) = crate::ir::UpstreamItem::of(item) {
                        self.items.push(recorded);
                    }
                    if let Some(ty) = is_billable(item) {
                        self.passed_calls.add(ty, 1);
                    }
                }
                Some(t) if self.count_text && count::DELTA_EVENTS.contains(&t) => {
                    if let Some(d) = data.get("delta").and_then(Value::as_str) {
                        self.relayed.push_str(d);
                    }
                }
                Some("error") => {
                    let message = data
                        .get("message")
                        .or_else(|| data.pointer("/error/message"))
                        .and_then(Value::as_str)
                        .unwrap_or("the upstream sent an error event");
                    self.error.get_or_insert_with(|| message.to_string());
                }
                Some(CREATED) if self.opened.is_none() => {
                    self.opened = data.get("response").cloned();
                }
                _ => {}
            }
        }
        events
    }

    /// The response's id the first time it is read (`response.created`, or
    /// the terminal event when none came before); `None` after that.
    pub(super) fn take_id(&mut self) -> Option<String> {
        self.id.take()
    }

    /// The output item ids read since the last take: each finished item's
    /// as it passes, and the terminal Response's again. Recording one twice
    /// keeps the first record and adds the hash of the encrypted content
    /// the second carries (WP8 verify V-F4, migration 0100): OpenAI encrypts
    /// the item anew for the terminal event, and a client may replay
    /// either.
    pub(super) fn take_items(&mut self) -> Vec<crate::ir::UpstreamItem> {
        std::mem::take(&mut self.items)
    }

    /// The billable hosted calls the answer made: the terminal Response's
    /// when it came, which names every one, else those of the items that
    /// passed before the stream was cut (openai egress design §6.1).
    pub(super) fn hosted_calls(&self) -> HostedCalls {
        match &self.report {
            Some(r) => r.hosted_calls.clone(),
            None => self.passed_calls.clone(),
        }
    }

    /// The output text the deltas relayed (§13.4).
    pub(super) fn relayed(&self) -> &str {
        &self.relayed
    }

    /// The upstream ended its body: a last event it did not end with a blank
    /// line is still its last event.
    pub(super) fn finish(&mut self) {
        let _ = self.feed(b"\n\n");
    }

    /// Whether the terminal event has passed: the answer is whole.
    pub(super) fn finished(&self) -> bool {
        self.report.is_some()
    }

    /// Whether the terminal event reported usage: not before it, and not
    /// for one whose `usage` is null (a `response.failed` may have none).
    pub(super) fn usage_reported(&self) -> bool {
        self.report.as_ref().is_some_and(|r| r.usage.reported())
    }

    /// The usage the terminal event reported; none before it.
    #[cfg(test)]
    pub(super) fn usage(&self) -> Usage {
        self.report.as_ref().map(|r| r.usage).unwrap_or_default()
    }

    /// What went wrong upstream, as the stream said: an `error` event, or a
    /// `response.failed`.
    pub(super) fn error(&self) -> Option<String> {
        self.error
            .clone()
            .or_else(|| self.report.as_ref().and_then(|r| r.failed.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    #[test]
    fn the_meter_reads_the_completed_events_usage_across_chunks() {
        let mut m = Meter::default();
        m.feed(
            event(
                "response.created",
                json!({"type": "response.created",
            "response": {"status": "in_progress", "usage": null}}),
            )
            .as_bytes(),
        );
        m.feed(
            event(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "Hi"}),
            )
            .as_bytes(),
        );
        assert!(!m.finished());
        let done = event(
            "response.completed",
            json!({"type": "response.completed",
            "response": {"status": "completed", "usage": {
                "input_tokens": 9, "input_tokens_details": {"cached_tokens": 4},
                "output_tokens": 3, "output_tokens_details": {"reasoning_tokens": 1}}}}),
        );
        let (a, b) = done.as_bytes().split_at(17);
        m.feed(a);
        assert!(!m.finished(), "half an event is no event");
        m.feed(b);
        assert!(m.finished());
        let u = m.usage();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(9), Some(3)));
        assert_eq!(
            (u.cached_input_tokens, u.reasoning_tokens),
            (Some(4), Some(1))
        );
        assert_eq!(m.error(), None);
    }

    #[test]
    fn the_meter_hands_over_the_response_id_once() {
        let mut m = Meter::default();
        m.feed(
            event(
                "response.created",
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
            )
            .as_bytes(),
        );
        assert_eq!(m.take_id().as_deref(), Some("resp_1"));
        assert_eq!(m.take_id(), None);
        m.feed(
            event(
                "response.completed",
                json!({"type": "response.completed", "response": {"id": "resp_1",
                       "status": "completed"}}),
            )
            .as_bytes(),
        );
        assert_eq!(m.take_id(), None, "taken already");
    }

    /// Only a counting meter (an `openai` row's) keeps the deltas' text;
    /// every other one passes them over unparsed.
    #[test]
    fn only_a_counting_meter_keeps_the_relayed_text() {
        let delta = event(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "Hi"}),
        );
        let mut counting = Meter::counting(true);
        counting.feed(delta.as_bytes());
        assert_eq!(counting.relayed(), "Hi");
        let mut plain = Meter::counting(false);
        plain.feed(delta.as_bytes());
        assert_eq!(plain.relayed(), "");
    }

    #[test]
    fn an_event_without_a_name_is_read_for_its_type() {
        let mut m = Meter::default();
        let data = json!({"type": "response.incomplete",
            "response": {"status": "incomplete", "usage": {"input_tokens": 2, "output_tokens": 8}}});
        m.feed(format!("data: {data}\n\n").as_bytes());
        assert!(m.finished());
        assert_eq!(m.usage().completion_tokens, Some(8));
    }

    #[test]
    fn a_failed_or_error_event_is_the_rows_error() {
        let mut m = Meter::default();
        m.feed(
            event(
                "error",
                json!({"type": "error", "code": "server_error",
            "message": "overloaded"}),
            )
            .as_bytes(),
        );
        assert_eq!(m.error().as_deref(), Some("overloaded"));
        assert!(!m.finished());

        let mut m = Meter::default();
        m.feed(
            event(
                "response.failed",
                json!({"type": "response.failed",
            "response": {"status": "failed", "error": {"message": "boom"},
                         "usage": {"input_tokens": 5, "output_tokens": 0}}}),
            )
            .as_bytes(),
        );
        assert_eq!(m.error().as_deref(), Some("boom"));
        assert_eq!(m.usage().prompt_tokens, Some(5));
    }

    #[test]
    fn a_cut_stream_keeps_the_hosted_calls_that_passed_and_the_terminal_report_wins() {
        let item = |ty: &str, id: &str| {
            event(
                ITEM_DONE,
                json!({"type": ITEM_DONE, "item": {"type": ty, "id": id, "status": "completed"}}),
            )
        };
        let mut m = Meter::default();
        m.feed(item("web_search_call", "ws_1").as_bytes());
        m.feed(item("message", "msg_1").as_bytes());
        m.feed(item("web_search_call", "ws_2").as_bytes());
        assert_eq!(
            m.take_items(),
            ["ws_1", "msg_1", "ws_2"].map(crate::ir::UpstreamItem::plain)
        );
        let calls: Vec<_> = m
            .hosted_calls()
            .iter()
            .map(|(t, n)| (t.to_string(), n))
            .collect();
        assert_eq!(
            calls,
            [("web_search_call".to_string(), 2)],
            "cut here: what passed"
        );

        // The terminal Response names every call: its count, not the sum.
        m.feed(
            event(
                "response.completed",
                json!({"type": "response.completed", "response": {"status": "completed",
                    "output": [{"type": "web_search_call", "id": "ws_1"},
                               {"type": "web_search_call", "id": "ws_2"},
                               {"type": "message", "id": "msg_1"}]}}),
            )
            .as_bytes(),
        );
        let calls: Vec<_> = m
            .hosted_calls()
            .iter()
            .map(|(t, n)| (t.to_string(), n))
            .collect();
        assert_eq!(calls, [("web_search_call".to_string(), 2)]);
        assert_eq!(
            m.take_items(),
            ["ws_1", "ws_2", "msg_1"].map(crate::ir::UpstreamItem::plain)
        );
    }
}
