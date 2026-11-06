//! GPU hold

use lmgw_api_types as dto;

use crate::runtime::lifecycle;
use crate::state::SharedState;
use crate::store::{self};

/// Engage or release the GPU hold (gpu-hold design §6) — the one way
/// `settings.hold.active` is written.
///
/// It is not a settings patch, and that is the whole reason this function
/// exists: engaging has a side effect no generic "save the settings blob" may
/// grow, the sweep that hands the card back (§5). A dashboard save that also
/// happened to carry `hold.active` would either skip the sweep or start doing
/// container work behind an unrelated form submit.
///
/// The whole read-modify-write runs under [`crate::state::AppState::settings_write`]:
/// without it a settings save racing this one reverts `hold.active` from its
/// own older snapshot *after* the sweep already stopped everything, leaving a
/// gateway that says it is holding nothing while every model is down (§3.1).
///
/// Unchanged is a no-op, not an error: the tray, the titlebar pill and MCP can
/// all toggle this, and two of them arriving at the same answer within a tick
/// of each other is normal. The response still reports the current state so a
/// caller never has to guess.
///
/// From the change on it runs as a task of its own (*changed 2026-10-24*, see
/// [`crate::detached::detached`]): a dropped request can no longer cut the
/// sweep, the frame or the MCP reconcile after the write, which would leave
/// the hold on and the card still in use.
pub async fn hold_set(
    state: &SharedState,
    active: bool,
) -> Result<dto::HoldSet, crate::principal::Refusal> {
    // The fast path for a no-op, only while no settings write is in flight:
    // the snapshot then is what the last write published. With one in
    // flight — an "on" that has not published yet — the snapshot can still
    // say the old state, and "already off" would be the wrong answer to an
    // "off" sent after it, leaving the hold on. That case waits for the lock
    // in `change`, which asks again under it.
    if let Ok(_free) = state.settings_write.try_lock() {
        if state.snapshot().settings.hold.active == active {
            return Ok(unchanged(state, active));
        }
    }

    let task_st = state.clone();
    crate::detached::detached("switching the GPU hold", async move {
        change(&task_st, active).await.map_err(Into::into)
    })
    .await?
}

/// [`hold_set`]'s change, once it is one.
async fn change(state: &SharedState, active: bool) -> Result<dto::HoldSet, String> {
    let (sweep, bench_aborted) = {
        let _guard = state.settings_write.lock().await;
        // Re-read under the lock: the snapshot above was taken outside it,
        // and a change that held the lock meanwhile may have made this one
        // a no-op.
        let mut s = state.snapshot().settings.clone();
        if s.hold.active == active {
            return Ok(unchanged(state, active));
        }
        s.hold.active = active;
        store::save_settings(&state.db, &s)
            .await
            .map_err(|e| e.to_string())?;
        // Three steps in this order, and the order is the whole point.
        //
        // 1. Publish the snapshot. That is what arms the gate — until every
        //    reader sees `hold.active`, `resolve_for_request` still hands out
        //    local routes and a request arriving mid-sweep would start a
        //    container the sweep has already walked past.
        // 2. Sweep, while still holding the settings lock, so the setting the
        //    sweep acted on cannot have been reverted underneath it.
        // 3. Only then reconcile MCP, once the lock is let go. It is a
        //    `join_all` over autostart servers, and one server's connect can
        //    take as long as its image pull; between 1 and 2 it would delay
        //    handing the card back by exactly that long, for a reason with
        //    nothing to do with the GPU. Same hazard the design's §5
        //    restructured `boot` for. Under the lock it held up the next
        //    settings writer as long (`AppState::settings_saved`). It
        //    reconciles the snapshot of then, which a publish by another
        //    path during the sweep may have replaced (`reconcile_mcp`).
        state.publish_snapshot().await.map_err(|e| e.to_string())?;
        // A benchmark run in flight ends here (benchmark design §3.3): hold
        // means lmgw uses no VRAM, and the run's container is not a registry
        // entry the sweep below would see — so it is removed by name now.
        let bench_aborted = if active {
            crate::bench::abort_for_hold(state).await
        } else {
            None
        };
        let swept = if active {
            lifecycle::hold_sweep(state).await
        } else {
            lifecycle::HoldSweep::default()
        };
        (swept, bench_aborted)
    };

    // Event-driven frame: the containers this just stopped (or the badge a
    // release just cleared) are not visible on any surface until something
    // pushes, and no request ran to do it. Before the reconcile, which can
    // wait on a server's connect.
    crate::vram::broadcast(state);
    state.reconcile_mcp().await;

    let message = if active {
        // The failures are in the sentence, not only in the list, because the
        // caller may be a tray click whose only feedback is this one line —
        // and a container that would not stop is still on the card the owner
        // just asked for back. Nothing retries it (see `HoldSweep::failed`),
        // so silence here would be a lie by omission.
        let mut m = format!(
            "GPU hold engaged — {} container(s) stopped, {} still draining; local models are \
             refused or served by their fallback until it is released",
            sweep.stopped.len(),
            sweep.draining.len()
        );
        if !sweep.kept_on_cpu.is_empty() {
            m.push_str(&format!(
                "; models that run on the CPU keep serving ({})",
                sweep.kept_on_cpu.join(", ")
            ));
        }
        if let Some(run) = &bench_aborted {
            match &run.removal_error {
                None => m.push_str(&format!(
                    "; benchmark run {} was aborted and its container removed",
                    run.run_id
                )),
                Some(e) => m.push_str(&format!(
                    "; benchmark run {} was aborted but its container could NOT be removed and \
                     may still be holding GPU memory: {e}",
                    run.run_id
                )),
            }
        }
        if !sweep.failed.is_empty() {
            m.push_str(&format!(
                ". {} container(s) could NOT be stopped and are still holding GPU memory — \
                 lmgw retries the ones it started on later reaper ticks, backing off while the \
                 stop keeps failing; `podman stop` them if they stay: {}",
                sweep.failed.len(),
                sweep.failed.join("; ")
            ));
        }
        m
    } else {
        "GPU hold released — local models start on demand again".to_string()
    };
    Ok(dto::HoldSet {
        ok: true,
        active,
        stopped: sweep.stopped,
        draining: sweep.draining,
        failed: sweep.failed,
        kept_on_cpu: sweep.kept_on_cpu,
        // Absent unless this call aborted a run (never, on a release).
        benchmark_aborted: bench_aborted.map(|a| a.run_id),
        message,
    })
}

/// [`hold_set`]'s answer when the hold already is `active`: nothing changed,
/// so no run was aborted.
fn unchanged(state: &SharedState, active: bool) -> dto::HoldSet {
    dto::HoldSet {
        ok: true,
        active,
        stopped: Vec::new(),
        draining: draining_now(state, active),
        failed: Vec::new(),
        kept_on_cpu: on_cpu_now(state, active),
        benchmark_aborted: None,
        message: format!(
            "the GPU hold was already {} — nothing to do",
            if active { "on" } else { "off" }
        ),
    }
}

/// What the registry currently believes is still working, for the no-op arm of
/// [`hold_set`]. The registry's own view, no `/slots` probe: this arm changed
/// nothing, so it reports rather than acts.
fn draining_now(state: &SharedState, active: bool) -> Vec<String> {
    if !active {
        return Vec::new();
    }
    state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| v.placement.is_gpu())
        .filter(|v| v.state == crate::runtime::registry::RuntimeState::Starting || v.in_flight > 0)
        .map(|v| format!("{}/{}", v.class.as_str(), v.model_id))
        .collect()
}

/// The containers on the CPU the hold leaves running, for the no-op arm of
/// [`hold_set`] (the sweep's own [`lifecycle::HoldSweep::kept_on_cpu`]
/// otherwise).
fn on_cpu_now(state: &SharedState, active: bool) -> Vec<String> {
    if !active {
        return Vec::new();
    }
    state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| !v.placement.is_gpu())
        .map(|v| format!("{}/{}", v.class.as_str(), v.model_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An "off" sent while an "on" is in flight (here: the settings lock
    /// held, the snapshot still off) is not answered "already off": it waits
    /// for the "on", then releases the hold.
    #[tokio::test]
    async fn an_off_behind_an_on_in_flight_releases_the_hold() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        assert!(!state.snapshot().settings.hold.active);
        let held = state.settings_write.lock().await;

        let st = state.clone();
        let on = tokio::spawn(async move { hold_set(&st, true).await });
        // The "on" queues on the lock first.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let st = state.clone();
        let off = tokio::spawn(async move { hold_set(&st, false).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !off.is_finished(),
            "the off did not answer from the snapshot of before the on"
        );

        drop(held);
        let on = on.await.unwrap().unwrap();
        assert!(
            on.active && !on.message.contains("already"),
            "{}",
            on.message
        );
        let off = off.await.unwrap().unwrap();
        assert!(
            !off.active && !off.message.contains("already"),
            "{}",
            off.message
        );
        assert!(!state.snapshot().settings.hold.active, "the hold is off");
    }

    #[tokio::test]
    async fn an_unchanged_hold_is_a_no_op() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let r = hold_set(&state, false).await.unwrap();
        assert_eq!(r.message, "the GPU hold was already off — nothing to do");
    }
}
