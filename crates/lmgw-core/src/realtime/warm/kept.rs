//! What an Admit group keeps claimed, and when a stage still waiting for
//! admission lets its wait go (chat-voice design §4.2).
//!
//! A group keeps every claim until all of its stages are up, so a sibling's
//! admission cannot evict a stage that has just loaded. A stage that waits
//! for room only those claims could free would then wait on its own group —
//! and a waiting admission holds the global gate, so every cold start in
//! lmgw would wait with it. So while a stage waits, it asks whether it could
//! still get room beside the kept claims (`VramScheduler::crowded_beside`):
//! once its siblings keep a model on the card, whenever one more settles,
//! and every `vram::POLL` after (room comes and goes with what runs beside
//! lmgw). When it could not, it gives up as `skipped: does_not_fit`, and the
//! group's claims are let go with the group. A stage that waits for a busy
//! model outside the group keeps waiting, as its request would.
//!
//! A wait is only dropped where nothing is left half-way ([`let_go`]): a
//! start of the stage's model in flight is awaited to its end (the
//! registry's start is never dropped mid-way), and while a container is on
//! its way out — an eviction the admission may be making — the drop waits
//! for it.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::watch;

use crate::hf::fmt_bytes;
use crate::runtime::registry::RuntimeState;
use crate::state::SharedState;
use crate::vram::{Crowded, Target};

use super::{GroupSizes, SkipReason, WarmOutcome};

/// The models a group's settled stages keep claimed.
pub(super) struct Kept(watch::Sender<Vec<Target>>);

impl Kept {
    pub(super) fn new() -> Self {
        Self(watch::Sender::new(Vec::new()))
    }

    /// A stage settled holding a claim of `target`.
    pub(super) fn keep(&self, target: Target) {
        self.0.send_if_modified(|kept| {
            let new = !kept.contains(&target);
            if new {
                kept.push(target);
            }
            new
        });
    }

    /// Resolves with the stage's outcome once `target` cannot get room
    /// beside the kept claims (module doc); never for a stage with no local
    /// target, or while the group keeps nothing.
    pub(super) async fn crowded_out(
        &self,
        state: &SharedState,
        target: Option<&Target>,
    ) -> WarmOutcome {
        let Some(target) = target else {
            return std::future::pending().await;
        };
        let mut rx = self.0.subscribe();
        loop {
            let kept = rx.borrow_and_update().clone();
            if !kept.is_empty() && !stopping(state, &kept) {
                let snap = state.snapshot();
                if let Some(c) = state.vram.crowded_beside(state, &snap, target, &kept).await {
                    return outcome(&kept, c);
                }
            }
            // The sender lives as long as the group, which outlives this.
            if kept.is_empty() {
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            } else {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            tokio::time::sleep(crate::vram::POLL).await;
                        }
                    }
                    () = tokio::time::sleep(crate::vram::POLL) => {}
                }
            }
        }
    }
}

/// `skipped: does_not_fit` for a stage crowded out by its own group. The
/// sizes are the group's, as at the group check: the kept models plus this
/// one and the headroom, against what they could have together.
fn outcome(kept: &[Target], c: Crowded) -> WarmOutcome {
    let names: Vec<String> = kept
        .iter()
        .map(|t| format!("{}/{}", t.class.as_str(), t.model_id))
        .collect();
    WarmOutcome::Skipped {
        reason: SkipReason::DoesNotFit,
        message: format!(
            "it does not fit beside {} as they loaded: it needs {} with headroom, and the free \
             memory plus every model it could evict comes to {} — its siblings stay up, so \
             they cannot be evicted for it, and its request will make room",
            names.join(" + "),
            fmt_bytes(c.needs),
            fmt_bytes(c.room)
        ),
        sizes: Some(GroupSizes {
            needed_bytes: c.kept.saturating_add(c.needs),
            capacity_bytes: c.kept.saturating_add(c.room),
        }),
    }
}

/// Let a waiting admission go once nothing would be left half-way (module
/// doc): `None` once it is dropped; `Some` with its result when a start of
/// `target` was in flight, or it finished while a stop was waited out.
pub(super) async fn let_go<F: Future>(
    state: &SharedState,
    target: Option<&Target>,
    mut admit: Pin<&mut F>,
) -> Option<F::Output> {
    loop {
        if target.is_some_and(|t| starting(state, t)) {
            return Some(admit.await);
        }
        if !stopping(state, &[]) {
            return None;
        }
        tokio::select! {
            biased;
            a = &mut admit => return Some(a),
            () = tokio::time::sleep(crate::vram::POLL) => {}
        }
    }
}

/// A start of `target` is in flight: an admission parked on it is awaited
/// to its end. Not when it is up (admission takes the hit path at once, so
/// a stage still waiting is queued, and may go) or on its way out.
fn starting(state: &SharedState, target: &Target) -> bool {
    state.runtime().list().iter().any(|v| {
        v.class == target.class
            && v.model_id == target.model_id
            && v.state == RuntimeState::Starting
    })
}

/// A container the admission may be waiting on is on its way out: room is
/// changing, and a stop dropped mid-way would leave its entry `stopping`.
/// Only one on the card and not `kept` counts — the target, or a resident
/// an eviction would make room with (WP11 server review n3): a CPU row the
/// idle reaper stops elsewhere changes no room, and would switch the
/// crowded-out check off and keep `let_go` polling for nothing.
fn stopping(state: &SharedState, kept: &[Target]) -> bool {
    let snap = state.snapshot();
    state.runtime().list().iter().any(|v| {
        v.state == RuntimeState::Stopping
            && snap.placement(v.class, &v.model_id).is_gpu()
            && !kept
                .iter()
                .any(|k| k.class == v.class && k.model_id == v.model_id)
    })
}
