//! A dev instance's step before it serves (chat-archive-pin-attachments
//! review finding 12): a fresh `LMGW_DATA_DIR` loads settings whose
//! `container_prefix` is still the production default — the label the real
//! tray app's model containers carry — and boot reconciliation, which
//! `server::run` spawns, force-removes whatever it does not recognize under
//! that prefix. So an entry point that runs a dev instance without a window
//! (the headless runner, `lmgw-server` under `LMGW_DEV`) moves a fresh data
//! dir off it here, between `AppState::init_with` and `server::run`;
//! `server::bind` refuses a dev instance still on it as the backstop.

use crate::config::dev_instance_override;
use crate::state::SharedState;
use crate::store;

/// If this data dir's settings still carry the production default
/// `container_prefix`, move it to `lmgw-dev` (or `env_prefix`, the
/// `LMGW_CONTAINER_PREFIX` the entry point read) and pin `bind_addr` to
/// `cli_addr` when the run was given one, persist both, and log one line
/// saying so. A data dir that was already customized — an owner-configured
/// one, or one an earlier run of this same check already fixed — is left
/// alone entirely (see [`dev_instance_override`], which makes that call).
pub async fn ensure_dev_instance_safety(
    state: &SharedState,
    env_prefix: Option<&str>,
    cli_addr: Option<&str>,
) -> anyhow::Result<()> {
    let settings = state.snapshot().settings.clone();
    let Some((new_prefix, new_bind_addr)) =
        dev_instance_override(&settings.container_prefix, env_prefix, cli_addr)
    else {
        return Ok(());
    };

    let mut settings = settings;
    settings.container_prefix = new_prefix;
    if let Some(addr) = new_bind_addr {
        settings.bind_addr = addr;
    }
    tracing::info!(
        "dev instance safety: container_prefix -> '{}', bind_addr -> '{}' — a fresh data dir \
         must never boot under the production container_prefix, or boot reconciliation could \
         force-remove the real app's model containers (see scripts/dev-instance.sh)",
        settings.container_prefix,
        settings.bind_addr,
    );
    store::save_settings(&state.db, &settings).await?;
    state.reload_snapshot().await?;
    Ok(())
}
