//! The install step: `pkexec dnf install -y <rpm>`, and whether this host
//! can run it at all.
//!
//! Self-install is RPM-only. The same binary built and run on a host without
//! `dnf` (Arch, an AppImage) has nothing to hand the package to, so a caller
//! gates its whole install path on [`can_self_install`] — and says where to
//! get the new version instead of offering a button whose only outcome is
//! "could not run pkexec/dnf".

use std::fmt;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// Whether this host can install the RPM a manifest points at: both halves
/// of `pkexec dnf install` have to exist.
///
/// Checked at prompt time rather than at build time — the same binary is built
/// on Fedora and run from source on other distributions, so the package
/// manager is a property of the host, not of the build.
pub fn can_self_install() -> bool {
    ["pkexec", "dnf"].iter().all(|cmd| on_path(cmd))
}

/// `cmd` resolves to an executable somewhere on `PATH`.
fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

/// Why [`install_rpm`] did not install. Displays as `dnf exited with <status>`
/// or `could not run pkexec/dnf: <error>`.
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    /// pkexec ran and exited unsuccessfully: dnf failed, or the user
    /// dismissed the polkit prompt.
    Exited(ExitStatus),
    /// pkexec could not be started.
    Spawn(std::io::Error),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::Exited(status) => write!(f, "dnf exited with {status}"),
            InstallError::Spawn(e) => write!(f, "could not run pkexec/dnf: {e}"),
        }
    }
}

// The spawn error is in the message already, so its source is the io
// error's own, as for `feed::Error`.
impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstallError::Exited(_) => None,
            InstallError::Spawn(e) => e.source(),
        }
    }
}

/// Install `rpm` via polkit (`pkexec dnf install -y <rpm>`); `dnf install` of
/// a newer RPM upgrades the package in place. Blocks until dnf is done, which
/// includes the user's password prompt — call it from a blocking thread
/// (`tokio::task::spawn_blocking`). The file is left in place: removing it is
/// the caller's, whatever the outcome.
///
/// `rpm` is the path the caller downloaded to, never one built from the
/// manifest (see the crate docs).
pub fn install_rpm(rpm: &Path) -> Result<(), InstallError> {
    let status = Command::new("pkexec")
        .arg("dnf")
        .arg("install")
        .arg("-y")
        .arg(rpm)
        .status()
        .map_err(InstallError::Spawn)?;
    if status.success() {
        Ok(())
    } else {
        Err(InstallError::Exited(status))
    }
}
