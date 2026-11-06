//! The install step: `/usr/bin/pkexec /usr/bin/dnf install -y <rpm>`, and
//! whether this host can run it at all.
//!
//! Self-install is RPM-only. The same binary built and run on a host without
//! `dnf` (Arch, an AppImage) has nothing to hand the package to, so a caller
//! gates its whole install path on [`can_self_install`] — and says where to
//! get the new version instead of offering a button whose only outcome is
//! "could not run pkexec/dnf".
//!
//! Both programs are named by absolute path, never looked up in `PATH`. The
//! caller's `PATH` is the user's, and on Fedora the user-writable
//! `~/.local/bin` comes first in it, so any process running as the user could
//! put a `pkexec` there that asks for the password itself. pkexec does not
//! protect the second half either: it looks a relative program name up in the
//! caller's `PATH` before it clears the environment (polkit 127,
//! `src/programs/pkexec.c`; the safe `PATH` it sets is only the child's), so
//! `pkexec dnf` would run a planted `~/.local/bin/dnf` as root once the user
//! authenticates.
//!
//! That closes the planted-program hole and nothing more. It does not make the
//! install trustworthy: a process of the same user can still ptrace lmgw (yama
//! `ptrace_scope` is 0 on Fedora) and steer what it installs or when, and the
//! RPM itself is unsigned, so whoever can alter the download (or the file
//! between the download and dnf reading it) gets root. Not yet done: RPM
//! signing in CI, a root-side pinned key, and verification of a root-owned
//! copy of the file before dnf installs it. That is the owner's decision.
//! The download itself goes to a private file ([`crate::private_dir`], created
//! `O_EXCL | O_NOFOLLOW`, 0600), which keeps other users out but is no defence
//! against the same user.

use std::fmt;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// polkit's pkexec. `/usr/bin`, not `/bin`: Fedora's `/bin` is a symlink to it.
const PKEXEC: &str = "/usr/bin/pkexec";
/// The package manager pkexec runs as root (dnf5 on Fedora 41 and later,
/// through the `/usr/bin/dnf` symlink).
const DNF: &str = "/usr/bin/dnf";

/// Whether this host can install the RPM a manifest points at: both halves
/// of `pkexec dnf install` have to exist, at the paths [`install_rpm`] runs.
///
/// Checked at prompt time rather than at build time — the same binary is built
/// on Fedora and run from source on other distributions, so the package
/// manager is a property of the host, not of the build.
pub fn can_self_install() -> bool {
    [PKEXEC, DNF].iter().all(|p| Path::new(p).is_file())
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

/// Install `rpm` via polkit (`/usr/bin/pkexec /usr/bin/dnf install -y <rpm>`);
/// `dnf install` of a newer RPM upgrades the package in place. Blocks until
/// dnf is done, which includes the user's password prompt — call it from a
/// blocking thread (`tokio::task::spawn_blocking`). The file is left in place:
/// removing it is the caller's, whatever the outcome.
///
/// `rpm` is the path the caller downloaded to, never one built from the
/// manifest (see the crate docs).
pub fn install_rpm(rpm: &Path) -> Result<(), InstallError> {
    let status = install_command(rpm).status().map_err(InstallError::Spawn)?;
    if status.success() {
        Ok(())
    } else {
        Err(InstallError::Exited(status))
    }
}

fn install_command(rpm: &Path) -> Command {
    let mut cmd = Command::new(PKEXEC);
    cmd.arg(DNF).arg("install").arg("-y").arg(rpm);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn install_runs_pkexec_and_dnf_by_absolute_path() {
        let cmd = install_command(Path::new("/tmp/app-update.rpm"));
        assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/pkexec"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["/usr/bin/dnf", "install", "-y", "/tmp/app-update.rpm"].map(OsStr::new)
        );
    }

    #[test]
    fn availability_reads_the_two_paths_the_install_runs() {
        // The availability check and the install command name the same two absolute paths,
        // not a bare name that a PATH lookup would resolve.
        for p in [PKEXEC, DNF] {
            assert!(Path::new(p).is_absolute(), "{p} is not absolute");
        }
        let cmd = install_command(Path::new("/x.rpm"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(cmd.get_program(), OsStr::new(PKEXEC));
        assert_eq!(args[0], OsStr::new(DNF));
        assert_eq!(
            can_self_install(),
            Path::new(PKEXEC).is_file() && Path::new(DNF).is_file()
        );
    }
}
