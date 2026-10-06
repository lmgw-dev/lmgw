//! GPU hold

use serde_json::{json, Value};

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
pub async fn hold_set(state: &SharedState, active: bool) -> Result<Value, String> {
    let snap = state.snapshot();
    if snap.settings.hold.active == active {
        return Ok(json!({
            "ok": true,
            "active": active,
            "fallback_alias": snap.settings.hold.fallback_alias,
            "stopped": Vec::<String>::new(),
            "draining": draining_now(state, active),
            "failed": Vec::<String>::new(),
            "kept_on_cpu": on_cpu_now(state, active),
            "message": format!(
                "the GPU hold was already {} — nothing to do",
                if active { "on" } else { "off" }
            ),
        }));
    }

    let (sweep, bench_aborted) = {
        let _guard = state.settings_write.lock().await;
        // Re-read under the lock: the snapshot above was taken outside it.
        let mut s = state.snapshot().settings.clone();
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
        // 3. Only then reconcile MCP. It is a `join_all` over autostart
        //    servers, and one unreachable server stalls it for its whole
        //    connect timeout; between 1 and 2 it would delay handing the card
        //    back by exactly that long, for a reason with nothing to do with
        //    the GPU. Same hazard the design's §5 restructured `boot` for.
        let snap = state.publish_snapshot().await.map_err(|e| e.to_string())?;
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
        state.mcp.reconcile(&snap).await;
        (swept, bench_aborted)
    };

    // Event-driven frame: the containers this just stopped (or the badge a
    // release just cleared) are not visible on any surface until something
    // pushes, and no request ran to do it.
    crate::vram::broadcast(state);

    let snap = state.snapshot();
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
                "; audio models that run on the CPU keep serving ({})",
                sweep.kept_on_cpu.join(", ")
            ));
        }
        if let Some(run) = bench_aborted {
            m.push_str(&format!(
                "; benchmark run {run} was aborted and its container removed"
            ));
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
    Ok(json!({
        "ok": true,
        "active": active,
        "fallback_alias": snap.settings.hold.fallback_alias,
        "stopped": sweep.stopped,
        "draining": sweep.draining,
        "failed": sweep.failed,
        "kept_on_cpu": sweep.kept_on_cpu,
        "benchmark_aborted": bench_aborted,
        "message": message,
    }))
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
