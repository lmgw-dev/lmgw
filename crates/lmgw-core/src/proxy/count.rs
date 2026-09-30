//! `POST /v1/count_tokens`: the universal token counter (§6), and what the two
//! compatibility counters — `count_messages.rs` (`/v1/messages/count_tokens`)
//! and `tokenize.rs` (`/tokenize`) — share with it (api-docs design §5.1):
//! [`Count`] and its [`Approx`] flags, the header that carries them, the key
//! policy all three apply, and the send-and-parse of a native count.
//!
//! **One counter, thin adapters.** lmgw has one token counter that works on
//! every backend; the other two routes exist because real clients call them,
//! and each is its client's wire shape around this module's machinery. Any
//! number that is not what the backend would count for that exact request
//! says so in `x-lmgw-count-approximate` ([`COUNT_APPROXIMATE_HEADER`]) —
//! there are no silent approximations.

use std::time::Instant;

use axum::http::header::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::config::{Protocol, Route};
use crate::egress::{for_protocol, with_timeout, CountPlan};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::telemetry::{RequestClass, TelemetryBus};
use crate::vram::LocalHold;

use super::*;

/// Why a count is not exactly what the backend would count for the request it
/// was made for (api-docs design §5.1). Declaration order is the header's
/// order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Approx {
    /// The request's structure was counted as plain text: chat template,
    /// tool-definition and per-message overhead are not in the number, so
    /// the real prompt is larger.
    Flattened,
    /// The backend's tokenizer is unknown; tiktoken `o200k_base` stood in
    /// ([`CountPlan::Guessed`]).
    TokenizerGuess,
    /// Images are counted at the model's per-image upper bound
    /// ([`crate::gate::image_token_bound`]), not at what they cost.
    MediaBound,
    /// Image or audio parts are not in the number at all.
    MediaOmitted,
    /// `/v1/count_tokens` on a backend that counts messages, not strings: the
    /// text went as one user message, and that message's framing is in the
    /// number.
    MessageFraming,
}

impl Approx {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flattened => "flattened",
            Self::TokenizerGuess => "tokenizer_guess",
            Self::MediaBound => "media_bound",
            Self::MediaOmitted => "media_omitted",
            Self::MessageFraming => "message_framing",
        }
    }
}

/// A token count, and everything that makes it less than exact — sorted and
/// deduplicated, so the header reads the same however the flags were found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Count {
    pub tokens: u64,
    pub approx: Vec<Approx>,
}

impl Count {
    /// What the backend counts for this request, exactly.
    pub fn exact(tokens: u64) -> Self {
        Self {
            tokens,
            approx: Vec::new(),
        }
    }

    /// Record one more reason the number is not exact.
    pub fn flag(&mut self, a: Approx) {
        if let Err(at) = self.approx.binary_search(&a) {
            self.approx.insert(at, a);
        }
    }

    fn flagged(mut self, a: Approx) -> Self {
        self.flag(a);
        self
    }
}

/// `x-lmgw-count-approximate`'s value for `a`: the flags' names, comma
/// separated in declaration order, each once. `None` when the count is exact —
/// the header is then absent, never empty.
pub(crate) fn approx_header(a: &[Approx]) -> Option<HeaderValue> {
    let mut flags = a.to_vec();
    flags.sort();
    flags.dedup();
    if flags.is_empty() {
        return None;
    }
    let joined = flags
        .iter()
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(",");
    // Only ASCII identifiers and commas: always a valid header value.
    HeaderValue::from_str(&joined).ok()
}

/// Stamp a count's flags on the counter's response ([`approx_header`]).
pub(crate) fn stamp_count(mut resp: Response, count: &Count) -> Response {
    if let Some(v) = approx_header(&count.approx) {
        resp.headers_mut()
            .insert(HeaderName::from_static(COUNT_APPROXIMATE_HEADER), v);
    }
    resp
}

/// The key's alias scope, for a counter (api-docs design §5.1, §5.4; finding
/// F3): the refusal is logged, a success is not. No budget, the key's or the
/// gateway's: a count costs nothing ([`crate::policy::check_scope`], §12
/// entry 15).
///
/// Before this, a key scoped to other aliases could count on any alias — and
/// cold-load a local model doing so, since counting on one starts its
/// container. The row a refusal writes closes a `request_started`, which a
/// counter never makes for its unlogged successes; so it is opened only once
/// there is a refusal to log, and a client that hangs up while the row is
/// being written abandons it ([`InFlight`]) — the live gauge of requests in
/// flight never ends up one over (review R1 #7).
pub(super) async fn counter_policy(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    alias: &str,
) -> Option<GatewayError> {
    let e =
        crate::policy::check_scope(&state.snapshot(), ctx.client_key.as_deref(), alias).err()?;
    let in_flight = InFlight::start(&state.telemetry);
    record_refusal(
        state,
        proto,
        ctx,
        alias,
        Instant::now(),
        RequestClass::Chat,
        &e,
    )
    .await;
    in_flight.finished();
    Some(e)
}

/// One `request_started` that a refusal row is about to close. Dropped
/// before [`Self::finished`] — the handler's future dropped mid-write, when
/// the client hung up — it abandons the request instead, since no row will
/// close it now.
pub(super) struct InFlight<'a> {
    telemetry: &'a TelemetryBus,
    finished: bool,
}

impl<'a> InFlight<'a> {
    pub(super) fn start(telemetry: &'a TelemetryBus) -> Self {
        telemetry.request_started();
        Self {
            telemetry,
            finished: false,
        }
    }

    /// The row was written, and closed the request.
    pub(super) fn finished(mut self) {
        self.finished = true;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.telemetry.request_abandoned();
        }
    }
}

/// `POST /v1/count_tokens` — uniform token counter (§6). Takes `{model, input}`
/// and routes by `model` exactly like the chat endpoints, counting via the
/// resolved backend's native mechanism (llama.cpp `/tokenize`, Anthropic
/// `/v1/messages/count_tokens`, Gemini `:countTokens`) or — for real OpenAI
/// upstreams — locally with the model's tiktoken encoding. The response shape
/// is always `{"model": <alias>, "tokens": <n>}`, independent of backend, with
/// `x-lmgw-count-approximate` when the number is not the backend's own exact
/// count of `input` (api-docs design §5.1, §10 choice 3).
///
/// This is a lightweight metadata utility, so a count is not written to the
/// request log or live feed — but a key refused by its alias scope is
/// (api-docs design §5.4). No budget applies to a count (§12 entry 15).
pub async fn handle_count_tokens(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    // Errors are reported in OpenAI shape (this is lmgw's own endpoint).
    let proto = ClientProto::OpenaiChat;
    let alias = match body.get("model").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            return error_response(proto, &GatewayError::BadRequest("missing 'model'".into()));
        }
    };
    let input = match body.get("input") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return error_response(
                proto,
                &GatewayError::BadRequest("missing 'input' (must be a string)".into()),
            );
        }
    };
    if let Some(e) = counter_policy(&state, proto, &ctx, &alias).await {
        return error_response(proto, &e);
    }

    match count_alias(&state, &alias, &input, false).await {
        Ok((count, headers)) => stamp_count(
            headers
                .stamp(axum::Json(json!({"model": alias, "tokens": count.tokens})).into_response()),
            &count,
        ),
        // Same reasoning as [`Failed`]: a count that failed *on the fallback*
        // is still attributed to it, so the header is on the error too.
        Err((headers, e)) => headers.stamp(error_response(proto, &e)),
    }
}

/// Count `input`'s tokens on the upstream `alias` resolves to, using that
/// upstream's own tokenizer (§6). The real count, not an estimate — which is
/// what lets ingestion size its extraction windows from a model's actual
/// context instead of a guessed characters-per-token ratio.
///
/// The [`GateHeaders`] name the fallback alias when the count was made against
/// one instead of the requested model (gpu-hold design §4) — the HTTP handler
/// stamps them as `x-lmgw-fallback`. It matters more here than it looks:
/// a count is what sizes a caller's context window, and a number produced by a
/// *different* tokenizer than the one the caller thinks it asked for is a
/// silently wrong window unless something says so.
///
/// `pinned`: the caller sizes something with *this* model's tokenizer
/// (quickdoc ingest's extraction windows), so admission never swaps it to a
/// fallback when VRAM outside lmgw's control is short — it waits for room
/// ([`crate::gate::Routed::admit_pinned`]). Ingest refuses the hold itself,
/// before it counts.
pub(crate) async fn count_tokens_inner(
    state: &SharedState,
    alias: &str,
    input: &str,
    pinned: bool,
) -> Result<(u64, GateHeaders), (GateHeaders, GatewayError)> {
    count_alias(state, alias, input, pinned)
        .await
        .map(|(count, headers)| (count.tokens, headers))
}

/// [`count_tokens_inner`] with the count's [`Approx`] flags kept, for the
/// HTTP handler that reports them.
async fn count_alias(
    state: &SharedState,
    alias: &str,
    input: &str,
    pinned: bool,
) -> Result<(Count, GateHeaders), (GateHeaders, GatewayError)> {
    // The gate's per-request half, and only that half: a count is not a chat
    // send, so it reserves nothing in a guarded model's pool. llama.cpp's
    // `/tokenize` is answered by the model's own container, so counting
    // tokens on a cold local model loads it — metadata call or not, that is a
    // load, and it goes through admission like any other (§9b).
    let check = crate::gate::RouteCheck::Text("/v1/count_tokens");
    let opened = if pinned {
        crate::gate::open_pinned(state, alias, check).await
    } else {
        crate::gate::open(state, alias, check).await
    }
    .map_err(|f| (f.headers, f.error))?;
    // Everything past the resolve can fail *on the fallback*, so the count and
    // its failure both leave through one place that names it — see [`Failed`].
    match count_text_route(state, input, &opened.route, opened.hold.as_ref()).await {
        Ok(count) => Ok((count, opened.headers)),
        Err(e) => Err((opened.headers, e)),
    }
}

/// [`count_tokens_inner`] with the alias already resolved and admitted — the
/// half that can fail against the upstream rather than against the routing
/// table. On a candidate alias whose model went away under the count, the
/// answer is `503 candidate_lost`: a count is not re-picked (candidate-aliases
/// §12 entry 76).
///
/// Also the "everything else" branch of `/v1/messages/count_tokens`, which
/// counts its flattened request here (api-docs design §5.2). The flags are the
/// route's own: [`Approx::TokenizerGuess`] for a stand-in tiktoken encoding,
/// [`Approx::MessageFraming`] where the backend only counts messages.
pub(crate) async fn count_text_route(
    state: &SharedState,
    input: &str,
    route: &Route,
    hold: Option<&LocalHold>,
) -> Result<Count, GatewayError> {
    let egress = for_protocol(route.upstream.protocol);
    match egress.build_count_tokens(&state.http, &route.upstream, &route.upstream_model, input)? {
        CountPlan::Ready(n) => Ok(Count::exact(n)),
        CountPlan::Guessed(n) => Ok(Count::exact(n).flagged(Approx::TokenizerGuess)),
        CountPlan::Request(_) => {
            // Rebuilt per attempt rather than reusing the plan's builder: the
            // retry (§3.2) goes to a different port, so the request has to be
            // rendered against the route the send is actually made on.
            let n = send_count(route, hold, |r| {
                match egress.build_count_tokens(
                    &state.http,
                    &r.upstream,
                    &r.upstream_model,
                    input,
                )? {
                    CountPlan::Request(rb) => Ok(rb),
                    // The plan cannot change shape between two builds against
                    // the same upstream protocol; this arm exists because the
                    // type says it could.
                    CountPlan::Ready(_) | CountPlan::Guessed(_) => Err(GatewayError::Internal(
                        "token counting changed plan between attempts".into(),
                    )),
                }
            })
            .await?;
            let count = Count::exact(n);
            // Anthropic and Gemini count a request, not a string: the text
            // went as one user turn (`build_count_tokens`), and that turn's
            // framing is in the number.
            Ok(match route.upstream.protocol {
                Protocol::Anthropic | Protocol::Gemini => count.flagged(Approx::MessageFraming),
                Protocol::Openai => count,
            })
        }
    }
}

/// Send one native count request and read its number: the request `build`
/// renders against the route the attempt goes to, bounded by the route's own
/// timeout, through [`crate::vram::send_local`]'s dead-container policy; an
/// error status mapped by the route's egress, a success read by its
/// [`crate::egress::Egress::parse_count`].
pub(super) async fn send_count<F>(
    route: &Route,
    hold: Option<&LocalHold>,
    build: F,
) -> Result<u64, GatewayError>
where
    F: Fn(&Route) -> Result<reqwest::RequestBuilder, GatewayError>,
{
    let egress = for_protocol(route.upstream.protocol);
    let timeout = route.upstream.request_timeout();
    let resp = crate::vram::send_local(hold, route, None, |r| {
        build(r).map(|rb| with_timeout(rb, timeout))
    })
    .await?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    if !status.is_success() {
        return Err(egress.map_error(status.as_u16(), &bytes));
    }
    egress.parse_count(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_header_is_sorted_and_deduped() {
        assert_eq!(approx_header(&[]), None);
        let v = approx_header(&[
            Approx::MediaOmitted,
            Approx::Flattened,
            Approx::TokenizerGuess,
            Approx::Flattened,
        ])
        .unwrap();
        assert_eq!(v, "flattened,tokenizer_guess,media_omitted");
        assert_eq!(
            approx_header(&[Approx::MessageFraming]).unwrap(),
            "message_framing"
        );
    }

    /// Review R1 #7: a refusal row that is never written — the client hung up
    /// mid-write and the handler's future was dropped — leaves the gauge where
    /// it was; a written one is closed by the row itself (`record`), not here.
    #[test]
    fn an_unwritten_refusal_leaves_the_gauge_balanced() {
        let bus = TelemetryBus::new();
        drop(InFlight::start(&bus));
        assert_eq!(bus.stats().active_requests, 0);

        InFlight::start(&bus).finished();
        assert_eq!(
            bus.stats().active_requests,
            1,
            "still open: the row it wrote is what closes it"
        );
    }

    #[test]
    fn a_count_keeps_its_flags_sorted_and_once() {
        let mut c = Count::exact(7);
        c.flag(Approx::MediaOmitted);
        c.flag(Approx::Flattened);
        c.flag(Approx::MediaOmitted);
        assert_eq!(c.approx, vec![Approx::Flattened, Approx::MediaOmitted]);
        assert_eq!(c.tokens, 7);
    }
}
