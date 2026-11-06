//! An image call's one `request_logs` row (chat-media-tools design §4.1
//! step 6): class `image`, zero tokens, the pictures as `images_out`,
//! latency the call's wall time, charged to the caller's context.

use std::time::Instant;

use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::pricing::Quantities;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::super::{record, row_status, LogParams, RequestCtx};
use super::Done;

pub(super) async fn write(
    state: &SharedState,
    (ctx, proto, alias): (&RequestCtx, ClientProto, &str),
    started: Instant,
    (route, headers): (Option<&Route>, &GateHeaders),
    result: &Result<Done, GatewayError>,
) {
    let counted = result.as_ref().map(|done| done.images.len() as u64);
    write_counted(
        state,
        (ctx, proto, alias),
        started,
        (route, headers),
        counted,
    )
    .await;
}

/// [`write`] for a call that counts its pictures itself: an upscale's one.
pub(super) async fn write_counted(
    state: &SharedState,
    (ctx, proto, alias): (&RequestCtx, ClientProto, &str),
    started: Instant,
    (route, headers): (Option<&Route>, &GateHeaders),
    result: Result<u64, &GatewayError>,
) {
    let quantities = match result {
        Ok(images) => Quantities {
            images_out: Some(images),
            ..Quantities::answered()
        },
        Err(_) => Quantities::default(),
    };
    let params = LogParams {
        state,
        proto,
        ctx,
        alias: alias.to_string(),
        route,
        started,
        streamed: false,
        class: RequestClass::Image,
        timings: None,
        max_tokens_clamped: None,
        fallback: headers.fallback_reason(),
        rung: None,
        degraded: None,
        quantities,
        service_tier: Default::default(),
    };
    match result {
        Ok(_) => {
            let ttfb = Some(started.elapsed().as_millis() as i64);
            record(params, 200, ttfb, Usage::default(), None).await;
        }
        Err(e) => {
            record(
                params,
                row_status(e),
                None,
                Usage::default(),
                Some((e.kind(), e.log_text())),
            )
            .await;
        }
    }
}
