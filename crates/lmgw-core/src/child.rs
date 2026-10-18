//! What a process lmgw starts inherits from it.
//!
//! **Descriptors.** lmgw's process holds descriptors that are not
//! close-on-exec, and not all of them are its own: in the desktop app
//! WebKitGTK's memory-pressure monitor keeps `/proc/meminfo`,
//! `/proc/zoneinfo` and the app cgroup's `memory.current`, `memory.max` and
//! `memory.high` open, and the NVIDIA driver its `/dev/dri/renderD128`
//! handles; whatever started lmgw can hand down more (the lock file of a
//! `flock FILE cmd`, a terminal's `/dev/ptmx`, an AppImage's mount
//! directory). A child inherits every one of them, and `podman` passes them
//! on to `conmon` and `pasta`. pasta runs in an SELinux domain of its own
//! (`pasta_t`), so at its exec the kernel revokes each descriptor that domain
//! may not use and logs an AVC denial for it: every container start a burst
//! of setroubleshoot alerts on the desktop, measured as one denial
//! per such descriptor per start.
//!
//! **The loader path.** `cargo run` and `cargo nextest` point
//! `LD_LIBRARY_PATH` into the target directory, and the AppImage into its
//! FUSE mount. pasta's dynamic loader searches every entry for libc (six
//! lookups an entry, and again after pasta re-executes itself as
//! `pasta.avx2`), and an entry `pasta_t` may not search — a FUSE mount, a
//! tree a `:Z` bind mount relabelled `container_file_t` — is twelve denials
//! per entry per start. The tools lmgw runs are system binaries that resolve
//! their libraries the system's way, so they get no `LD_LIBRARY_PATH`.
//!
//! The working directory is left alone: a container start from a directory
//! `pasta_t` may not search causes no denial (measured), and an isolated MCP
//! server's run flags may name a relative bind-mount source that resolves
//! against it.

use std::os::unix::process::CommandExt;
use std::process::Command;

/// A system tool lmgw runs — `podman`, `nvidia-smi`: it inherits no
/// descriptor beyond stdin, stdout and stderr ([`stdio_only`]) and no
/// `LD_LIBRARY_PATH`.
pub fn system_tool(cmd: &mut Command) -> &mut Command {
    stdio_only(cmd).env_remove("LD_LIBRARY_PATH")
}

/// [`system_tool`] on a tokio command, through the std command it wraps.
pub fn system_tool_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    system_tool(cmd.as_std_mut());
    cmd
}

/// The child inherits no descriptor beyond stdin, stdout and stderr: every
/// descriptor from 3 up is made close-on-exec between the fork and the exec.
///
/// Close-on-exec rather than closed, because std reports a failed exec back
/// to the parent through a close-on-exec pipe of its own that has to stay
/// open until the exec. The redirections of 0–2 are already in place when
/// this runs (std's `pre_exec` order).
///
/// `close_range(2)` with `CLOSE_RANGE_CLOEXEC` needs Linux 5.11. On an older
/// kernel the call fails and the child inherits what it would have without
/// this; the spawn itself goes on.
pub fn stdio_only(cmd: &mut Command) -> &mut Command {
    // SAFETY: the closure runs in the forked child before the exec, where
    // only async-signal-safe work is allowed. It makes one raw system call:
    // no allocation, no lock, no memory of the parent's touched.
    unsafe {
        cmd.pre_exec(|| {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
