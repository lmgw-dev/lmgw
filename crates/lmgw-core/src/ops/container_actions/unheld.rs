//! `container stop` for a model the registry holds no entry for: a container
//! the registry lost track of still runs under the name the model renders
//! (`runtime/registry/unheld.rs`), and only podman can say whether it does —
//! and whether it is this lmgw's to stop.

use super::{done, ContainerAnswer};
use crate::runtime::registry::Unheld;
use crate::runtime::Class;
use crate::state::SharedState;

/// `model_stop` for a model the registry holds no entry for.
pub(super) async fn model_stop_unheld(
    state: &SharedState,
    class: Class,
    model_id: &str,
) -> Result<ContainerAnswer, String> {
    let s = state.snapshot().settings.clone();
    let stop_timeout = std::time::Duration::from_secs(s.vram.unload_timeout_seconds);
    match state
        .runtime()
        .stop_unheld(&s.container_prefix, class, model_id, stop_timeout)
        .await
    {
        Ok(Unheld::Stopped(name)) => {
            // Off the card without a request having run: the dashboard is
            // told, as after a hold sweep.
            crate::vram::broadcast(state);
            Ok(ContainerAnswer::Done(done(
                class,
                model_id,
                true,
                format!(
                    "'{model_id}' ({class}) had no lmgw entry, but its container '{name}' was \
                     running — stopped it by name"
                ),
            )))
        }
        Ok(Unheld::NotRunning) => Ok(ContainerAnswer::Done(done(
            class,
            model_id,
            true,
            format!("'{model_id}' ({class}) is not running — nothing to stop"),
        ))),
        // Another instance sharing this prefix, or an older lmgw: not ours to
        // stop, and the owner is told how to, if it is theirs after all.
        Ok(Unheld::NotOurs { name, whose }) => Ok(ContainerAnswer::Done(done(
            class,
            model_id,
            false,
            format!(
                "'{model_id}' ({class}) has no lmgw entry; its container '{name}' is running but \
                 {whose} — not stopping it. `podman stop {name}` if it is this lmgw's after all"
            ),
        ))),
        Err(e) => Err(e.to_string()),
    }
}
