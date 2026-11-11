//! The machine check (machine-check design §6 "Driver facts", "The toolkit
//! by owner" and "Core"; WP4): the verdict and sentences on stated
//! machines, NVML only in a separate process and only while the driver's
//! two halves agree, the plan as the API shows it, the op, the self-admin
//! tool, the status summary, joined runs and the boot run with lmgw's own
//! spec, and the switch to the model settings for the card (WP12).
//!
//! Every machine is `support/machine_world.rs`'s: nothing of the computer
//! the suite runs on is read or run.

mod api;
mod blame;
mod container;
mod download;
pub mod driver_states;
mod fixed;
mod machines;
mod settings_fit;
mod spec_writes;
mod verdicts;
// Sets a directory's mode.
#[cfg(unix)]
mod words;
