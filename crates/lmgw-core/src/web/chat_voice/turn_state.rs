//! A chat turn's `state` frames (chat-voice design §4.3).
//!
//! Before an admission whose model is not up and ready (absent, starting
//! for another caller, or on its way out — any of them is a wait), the
//! turn says `loading` for its `chat` stage; once admission settles it
//! says `ready` with the time it took — or `fallback` when the outside-VRAM
//! verdict answered with the fallback, `held` or `failed` when admission
//! refused (the turn's own `error` frame follows, as before). A model that
//! is up, a route the GPU hold already swapped to its fallback, a cloud
//! route and a candidate alias (its pick is made at admission) say nothing.
//!
//! The GPU hold and a benchmark's lease refuse a local model with no usable
//! fallback earlier, at the route's resolve, before any admission: that
//! refusal says `held` too ([`held_at_resolve`], WP11 server review M1), so
//! a hold is the amber chip §4.3 promises on every path, not an error.
//!
//! The plain text `send` carries these frames too: a wire change the page
//! before it ignores (it skips unknown events).

use std::time::Instant;

use crate::error::GatewayError;
use crate::gate::{OpenFailed, Opened, Routed};
use crate::realtime::warm::{ModelState, WarmOutcome};
use crate::state::SharedState;

use super::super::chat_turn::{Events, TurnFrame};

/// Admit `routed` — the turn's model, asked for as `alias` — saying its
/// `state` around an admission that has to start it (module doc).
pub(crate) async fn admit_reporting(
    state: &SharedState,
    routed: Routed,
    alias: &str,
    tx: &Events,
) -> Result<Opened, OpenFailed> {
    let cold = state.snapshot().candidate_alias(alias).is_none()
        && routed.headers().fallback().is_none()
        && crate::vram::classify(routed.resolved())
            .is_some_and(|t| state.runtime().ready_port(t.class, &t.model_id).is_none());
    if !cold {
        return routed.admit(state).await;
    }
    frame(tx, ModelState::loading("chat", alias)).await;
    let started = Instant::now();
    let admitted = routed.admit(state).await;
    let outcome = match &admitted {
        Ok(o) => match o.headers.fallback() {
            Some(fb) => WarmOutcome::Fallback {
                answered_by: fb.to_string(),
            },
            None => WarmOutcome::Ready {
                ms: Some(started.elapsed().as_millis() as u64),
            },
        },
        Err(f) => WarmOutcome::refused(&f.error),
    };
    if let Some(s) = ModelState::of("chat", alias, &outcome) {
        frame(tx, s).await;
    }
    admitted
}

/// The `held` frame of a refusal at the route's resolve: said for the GPU
/// hold and a benchmark's lease (module doc), nothing for any other
/// refusal. The turn's `error` frame follows.
pub(crate) async fn held_at_resolve(tx: &Events, alias: &str, e: &GatewayError) {
    if !matches!(
        e,
        GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }
    ) {
        return;
    }
    if let Some(s) = ModelState::of("chat", alias, &WarmOutcome::refused(e)) {
        frame(tx, s).await;
    }
}

async fn frame(tx: &Events, s: ModelState) {
    if let Ok(data) = serde_json::to_string(&s) {
        let _ = tx.send(TurnFrame::new("state", data)).await;
    }
}
