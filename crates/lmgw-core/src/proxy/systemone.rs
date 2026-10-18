//! `POST /v1/systemone`: TypeSafe's System One wire as llama.cpp serves it,
//! a thin relay through admission (decision-models design §2).
//!
//! A decision model answers typed questions about a `state` with calibrated
//! probabilities. The route copies the wire both ways: the client's object
//! goes upstream with only `model` rewritten to the route's upstream model,
//! and the answer comes back as the upstream sent it — status, content type
//! and body, `model` included, which on TypeSafe is the versioned id that
//! answered (§2.1). Which alias answered is `x-lmgw-fallback`'s to say.
//! The upstream's `Retry-After` is relayed too; no other header is, as on `/tokenize`.
//!
//! A local route is an aux row of kind decision; a cloud one is an `openai`
//! or `llama_cpp` upstream (§4.1). Everything else is refused before a
//! container starts ([`crate::gate::RouteCheck::Decision`], §2.2). The body
//! is read whatever its `Content-Type` says, as llama-server reads it, so the
//! route takes bytes rather than the `/v1` groups' `Json<Value>` (§2.1).
//!
//! Errors (§2.7): the upstream's are relayed verbatim — llama.cpp's 500 for
//! a prompt over the ubatch included (§7 Q6) — except a 401 or 403, which is
//! lmgw's own credential and answers 502, and a 404 from a local decision
//! row's container, whose image predates the route and answers a named 501
//! (§3.6). lmgw's own refusals use llama.cpp's error shape with `lmgw_code`
//! ([`llama_error`](super::tokenize::llama_error)); the shared `/v1` layers'
//! 401, 403, 429 and declared-length 413 come before this handler and are
//! OpenAI's.

use std::time::Instant;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::config::{AuxKind, Route};
use crate::egress::openai_wire::SystemOneBody;
use crate::egress::{for_protocol, with_timeout};
use crate::error::GatewayError;
use crate::gate::{Opened, RouteCheck};
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::tokenize::{llama_error_of, read_bytes};
use super::*;

/// The route.
pub const PATH: &str = "/v1/systemone";

/// What a request without `model` is told (§2.1). Nothing else in the body
/// is validated here: the upstream validates its own wire, and a pre-check
/// would answer 400 where TypeSafe answers 422.
const MODEL_REQUIRED: &str = "'model' is required: lmgw routes /v1/systemone by model name, as \
                              TypeSafe requires it — see GET /v1/models";

/// The handler (module doc). Every outcome writes one row: ingress
/// `systemone`, class `aux`, the answer's `input_tokens` and `output_tokens`
/// as its prompt and completion tokens, priced from lmgw's own price rows
/// (§2.6). The key's alias scope and budgets apply: a decision on a cloud
/// model costs money (§2.1 step 3).
pub async fn handle(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = read_bytes(&state, body);
    relay(state, ctx, body).await
}

/// The route in process, for the Decision lab's cells (labs-store design
/// §2.6), as [`super::handle_image_generation`] is the image route's:
/// `body` is what a client would have posted. Everything [`handle`] does
/// after its extractors runs, the request log row included; only the
/// shared `/v1` layers (auth, the declared-length 413) are not in front of
/// it. `max_body_mb` still bounds the bytes, with the 413 the route answers
/// for a body past it.
pub async fn dispatch(state: SharedState, ctx: RequestCtx, body: Bytes) -> Response {
    let max_mb = state.snapshot().settings.max_body_mb;
    let body = if max_mb != 0 && body.len() as u64 > u64::from(max_mb) * 1024 * 1024 {
        Err(GatewayError::BodyTooLarge { max_mb })
    } else {
        Ok(body)
    };
    relay(state, ctx, body).await
}

/// The mark on an answer that would be the same for every item of a set run
/// (labs-store design §3.5): lmgw's own state refused the request before
/// any model could answer it (the policy check; the gate, that is the route
/// check, the GPU hold and a benchmark's lease, admission and its queue, an
/// alias's candidates, the container's start), or the upstream refused
/// lmgw's own credential (a 401 or 403, lmgw's 502). Not a request the model
/// could not take: a body that does not read, a state over the context, the
/// model's own error, a 429 or 5xx, a transport failure or a timeout are the
/// request's or the model's. An in-process caller reads it from the
/// response's extensions; it never goes on the wire. A set run's job ends on
/// one rather than keep it as the item's answer, because the next item would
/// get the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SameForEveryItem;

/// `resp` with the [`SameForEveryItem`] mark.
fn same_for_every_item(mut resp: Response) -> Response {
    resp.extensions_mut().insert(SameForEveryItem);
    resp
}

/// [`handle`] and [`dispatch`] from the body on.
async fn relay(state: SharedState, ctx: RequestCtx, body: Result<Bytes, GatewayError>) -> Response {
    let started = Instant::now();
    let proto = ClientProto::SystemOne;
    // Opens the in-flight gauge, and writes the `499` row should the client
    // go away before the row is written (`unanswered.rs`).
    let mut row = Unanswered::open(&state, proto, &ctx, "?", started, RequestClass::Aux);
    let body = match body.and_then(|b| SystemOneBody::parse(&b)) {
        Ok(b) => b,
        Err(e) => {
            row.failed(&e).await;
            return llama_error_of(&e);
        }
    };
    let alias = match body.model() {
        Some(Ok(alias)) => alias,
        missing_or_not => {
            let e = GatewayError::BadRequest(match missing_or_not {
                Some(Err(sent)) => format!(
                    "'model' must be a string, the name of the model to ask — got {sent}; see \
                     GET /v1/models"
                ),
                _ => MODEL_REQUIRED.into(),
            });
            row.failed(&e).await;
            return llama_error_of(&e);
        }
    };
    row.alias(&alias);
    if let Some(e) = policy_refusal(&state, &ctx, &alias).await {
        row.failed(&e).await;
        return same_for_every_item(llama_error_of(&e));
    }

    row.stage("waiting for admission, or for the model's answer");
    let Opened {
        route,
        hold,
        headers,
    } = match crate::gate::open(&state, &alias, RouteCheck::Decision).await {
        Ok(o) => o,
        Err(f) => {
            if let Some(route) = f.route.as_deref() {
                row.routed_to(route, f.headers.fallback_reason());
            }
            row.failed(&f.error).await;
            return same_for_every_item(f.headers.stamp(llama_error_of(&f.error)));
        }
    };
    row.routed_to(&route, headers.fallback_reason());

    let egress = for_protocol(route.upstream.protocol);
    let timeout = route.upstream.request_timeout();
    let sent = crate::vram::send_local(hold.as_ref(), &route, None, |r| {
        Ok(with_timeout(
            egress.build_systemone(&state.http, &r.upstream, &r.upstream_model, &body)?,
            timeout,
        ))
    })
    .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            row.failed(&e).await;
            return headers.stamp(llama_error_of(&e));
        }
    };
    let status = resp.status();
    let relayed = super::relay::RelayedHead::of(resp.headers());
    let bytes = resp.bytes().await;
    // Read to the end: the container no longer has to stay up for it.
    drop(hold);
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            let e = GatewayError::from(e);
            row.failed(&e).await;
            return headers.stamp(llama_error_of(&e));
        }
    };
    if let Some(e) = answered_by_lmgw(&state, &route, &alias, status, &bytes) {
        row.failed(&e).await;
        let resp = headers.stamp(llama_error_of(&e));
        return if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            same_for_every_item(resp)
        } else {
            resp
        };
    }

    if status.is_success() {
        row.logging();
        record(
            LogParams {
                state: &state,
                proto,
                ctx: &ctx,
                alias,
                route: Some(&route),
                started,
                streamed: false,
                class: RequestClass::Aux,
                timings: None,
                max_tokens_clamped: None,
                fallback: headers.fallback_reason(),
                rung: None,
                degraded: None,
                quantities: Default::default(),
            },
            status.as_u16(),
            None,
            usage_of(&bytes),
            None,
        )
        .await;
    } else {
        // The upstream's own refusal, relayed as it is, is a failed request
        // all the same: its row says what the upstream said.
        let e = crate::gate::attribute(egress.map_error(status.as_u16(), &bytes), &route);
        row.answered(status.as_u16(), Some((e.kind(), e.to_string())))
            .await;
    }
    headers.stamp(relayed.response(status, bytes))
}

/// The two upstream answers lmgw does not relay (§2.7), as the error it
/// answers instead; `None` relays.
///
/// - **401 or 403**: the upstream refusing lmgw's own credential is the
///   gateway's configuration, not the client's request, and every other
///   route answers it with a 502 ([`GatewayError::http_status`]), as
///   `/tokenize` does.
/// - **404 from a local decision row's container**: llama-server answers
///   "File Not Found" on a route its build does not have, so the row's image
///   predates `/v1/systemone` (§3.6). The 501 names the row, and the image
///   when the row resolves one, instead of relaying a bare 404 that reads
///   like an unknown model. It names llama.cpp's first build with the route
///   only beside the running build's own `build_info`: an official build
///   sends one, ik_llama.cpp sends none, and whether ik serves the route at
///   all was never probed.
fn answered_by_lmgw(
    state: &SharedState,
    route: &Route,
    alias: &str,
    status: StatusCode,
    bytes: &[u8],
) -> Option<GatewayError> {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Some(for_protocol(route.upstream.protocol).map_error(status.as_u16(), bytes))
        }
        StatusCode::NOT_FOUND => {
            let snap = state.snapshot();
            let m = snap
                .aux_model_for(route)
                .filter(|m| m.kind == AuxKind::Decision)?;
            let image = crate::runtime::descriptor::model_runtime(&snap, Class::Aux, &m.model_id)
                .map(|r| r.image)
                .filter(|i| !i.is_empty());
            let build = state
                .runtime()
                .llama_entry_now(Class::Aux, &m.model_id)
                .and_then(|e| e.facts().and_then(|f| f.build_info.clone()));
            let server = match image {
                Some(image) => format!("its image '{image}'"),
                None => "its server".to_string(),
            };
            let mut message = format!(
                "model '{alias}': the decision model '{}' runs on {server}, which has no {PATH} \
                 — its llama-server answered 404.",
                m.model_id
            );
            if let Some(build) = build {
                message.push_str(&format!(
                    " It is llama.cpp build {build}; llama.cpp serves the route from b11361 \
                     (2026-10-02) on."
                ));
            }
            message.push_str(
                " Pick an image that serves decision models for the row or the aux class on \
                 the Backends page.",
            );
            Some(GatewayError::Refused {
                status: 501,
                code: "not_supported",
                message,
            })
        }
        _ => None,
    }
}

/// The answer's usage as the row's tokens (§2.6): `input_tokens` as prompt
/// tokens, `output_tokens` as completion tokens, as reported, each on its
/// own, so an answer that reports only its input is still priced on it. An
/// answer without usage leaves both unknown, and the row unpriced rather
/// than 0.
///
/// The wire has no streaming: llama.cpp ignores `stream: true` and TypeSafe
/// has none. An upstream that answers with an event stream anyway is relayed
/// as it came, read whole, and its row is unpriced, since no JSON object
/// carries its usage.
fn usage_of(bytes: &[u8]) -> Usage {
    #[derive(Deserialize)]
    struct Answered {
        usage: Option<Counts>,
    }
    #[derive(Deserialize)]
    struct Counts {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    }
    match serde_json::from_slice::<Answered>(bytes) {
        Ok(Answered { usage: Some(u) }) => Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            ..Default::default()
        },
        _ => Usage::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_count_is_read_on_its_own() {
        let both = usage_of(br#"{"usage": {"input_tokens": 299, "output_tokens": 0}}"#);
        assert_eq!(
            (both.prompt_tokens, both.completion_tokens),
            (Some(299), Some(0))
        );
        let input_only = usage_of(br#"{"answers": {}, "usage": {"input_tokens": 12}}"#);
        assert_eq!(
            (input_only.prompt_tokens, input_only.completion_tokens),
            (Some(12), None)
        );
        for none in [&b"{}"[..], b"data: {}\n\n", b"{\"usage\": null}"] {
            assert_eq!(usage_of(none).prompt_tokens, None);
        }
    }
}
