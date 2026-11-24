//! How a run ends when the tool ends by itself (security round B, L3): what
//! it left running in its group is killed then too, and a stderr held open
//! by a process outside the group is read no longer than
//! `STDERR_DRAIN_TIMEOUT`.

use super::*;

fn roomy() -> Bounds {
    Bounds {
        timeout: Duration::from_secs(60),
        output: 1 << 20,
        memory: 4 << 30,
    }
}

/// Whether `pid` is gone or a zombie.
fn ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
    }
}

async fn sh(script: &str, arg: &Path) -> Result<Vec<u8>, RunError> {
    let a = arg.to_string_lossy().into_owned();
    let args = ["-c", script, "sh", a.as_str()];
    piped(Path::new("/bin/sh"), &args, Bytes::new(), roomy()).await
}

fn pid_in(file: &Path) -> u32 {
    std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn what_a_tool_left_behind_ends_with_it() {
    if start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("left.pid");
    // A helper that closed stdout and stderr, so the run ends at once.
    let out = sh(
        "(exec >/dev/null 2>&1; sleep 30) & echo $! > \"$1\"; echo done",
        &pid,
    )
    .await
    .unwrap();
    assert_eq!(out, b"done\n");
    let left = pid_in(&pid);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ended(left) {
        assert!(
            std::time::Instant::now() < deadline,
            "the helper {left} outlived its run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // One that keeps stderr open no longer holds the run.
    let started = std::time::Instant::now();
    let out = sh(
        "(exec >/dev/null; sleep 30) & echo $! > \"$1\"; echo done",
        &pid,
    )
    .await
    .unwrap();
    assert_eq!(out, b"done\n");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let left = pid_in(&pid);
    while !ended(left) {
        assert!(std::time::Instant::now() < deadline + Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_stderr_held_outside_the_group_is_read_only_so_long() {
    if start::starter().is_none()
        || std::process::Command::new("setsid")
            .arg("--version")
            .output()
            .is_err()
    {
        eprintln!("skipped: no reader beside the tests, or no setsid");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("away.pid");
    let started = std::time::Instant::now();
    // It leaves the group (`setsid`) with stderr open, then the tool fails.
    let e = sh(
        "setsid sh -c 'exec >/dev/null; echo $$ > \"$1\"; sleep 30' sh \"$1\" & \
         while [ ! -s \"$1\" ]; do :; done; echo oops >&2; exit 3",
        &pid,
    )
    .await
    .unwrap_err();
    let took = started.elapsed();
    let RunError::Failed(m) = e else {
        panic!("{e:?}");
    };
    assert!(m.contains("STDERR_DRAIN_TIMEOUT"), "{m}");
    // What it wrote before the drain gave up is kept (review N2).
    assert!(m.starts_with("oops"), "{m}");
    assert!(
        took >= STDERR_DRAIN_TIMEOUT && took < STDERR_DRAIN_TIMEOUT + Duration::from_secs(5),
        "{took:?}"
    );
    // SAFETY: a signal to the test's own stray process.
    unsafe { libc::kill(pid_in(&pid) as i32, libc::SIGKILL) };
}

/// The review of round B, S3: a process that left the group keeps its CPU
/// time bound (`--cpu-seconds`) and the kernel ends it there; the runner
/// names the bound when it ends a tool.
#[tokio::test]
async fn the_cpu_bound_ends_a_process_that_left_the_group() {
    let Some(helper) = start::starter() else {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    };
    if std::process::Command::new("setsid")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: no setsid");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("spin.pid");
    let p = pid.to_string_lossy().into_owned();
    let script = "setsid sh -c 'echo $$ > \"$1\"; while :; do :; done' sh \"$1\" \
                  >/dev/null 2>&1 </dev/null &";
    let args = ["-c", script, "sh", p.as_str()];
    let argv = start::argv(
        None,
        Path::new("/bin/sh"),
        &args,
        (4 << 30, 1),
        false,
        &start::token(),
    );
    let status = tokio::process::Command::new(&helper)
        .args(argv)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "{status}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&pid).map_or(true, |m| m.len() == 0) {
        assert!(std::time::Instant::now() < deadline, "no pid written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let spin = pid_in(&pid);
    while !ended(spin) {
        if std::time::Instant::now() > deadline {
            // SAFETY: a signal to the test's own stray process.
            unsafe { libc::kill(spin as i32, libc::SIGKILL) };
            panic!("the escaped process {spin} outlived its CPU bound");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A tool the bound ended is named so.
    use std::os::unix::process::ExitStatusExt;
    let xcpu = std::process::ExitStatus::from_raw(libc::SIGXCPU);
    let bound = cpu::CpuBound::of(Duration::from_secs(300));
    let e = confined::ended(&xcpu, "", "ffmpeg", &helper, (None, &bound), "t0k3n").unwrap();
    let RunError::Failed(m) = e else {
        panic!("{e:?}");
    };
    assert!(
        m.starts_with("ffmpeg used its CPU time bound (") && m.contains("RLIMIT_CPU"),
        "{m}"
    );
}
