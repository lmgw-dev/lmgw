//! Emptying the card before a run (benchmark design §3.2 step 3): every
//! lmgw container of every class is stopped — an idle one at once, a busy
//! one once its requests have drained. Nothing is killed mid-request, as
//! with the hold's draining (decision 3), and no new request arrives
//! meanwhile: the run holds the GPU lease already.

use std::time::Duration;

use super::run::{BenchSink, Progress};
use crate::agent::Cancel;
use crate::runtime::registry::{RuntimeError, RuntimeState};
use crate::state::SharedState;

/// How often a drain looks again at what is still busy — a sampling rate,
/// not a bound: the drain waits as long as the requests take, and the
/// owner's cancel is what ends it early.
const DRAIN_POLL: Duration = Duration::from_millis(500);

/// How emptying the card ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drained {
    Empty,
    Canceled,
    /// A container that would not stop: it is still on the card, and every
    /// number the run took next to it would be wrong.
    Failed(String),
}

/// Stop every registry container, waiting for the busy ones, reporting the
/// wait as the stage "waiting for X to finish its request". Returns once the
/// registry is empty.
///
/// Busy is the hold sweep's reading: starting, serving an lmgw request, or —
/// asked of its `/slots` — generating for a client on the container's own
/// port. An unanswerable `/slots` counts as idle, as it does there.
pub async fn empty_the_card(state: &SharedState, cancel: &Cancel, sink: &dyn BenchSink) -> Drained {
    let registry = state.runtime();
    // The lease is taken; a start decided before it may still be between its
    // check and its claim. Every decision that holds the admission gate
    // finishes first (each asks the lease again before it claims), and the
    // registry refuses to create an entry from here on — so what the loop
    // below finds is everything that will ever be on the card until the run
    // ends (§13 decision 44).
    if cancel.guard(state.vram.settle_decisions()).await.is_none() {
        return Drained::Canceled;
    }
    let mut reported = String::new();
    loop {
        let live = registry.list();
        // A start admission decided just before the lease was taken is not
        // in the registry yet; its container appears once it is up.
        let mut waiting: Vec<String> = state.vram.pending_starts();
        if live.is_empty() && waiting.is_empty() {
            return Drained::Empty;
        }
        for v in live {
            let name = format!("{}/{}", v.class.as_str(), v.model_id);
            if v.state != RuntimeState::Ready || v.in_flight > 0 {
                waiting.push(name);
                continue;
            }
            if v.port != 0
                && crate::vram::busy_slots(&state.http, v.port, crate::vram::CONTROL_TIMEOUT)
                    .await
                    .is_some_and(|n| n > 0)
            {
                waiting.push(name);
                continue;
            }
            stage(sink, &mut reported, format!("stopping {name}")).await;
            match registry
                .stop_generation(v.class, &v.model_id, v.generation, false)
                .await
            {
                Ok(()) => tracing::info!("benchmark: stopped {name} to empty the GPU"),
                // A request arrived between the look and the stop (admitted
                // just before the lease), or the container was replaced: it
                // is looked at again on the next pass.
                Err(RuntimeError::Busy { .. } | RuntimeError::Moved { .. }) => waiting.push(name),
                Err(e) => return Drained::Failed(format!("stopping {name} failed: {e}")),
            }
        }
        if !waiting.is_empty() {
            waiting.sort();
            waiting.dedup();
            let line = match waiting.as_slice() {
                [one] => format!("waiting for {one} to finish its request"),
                many => format!("waiting for {} to finish their requests", many.join(", ")),
            };
            stage(sink, &mut reported, line).await;
        }
        if cancel.guard(tokio::time::sleep(DRAIN_POLL)).await.is_none() {
            return Drained::Canceled;
        }
    }
}

/// Report `line` as the stage, once per change.
async fn stage(sink: &dyn BenchSink, last: &mut String, line: String) {
    if *last == line {
        return;
    }
    *last = line.clone();
    sink.progress(Progress {
        stage: line,
        done: 0,
        total: 0,
    })
    .await;
}
