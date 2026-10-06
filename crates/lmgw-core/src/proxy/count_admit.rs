//! A counter's way through the gate, and its row (api-docs design §5.4):
//! the three counters — `/v1/count_tokens`, `/v1/messages/count_tokens`,
//! `/tokenize` — open an [`Unanswered`] and admit through here, so the row
//! learns the route the gate settled on and whether this count had to bring
//! its model up. A cheap count leaves no row; a failed one, a cold one and
//! one whose client went away do.

use std::time::Instant;

use serde_json::Value;

use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::unanswered::would_start;
use super::*;

/// Open a counter's row ([`Unanswered`]): a count is metadata, and a cheap one
/// writes none, but one that failed or brought its model up does — and one
/// whose client went away while it waited says so (`499`).
pub(super) fn counter_row(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    body: &Value,
) -> Unanswered {
    let alias = body.get("model").and_then(Value::as_str).unwrap_or("?");
    // `Tool`, the class of a row that is no model call: a count's zero tokens
    // must not drag the chat class's tokens per request down, nor count as an
    // agent run's model call (`record`).
    Unanswered::open(state, proto, ctx, alias, Instant::now(), RequestClass::Tool)
}

/// The gate's per-request half for a counter ([`crate::gate::open`], or
/// [`crate::gate::open_pinned`]), telling `row` what it learns: the route it
/// settled on, and whether the count's admission had to bring the model up
/// ([`would_start`], asked before admission — a swap to a fallback is no
/// start).
pub(super) async fn admit_counter(
    state: &SharedState,
    alias: &str,
    check: crate::gate::RouteCheck,
    pinned: bool,
    row: Option<&mut Unanswered>,
) -> Result<crate::gate::Opened, (GateHeaders, GatewayError)> {
    let routed = crate::gate::resolve(state, alias, check)
        .await
        .map_err(|f| (f.headers, f.error))?;
    admit_routed(state, routed, pinned, row).await
}

/// [`admit_counter`] past the resolve, for a counter that judges the resolved
/// route itself first (`/v1/messages/count_tokens`'s facets).
pub(super) async fn admit_routed(
    state: &SharedState,
    routed: crate::gate::Routed,
    pinned: bool,
    mut row: Option<&mut Unanswered>,
) -> Result<crate::gate::Opened, (GateHeaders, GatewayError)> {
    let cold = would_start(state, routed.resolved());
    if let Some(r) = row.as_deref_mut() {
        r.routed_to(routed.resolved(), routed.headers().fallback_reason());
        r.stage(if cold {
            "waiting for the model's container to start, to count with it"
        } else {
            "waiting for admission"
        });
    }
    let opened = if pinned {
        routed.admit_pinned(state).await
    } else {
        routed.admit(state).await
    };
    let opened = match opened {
        Ok(o) => o,
        Err(f) => {
            if let (Some(r), Some(route)) = (row.as_deref_mut(), f.route.as_deref()) {
                r.routed_to(route, f.headers.fallback_reason());
            }
            return Err((f.headers, f.error));
        }
    };
    if let Some(r) = row {
        r.routed_to(&opened.route, opened.headers.fallback_reason());
        if cold && opened.hold.is_some() {
            r.cold_start();
        }
        r.stage("counting on the upstream");
    }
    Ok(opened)
}
