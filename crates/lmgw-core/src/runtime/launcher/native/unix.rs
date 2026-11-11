//! The native launcher's Unix side: a child's own process group, its death
//! signal, and the signals a stop sends.

use std::io;

/// Give the child a process group of its own (so a stop reaches what the
/// engine started, and a terminal's Ctrl-C to lmgw does not reach the
/// engine first) and, on Linux, `PR_SET_PDEATHSIG(SIGKILL)`, checked against
/// a parent that died before the call took effect.
pub(super) fn configure(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // SAFETY: runs in the forked child before the exec, where only
        // async-signal-safe work is allowed: two raw system calls and a
        // comparison, no allocation, no lock.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // lmgw died between the fork and the prctl: the signal will
                // never come, so the engine must not start.
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::other("lmgw exited while starting the engine"));
                }
                Ok(())
            });
        }
    }
}

/// `SIGTERM` to the child's process group: true, the stop escalates to
/// [`kill`] after the grace.
pub(super) fn ask_to_stop(pid: u32) -> bool {
    signal_group(pid, libc::SIGTERM);
    true
}

/// `SIGKILL` to the child's process group.
pub(super) fn kill(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

fn signal_group(pid: u32, sig: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: a plain system call on a group id lmgw created (the child's
    // own pid, `process_group(0)`); ESRCH once the group is gone is the
    // requested state.
    unsafe {
        libc::kill(-pgid, sig);
    }
}
