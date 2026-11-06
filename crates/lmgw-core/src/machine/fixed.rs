//! After the root helper ran (machine-check design §3.5, WP7): what the
//! app's shell does once the setup's steps are over, whatever their end.
//!
//! lmgw's own spec is written with the user's rights (the toolkit the steps
//! installed is what writes it, §3.6), then the machine is checked fresh,
//! and the check's result is what the person reads: a step that exited 0
//! without its effect is caught here, because the check reads the machine,
//! not the steps' exit codes. All of it under the one-run lock, published
//! once at the end, as the boot run does.
//!
//! **The ledger's probe.** VRAM admission reads the card through a probe
//! picked once at boot ([`crate::vram::detect_probe`]). The helper's steps
//! never change what it reads (they install NVIDIA's container toolkit,
//! never the driver whose `libnvidia-ml.so.1` the probe loads), so after a
//! fix the probe only needs replacing when the driver came up after lmgw
//! started (an akmod build that finished after the boot): the check then
//! says the card and its driver work, and the ledger's probe still reads
//! nothing. Only then is the probe detected again and, when it reads the
//! card, installed, so admission works without a restart.

use std::sync::Arc;

use lmgw_api_types::machine::MachineCheck;

use crate::state::SharedState;
use crate::vram::GpuProbe;

use super::{card, publish, run};

/// How the ledger's probe is detected again: [`crate::vram::detect_probe`]
/// in production; a test states one.
pub type DetectProbe = fn() -> Arc<dyn GpuProbe>;

/// What [`after_fix`] did besides the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixed {
    /// How the write of lmgw's own spec came out, in words for the log and
    /// the detail.
    pub spec: String,
    /// Whether the ledger's probe was replaced (it read nothing before and
    /// reads the card now).
    pub probe_installed: bool,
}

/// After the setup's steps (§3.5): lmgw's own spec written, a fresh check
/// published, and the ledger's probe installed again where the check says
/// the NVIDIA card works and the probe reads nothing.
pub async fn after_fix(state: &SharedState, detect: DetectProbe) -> (Arc<MachineCheck>, Fixed) {
    let m = &state.machine;
    let one = m.one.lock().await;
    let prefix = state.snapshot().settings.container_prefix.clone();
    let spec = match state
        .cdi()
        .ensure_fresh(&prefix, crate::runtime::cdi::load_timeout(state))
        .await
    {
        Ok(e) => format!("lmgw's own CDI spec after the setup: {e:?}"),
        Err(e) => format!("lmgw's own CDI spec after the setup could not be written: {e}"),
    };
    tracing::info!("{spec}");
    let ran = run(state).await;
    let nvidia_ok = ran.card == card::Card::NvidiaOk;
    let check = publish(state, ran);
    drop(one);
    tracing::info!(
        "machine check after the setup: {:?}: {}",
        check.verdict,
        check.headline
    );
    let probe_installed = nvidia_ok && probe_again(state, detect).await;
    (
        check,
        Fixed {
            spec,
            probe_installed,
        },
    )
}

/// Replace the ledger's probe when it reads nothing and a newly detected one
/// reads the card. Both reads on the blocking pool: NVML calls the driver.
async fn probe_again(state: &SharedState, detect: DetectProbe) -> bool {
    let current = state.vram.probe();
    let reads = tokio::task::spawn_blocking(move || current.devices().is_ok())
        .await
        .unwrap_or(false);
    if reads {
        return false;
    }
    let found = tokio::task::spawn_blocking(move || {
        let probe = detect();
        let reads = probe.devices();
        (probe, reads)
    })
    .await;
    match found {
        Ok((probe, Ok(devices))) => {
            tracing::info!(
                "the ledger's GPU probe read nothing since boot and reads {} card(s) now ({}): \
                 installed",
                devices.len(),
                probe.source()
            );
            state.vram.set_probe(probe);
            true
        }
        Ok((probe, Err(why))) => {
            tracing::info!(
                "the ledger's GPU probe still reads nothing after the setup ({}): {why}",
                probe.source()
            );
            false
        }
        Err(e) => {
            tracing::warn!("detecting the GPU probe again did not finish: {e}");
            false
        }
    }
}
