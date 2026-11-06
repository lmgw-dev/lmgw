//! A model call of a loop refused before it went out — at the loop's
//! admission, on the route admission settled on, or at a claim's regain
//! ([`super::LoopClaim::call_on`]) — writes its request row as a plain
//! request's refusal does: it is model traffic that did not happen, and the
//! Requests page says why.

use std::time::Instant;

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ir::Usage;
use crate::proxy;
use crate::state::SharedState;

/// The refusal `e` of the call for `alias` on `route`, under the loop's
/// `proto`, as its request row — charged to `key`, the loop's caller.
pub(crate) async fn record(
    state: &SharedState,
    (key, alias): (proxy::KeyRef, &str),
    proto: &str,
    route: &Route,
    fallback: Option<FallbackReason>,
    started: Instant,
    e: &GatewayError,
) {
    // The row closes the request the gauge counts, as every row does.
    state.telemetry.request_started();
    proxy::record_in_process(
        proxy::InProcessLog {
            key,
            ingress_proto: proto,
            alias,
            route,
            started,
            streamed: true,
            class: crate::telemetry::RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback,
            rung: None,
            degraded: None,
            quantities: Default::default(),
            service_tier: Default::default(),
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.log_text())),
        state,
    )
    .await;
}
