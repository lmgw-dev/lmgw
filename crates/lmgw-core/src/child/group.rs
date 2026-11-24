//! A tool and everything it starts, stopped together.
//!
//! `kill_on_drop` kills only the direct child. Fedora's `/usr/bin/soffice` is
//! a shell script that execs `oosplash`, which forks `soffice.bin`: stopping
//! the script alone orphans the real process, which goes on running (and
//! writing into a profile directory that is already deleted). So on unix the
//! child gets a process group of its own ([`own_group`]) and the whole group
//! is sent `SIGKILL` ([`KillGroup`]), as the native launcher does for an
//! engine (`runtime/launcher/native/unix.rs`).
//!
//! Windows has no process groups to signal; killing a tool's descendants
//! there (a job object, as the launcher uses) is not built yet, so a stopped
//! tool's children outlive it.

/// Wait for `child`, the leader of its own process group, to end, and
/// kill what is left of its group before it is reaped (security round B,
/// L3): a process the tool started that closed its stdout would otherwise
/// outlive a run that ended normally, with no timeout, no slot and no
/// owner. On Linux the end is seen through a pidfd, which does not reap
/// it: while the leader is a zombie its pid (the group's id) cannot be
/// taken by another process, so the group signal reaches only the tool's
/// own. Off Linux, or where no pidfd can be opened, the child is waited for
/// and its group is not signalled after a normal end yet.
pub async fn end_group(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
        if let Some(fd) = pidfd(pid) {
            if let Ok(fd) =
                tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)
            {
                // Readable once the process has ended.
                if let Ok(guard) = fd.readable().await {
                    drop(guard);
                    // SAFETY: a plain system call on the group of a child
                    // started with `process_group(0)` whose leader is not
                    // reaped yet (module doc).
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    child.wait().await
}

/// A pidfd for `pid` (Linux 5.3 and newer), or `None`.
#[cfg(target_os = "linux")]
fn pidfd(pid: libc::pid_t) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: `pidfd_open` takes a pid and flags; the descriptor is owned
    // below.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let fd = i32::try_from(fd).ok().filter(|f| *f >= 0)?;
    // SAFETY: just opened, owned by nothing else.
    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Run the child in a process group of its own (a no-op off unix).
pub fn own_group(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// `SIGKILL` to the process group `pid` leads, when dropped, unless
/// [`KillGroup::disarm`]ed: hold it from the spawn until the tool has ended
/// by itself, so a timeout or a dropped future stops the whole group.
///
/// Disarmed once the child has been reaped, because an ended group's id may
/// be reused by an unrelated process.
#[must_use = "the group is killed when this is dropped"]
pub struct KillGroup {
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
}

impl KillGroup {
    /// For the group `child` leads (`None` pid: the child already ended).
    pub fn of(child: &tokio::process::Child) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: child.id().and_then(|p| libc::pid_t::try_from(p).ok()),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child;
            Self {}
        }
    }

    /// The tool ended by itself: nothing to kill.
    pub fn disarm(self) {
        #[cfg(unix)]
        {
            let mut this = self;
            this.pgid = None;
        }
    }
}

impl Drop for KillGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: a plain system call on the group id of a child started
            // with `process_group(0)` (its own pid); ESRCH once the group is
            // gone is the requested state.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}
