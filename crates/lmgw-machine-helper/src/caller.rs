//! Who started the helper and where it runs (§3.1): `apply` only as root
//! through pkexec, `run` only as root inside the system task. Neither runs
//! as root by any other route.

use lmgw_machine::helper::{UNIT_CGROUP, UNIT_SERVICE};
use lmgw_machine::plan::embedded::PKEXEC;

/// What the process knows about itself, read once in `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The real user ID.
    pub ruid: u32,
    /// The effective user ID.
    pub euid: u32,
    /// `PKEXEC_UID`, which pkexec sets to the user who asked.
    pub pkexec_uid: Option<String>,
    /// `/proc/self/cgroup`, or why it could not be read.
    pub cgroup: Result<String, String>,
}

impl Caller {
    /// This process.
    pub fn of_this_process() -> Self {
        // SAFETY: getuid and geteuid have no preconditions and cannot fail.
        let (ruid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        Self {
            ruid,
            euid,
            pkexec_uid: std::env::var_os("PKEXEC_UID").map(|v| v.to_string_lossy().into_owned()),
            cgroup: std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string()),
        }
    }

    /// Root in both IDs: a copy of the helper with a setuid bit (which the
    /// RPM guard refuses) would have root's effective ID only.
    fn root(&self, verb: &str) -> Result<(), String> {
        if self.ruid == 0 && self.euid == 0 {
            Ok(())
        } else {
            Err(format!(
                "{verb} runs only as root, and this process runs as user {} (effective user {})",
                self.ruid, self.euid
            ))
        }
    }

    /// `PKEXEC_UID` as a user ID: decimal digits only, within 32 bits.
    pub fn asker(&self) -> Option<u32> {
        self.pkexec_uid
            .as_deref()
            .filter(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|uid| uid.parse().ok())
    }

    /// May `apply` act: root, started by pkexec (`PKEXEC_UID` a user ID)?
    pub fn may_apply(&self) -> Result<(), String> {
        self.root("apply")?;
        match &self.pkexec_uid {
            Some(_) if self.asker().is_some() => Ok(()),
            Some(uid) => Err(format!(
                "apply runs only when {PKEXEC} started it, and PKEXEC_UID is {uid:?}, not a \
                 user ID"
            )),
            None => Err(format!(
                "apply runs only when {PKEXEC} started it, and PKEXEC_UID is not set"
            )),
        }
    }

    /// May `run` act: root, inside the system task's cgroup?
    pub fn may_run(&self) -> Result<(), String> {
        self.root("run")?;
        let text = self
            .cgroup
            .as_ref()
            .map_err(|e| format!("/proc/self/cgroup could not be read: {e}"))?;
        match cgroup_path(text) {
            Some(path) if path == UNIT_CGROUP => Ok(()),
            Some(path) => Err(format!(
                "run runs only inside the system task {UNIT_SERVICE} (cgroup {UNIT_CGROUP}), and \
                 this process is in {path}"
            )),
            None => Err(format!(
                "run runs only inside the system task {UNIT_SERVICE}, and /proc/self/cgroup has \
                 no cgroup v2 line (0::): {text:?}"
            )),
        }
    }
}

/// The cgroup v2 path in `/proc/self/cgroup`'s text: its `0::` line.
pub fn cgroup_path(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("0::"))
}
