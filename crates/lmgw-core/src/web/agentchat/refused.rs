//! A tool turn refused before its loop's first model call — at admission,
//! or on the route admission settled on (`fit_route`: a continue with no
//! prefill, a heard turn's audio on a route lmgw does not run) — writes its
//! request row as the plain path does (`chat::relay`, voice-audio-input
//! WP2 review #8): the refusal is model traffic that did not happen, and
//! the Requests page says why.

use std::time::Instant;

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ir::Usage;
use crate::proxy;
use crate::state::SharedState;

/// The refusal `e` of the turn for `alias` on `route`, under the loop's
/// `proto`, as its request row.
pub(super) async fn record(
    state: &SharedState,
    alias: &str,
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
            key: proxy::KeyRef::default(),
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
        },
        e.http_status().as_u16(),
        None,
        Usage::default(),
        Some((e.kind(), e.to_string())),
        state,
    )
    .await;
}
