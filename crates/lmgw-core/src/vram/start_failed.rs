//! A start that failed on the graphics card (machine-check design §2.5,
//! §2.6, WP5): what on the computer this row uses is a problem is asked of
//! the machine check ([`crate::machine::after_failed_start`]: the last
//! check while the machine is as it read it, else the card and its driver
//! read again, the full check after it in the background), and when
//! something is, that is the start's answer — [`GatewayError::GpuSetup`], a
//! 503 with the check's plain sentence, which the gate hands on to a
//! configured fallback (`gate::machine_setup`). Otherwise the start's own
//! error, as before.
//!
//! Every place a start's failure comes back through admission maps it here:
//! a request's admission (`queue.rs`, a dead container's restart included),
//! a join of a start in flight and a guest's start (`background.rs`), and
//! the starts nobody waits on (the operator's start, boot's and realtime's
//! warm starts) so the check and the tray see it too.

use crate::error::GatewayError;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::registry::RuntimeError;
use crate::state::SharedState;

use super::queue::upstream_error;

/// `e`, the failed start of `runtime`'s container, as admission answers it:
/// [`GatewayError::GpuSetup`] when the machine check names a cause this
/// row's start meets ([`crate::machine::after_failed_start`]), else the
/// start's own error (`upstream_error`). The start's own words — the
/// registry's message and the container's last log lines, Podman's text
/// included — go after the plain sentence, in "(its start said: …)".
///
/// Every caller sits on a request's admission (a turn's included), so the
/// read is boxed here: its poll frame is not added to theirs.
pub(crate) async fn start_error(
    state: &SharedState,
    runtime: &ModelRuntime,
    e: RuntimeError,
) -> GatewayError {
    match Box::pin(crate::machine::after_failed_start(state, runtime, &e)).await {
        Some(blamed) => GatewayError::GpuSetup {
            model: runtime.model_id.clone(),
            problem: blamed.words(),
            detail: format!(" (its start said: {e})"),
        },
        None => upstream_error(e),
    }
}
