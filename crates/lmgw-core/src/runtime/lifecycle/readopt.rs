//! Reconciliation after boot (§3.4): when it runs, and what it says.
//!
//! [`Registry::readopt`](crate::runtime::registry::Registry::readopt) is the
//! comparison; this is its policy. Three moments: every reaper tick, so a
//! container that lost its entry is lmgw's again within a
//! [`super::REAP_INTERVAL`] or two; an admission that found nothing of lmgw's
//! to make room with, before it settles into waiting — the one place where
//! "this memory is somebody else's" turns into a refusal after
//! `vram.queue_timeout_seconds` (`vram::stall`); and the container verbs that
//! act on what is running.
//!
//! **Nothing waits for a pass it does not need.** The tick spawns its pass
//! ([`readopt_in_background`]) and skips while one is still running, so the
//! hold sweep and the reaping never wait on podman for it. A caller that
//! wants the answer ([`readopt`]) bounds its wait: an admission by what is
//! left of its queue budget, a verb by one tick. A pass cut short loses
//! nothing — see `registry/unheld.rs`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::config::Snapshot;
use crate::runtime::descriptor::{higher_rungs, model_runtimes, ModelRuntime};
use crate::runtime::registry::{AcquireSpec, PassLimits, PassWait};
use crate::state::{AppState, SharedState};

use super::{acquire_spec, REAP_INTERVAL};

/// One pass — or, with [`PassWait::Join`], the one already running — within
/// `within`. `true` when it changed what the registry holds: the caller's cue
/// to measure again. `false` too when it did not finish in time; the tick's
/// pass carries on where it left off.
///
/// Not before boot's own reconciliation is over: until then the containers a
/// previous lmgw left are boot's to adopt, and two passes at once would race
/// each other for them.
pub async fn readopt(state: &SharedState, wait: PassWait, within: Duration) -> bool {
    if !state.vram.boot_settled() || within.is_zero() {
        return false;
    }
    // Boxed: an admission's future would otherwise carry the whole pass's
    // type inside it, deeper than the compiler's layout queries go.
    let pass: Pin<Box<dyn Future<Output = bool> + Send + '_>> = Box::pin(pass(state, wait));
    match tokio::time::timeout(within, pass).await {
        Ok(changed) => changed,
        Err(_) => {
            tracing::debug!("reconciliation pass: not done within {within:?}; going on without it");
            false
        }
    }
}

/// The reaper tick's pass, as a task of its own: skipped while another pass
/// is still running, and never waited for.
pub fn readopt_in_background(state: &SharedState) {
    if !state.vram.boot_settled() {
        return;
    }
    let st = state.clone();
    tokio::spawn(async move {
        pass(&st, PassWait::Skip).await;
    });
}

async fn pass(state: &SharedState, wait: PassWait) -> bool {
    let snap = state.snapshot();
    let runtimes = model_runtimes(&snap);
    let rungs = higher_rungs(&snap);
    let candidates = candidates(state, &snap, &runtimes, &rungs);
    let s = &snap.settings;
    let limits = PassLimits {
        load_timeout: Duration::from_secs(s.vram.load_timeout_seconds),
        stop_timeout: Duration::from_secs(s.vram.unload_timeout_seconds),
        tick: REAP_INTERVAL,
    };
    let Some(report) = state
        .runtime()
        .readopt(&s.container_prefix, &candidates, limits, wait)
        .await
    else {
        return false;
    };
    // A pass this call only waited for has said all of this itself.
    if report.joined.is_some() {
        return report.changed();
    }
    for name in &report.adopted {
        tracing::warn!(
            container = %name,
            "adopted a running container lmgw held no entry for — it runs what its model renders \
             now, so it is ready and idle; left by a start that lost its entry, or started \
             outside lmgw"
        );
    }
    for (name, why) in &report.removed {
        tracing::warn!(
            container = %name,
            "removed a running container lmgw held no entry for: {why}"
        );
    }
    for e in &report.errors {
        // A podman that cannot be asked at all is boot's to report, once: on
        // a box without podman this would say so every tick. A failed removal
        // is the pass's own to say, once (`registry/unheld/memory.rs`).
        tracing::debug!("reconciliation pass: {e}");
    }
    if !report.changed() {
        return false;
    }
    // What is on the card changed without a request having run: learn the
    // adopted containers' PIDs (§4.7), and tell the dashboard.
    state.vram.cache_pids(state);
    crate::vram::broadcast(state);
    true
}

/// Boot's adoption candidates: one spec per enabled model and per higher
/// ladder rung, base first (ladder design §3.1), so a container a previous
/// lmgw had climbed is adopted at the rung it runs.
pub(super) fn candidates<'a>(
    state: &'a AppState,
    snap: &'a Snapshot,
    runtimes: &'a [ModelRuntime],
    rungs: &'a [ModelRuntime],
) -> Vec<AcquireSpec<'a>> {
    runtimes
        .iter()
        .chain(rungs)
        .filter(|r| r.enabled)
        .map(|r| acquire_spec(state, snap, r))
        .collect()
}
