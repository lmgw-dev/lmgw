//! CDI, the Container Device Interface (machine-check design §1.2, §3.6):
//! what a spec file says, how lmgw's own spec is judged, and how Podman
//! resolves the system's spec dirs.
//!
//! - [`spec`]: a spec file's devices and the host paths it names;
//! - [`own`]: lmgw's own spec (ruling 4) — its stamp, and whether it is still
//!   true of this machine;
//! - [`system`]: Podman's configured spec dirs (containers.conf) and the
//!   device names they resolve to, conflicts included.

pub mod own;
pub mod spec;
pub mod system;

pub use own::{
    judge, Blocked, Current, FileFingerprint, Judgement, OnDisk, Stale, Stamp, ToolStamp,
};
pub use spec::{Spec, SpecError};
pub use system::{configured_spec_dirs, resolve, Resolution, SpecDirs};

/// The device kind nvidia-ctk's spec defines, and the only kind lmgw's own
/// spec has: `nvidia.com/gpu=all`, `=0`, `=GPU-<uuid>`.
pub const NVIDIA_KIND: &str = "nvidia.com/gpu";
