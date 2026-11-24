//! The runner against Unix's own tools (sh, yes, cat, sleep, false); the
//! PDF reader's tests in `extract/pdf.rs` run it with poppler.

use super::super::pool_size::Machine;
use super::*;

/// Generous bounds for a tool that is expected to end by itself.
fn roomy() -> Bounds {
    Bounds {
        timeout: Duration::from_secs(30),
        output: 1 << 30,
        memory: 4 << 30,
    }
}

#[tokio::test]
async fn a_missing_tool_is_its_own_error() {
    let e = piped(
        Path::new("a-tool-that-is-not-installed"),
        &[],
        Bytes::new(),
        roomy(),
    )
    .await
    .unwrap_err();
    assert_eq!(e, RunError::Missing);
}

#[tokio::test]
async fn a_run_that_overstays_is_stopped() {
    // `sleep` ignores stdin and outlives a 100 ms budget.
    let bounds = Bounds {
        timeout: Duration::from_millis(100),
        ..roomy()
    };
    let e = piped(Path::new("sleep"), &["30"], Bytes::new(), bounds).await;
    assert_eq!(e.unwrap_err(), RunError::Timeout);
}

/// Whether `pid` is gone or a zombie waiting for its new parent to reap it:
/// either way it no longer runs.
fn ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // `pid (comm) S …`: the state follows the command's closing paren.
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
    }
}

/// A tool that starts a helper and is stopped for its timeout takes the
/// helper with it (LibreOffice's `soffice` script starts `soffice.bin`):
/// the whole process group is killed, not only the direct child.
#[tokio::test]
async fn a_stopped_run_takes_what_it_started_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("helper.pid");
    // The helper inherits stdout, so the run cannot end before the timeout.
    let script = "sleep 30 & echo $! > \"$1\"; wait";
    let bounds = Bounds {
        timeout: Duration::from_millis(500),
        ..roomy()
    };
    let file = pid_file.to_string_lossy().into_owned();
    let e = piped(
        Path::new("sh"),
        &["-c", script, "sh", &file],
        Bytes::new(),
        bounds,
    )
    .await;
    assert_eq!(e.unwrap_err(), RunError::Timeout);
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SIGKILL is delivered at once; the reaping by the helper's new parent
    // can trail it by a moment.
    let started = std::time::Instant::now();
    while !ended(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the helper {pid} still runs after its tool was stopped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A run whose future is dropped mid-run (a client that went away; not the
/// run's own timeout) gives its slot back and takes its process group with
/// it.
#[tokio::test]
async fn a_run_dropped_mid_way_gives_its_slot_back_and_stops_its_group() {
    let pool = pool::ToolPool::new(Machine {
        cores: 1,
        memory: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("helper.pid");
    let file = pid_file.to_string_lossy().into_owned();
    let task = {
        let pool = pool.clone();
        tokio::spawn(async move {
            // The helper inherits stdout, so the run is still going when it
            // is dropped.
            let script = "sleep 30 & echo $! > \"$1\"; wait";
            piped_on(
                &pool,
                Path::new("sh"),
                &["-c", script, "sh", &file],
                Bytes::new(),
                roomy(),
            )
            .await
        })
    };
    let started = std::time::Instant::now();
    let pid: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            break pid;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "no helper");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(pool.facts().running, 1);
    assert!(!ended(pid));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The slot goes back once the stopped tool has been reaped, not before.
    while pool.facts().running > 0 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the slot never came back after its run was dropped"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    while !ended(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the helper {pid} still runs after its run was dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_large_input_does_not_deadlock_the_pipe() {
    // `cat` echoes stdin to stdout: 4 MiB is far past both pipe buffers,
    // so a writer that shared the reader's task would hang.
    let data = Bytes::from(vec![b'x'; 4 << 20]);
    let out = piped(Path::new("cat"), &[], data.clone(), roomy())
        .await
        .unwrap();
    assert_eq!(out.len(), data.len());
}

#[tokio::test]
async fn a_failing_tool_reports_its_exit_status() {
    let e = piped(Path::new("false"), &[], Bytes::new(), roomy())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, RunError::Failed(m) if m.contains("exit status")),
        "{e:?}"
    );
}

#[tokio::test]
async fn output_past_the_bound_is_stopped() {
    // `yes` writes forever; the run is stopped when stdout crosses the bound.
    let started = std::time::Instant::now();
    let bounds = Bounds {
        output: 1 << 20,
        ..roomy()
    };
    let e = piped(Path::new("yes"), &[], Bytes::new(), bounds)
        .await
        .unwrap_err();
    assert_eq!(e, RunError::TooLarge);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn a_stderr_longer_than_what_is_kept_says_more_without_a_count() {
    // 400 KB of stderr against the 64 KiB kept: the count would be a lie.
    let script = "yes 'a line of complaint text' | head -c 400000 >&2; exit 3";
    let Err(RunError::Failed(m)) =
        piped(Path::new("sh"), &["-c", script], Bytes::new(), roomy()).await
    else {
        panic!("expected a failed run");
    };
    assert!(m.contains("\n(… more) ("), "{m}");
    assert!(!m.contains("more lines"), "{m}");
}

#[tokio::test]
async fn a_long_stderr_is_shown_as_its_first_lines_and_a_count() {
    let script = "for i in $(seq 1 100); do echo line$i >&2; done; exit 3";
    let Err(RunError::Failed(m)) =
        piped(Path::new("sh"), &["-c", script], Bytes::new(), roomy()).await
    else {
        panic!("expected a failed run");
    };
    assert!(m.starts_with("line1\nline2\n"), "{m}");
    assert!(m.contains("line20\n(… 80 more lines) ("), "{m}");
    assert!(!m.contains("line21"), "{m}");
}

/// The tool gets none of lmgw's loader path (`crate::child`'s module doc):
/// a `cargo test` run sets `LD_LIBRARY_PATH`, which a system tool must not
/// see.
#[tokio::test]
async fn a_tool_runs_without_the_loader_path() {
    let out = piped(
        Path::new("sh"),
        &["-c", "printf '%s' \"${LD_LIBRARY_PATH-unset}\""],
        Bytes::new(),
        roomy(),
    )
    .await
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "unset");
}

/// A run whose caller goes away is killed at once, and its slot stays in
/// use until the child has been reaped: the pool never hands out a slot
/// whose tool still has its memory.
#[tokio::test]
async fn a_dropped_run_keeps_its_slot_until_the_tool_is_reaped() {
    let pool = pool::ToolPool::new(Machine {
        cores: 1,
        memory: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let script = "echo $$ > \"$1\"; exec sleep 30";
    let args = ["-c", script, "sh", pid_file.to_str().unwrap()];
    let run = piped_on(&pool, Path::new("sh"), &args, Bytes::new(), roomy());
    assert!(tokio::time::timeout(Duration::from_millis(300), run)
        .await
        .is_err());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        // In this order: a slot counted free must mean the tool was reaped
        // (a killed child that nobody waited for still has its pid).
        if pool.facts().running == 0 {
            // SAFETY: signal 0 only asks whether the process exists.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            assert!(!exists, "the slot was freed before the tool was reaped");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "never reaped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // And the slot is there for the next run.
    let out = piped_on(&pool, Path::new("echo"), &["next"], Bytes::new(), roomy()).await;
    assert_eq!(out.unwrap(), b"next\n");
}

/// A file as the tool's stdin: read by the tool itself, by position if it
/// likes, with nothing piped.
#[tokio::test]
async fn a_file_can_be_the_tools_stdin() {
    use std::io::{Seek, Write};
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(b"0123456789").unwrap();
    f.rewind().unwrap();
    let slot = pool::global().acquire().await;
    // `tail -c 3` seeks to the end of a regular file.
    let out = run_held(
        &slot,
        Tool::new(Path::new("tail"), &["-c", "3"]),
        Input::File(f),
        roomy(),
    )
    .await
    .unwrap();
    assert_eq!(out, b"789");
}

/// A quiet tool's failure carries its exit status and nothing it wrote.
#[tokio::test]
async fn a_quiet_tools_stderr_stays_out_of_its_failure() {
    let slot = pool::global().acquire().await;
    let args = ["-c", "echo 'words of the document' >&2; exit 5"];
    let loud = Tool::new(Path::new("sh"), &args);
    let e = run_held(&slot, loud, Input::Bytes(Bytes::new()), roomy())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, RunError::Failed(m) if m.contains("words of the document")),
        "{e:?}"
    );
    let quiet = Tool {
        quiet: true,
        ..loud
    };
    let e = run_held(&slot, quiet, Input::Bytes(Bytes::new()), roomy())
        .await
        .unwrap_err();
    let RunError::Failed(m) = e else {
        panic!("{e:?}");
    };
    assert!(m.contains('5') && !m.contains("words"), "{m}");
}

/// A tool that limits itself is handed its memory limit as its first
/// arguments, after the flag it names (`echo` shows them in order).
#[tokio::test]
async fn a_tool_that_limits_itself_is_given_its_limit_first() {
    let slot = pool::global().acquire().await;
    let args = ["then", "its", "own"];
    let tool = Tool {
        limits_itself: Some("--memory-limit"),
        ..Tool::new(Path::new("echo"), &args)
    };
    let bounds = Bounds {
        memory: 123_456_789,
        ..roomy()
    };
    let out = run_held(&slot, tool, Input::Bytes(Bytes::new()), bounds)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out),
        "--memory-limit 123456789 then its own\n"
    );
}
