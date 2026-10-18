use std::os::fd::AsRawFd;
use std::process::Command;

use super::*;

/// `/proc/meminfo` opened the way WebKitGTK's memory-pressure monitor holds
/// it in the desktop app: inheritable. Rust opens close-on-exec, so the flag
/// is cleared by hand.
fn inheritable_meminfo() -> std::fs::File {
    let file = std::fs::File::open("/proc/meminfo").expect("open /proc/meminfo");
    // SAFETY: F_SETFD on a descriptor `file` owns, with no pointer argument.
    let cleared = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) };
    assert_eq!(cleared, 0, "{}", std::io::Error::last_os_error());
    file
}

/// Whether a `sh` started by `cmd`'s setup has descriptor `fd` open: the
/// shell's own table, read with a `stat` that opens nothing.
fn child_has_fd(fd: i32, setup: impl FnOnce(&mut Command)) -> bool {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &format!("[ -e /proc/$$/fd/{fd} ]")]);
    setup(&mut cmd);
    let status = cmd.status().expect("spawn sh");
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("sh ended with {other:?}"),
    }
}

/// The leak the module exists for, and its cure: a plain child inherits an
/// inheritable descriptor (so the check can see one), a `stdio_only` child
/// does not.
#[test]
fn a_stdio_only_child_inherits_no_descriptor_beyond_stdio() {
    let meminfo = inheritable_meminfo();
    let fd = meminfo.as_raw_fd();
    assert!(fd > 2);
    assert!(
        child_has_fd(fd, |_| {}),
        "a plain child inherits descriptor {fd}"
    );
    assert!(
        !child_has_fd(fd, |cmd| {
            stdio_only(cmd);
        }),
        "a stdio_only child does not inherit descriptor {fd}"
    );
    assert!(!child_has_fd(fd, |cmd| {
        system_tool(cmd);
    }));
}

/// Its stdio is untouched: piped output still arrives.
#[test]
fn a_stdio_only_child_keeps_its_stdio() {
    let _meminfo = inheritable_meminfo();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo out; echo err >&2"]);
    let out = stdio_only(&mut cmd).output().expect("spawn sh");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
}

/// Close-on-exec, not closed: std's pipe that reports a failed exec stays
/// open until the exec, so a missing program is still an error at the spawn.
#[test]
fn a_failed_exec_is_still_reported() {
    let mut cmd = Command::new("/nonexistent/lmgw-child-test");
    let err = stdio_only(&mut cmd).spawn().expect_err("no such program");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
}

/// A system tool gets no `LD_LIBRARY_PATH`, whether the caller set one or
/// lmgw's own environment carries it (cargo's, an AppImage's).
#[test]
fn a_system_tool_gets_no_loader_path() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "[ -z \"${LD_LIBRARY_PATH+set}\" ]"])
        .env("LD_LIBRARY_PATH", "/nowhere");
    let status = system_tool(&mut cmd).status().expect("spawn sh");
    assert!(status.success(), "LD_LIBRARY_PATH reached the tool");

    // The tokio path goes through the same std command.
    let mut tokio_cmd = tokio::process::Command::new("sh");
    tokio_cmd.env("LD_LIBRARY_PATH", "/nowhere");
    system_tool_tokio(&mut tokio_cmd);
    assert!(tokio_cmd
        .as_std()
        .get_envs()
        .any(|(k, v)| k == "LD_LIBRARY_PATH" && v.is_none()));
}
