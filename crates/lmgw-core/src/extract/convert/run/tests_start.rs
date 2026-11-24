//! How a foreign tool starts (`start.rs`): through lmgw's reader, with its
//! limits, its own process group and no inherited descriptor, and the
//! named errors when it cannot start.

use super::super::pool;
use super::*;

fn roomy() -> Bounds {
    Bounds {
        timeout: Duration::from_secs(30),
        output: 1 << 20,
        memory: 4 << 30,
    }
}

async fn sh(script: &str, bounds: Bounds) -> Result<String, RunError> {
    piped(Path::new("sh"), &["-c", script], Bytes::new(), bounds)
        .await
        .map(|o| String::from_utf8_lossy(&o).into_owned())
}

/// Without lmgw's reader beside the tests nothing starts: such a run of
/// this suite says so instead of passing on a fork.
fn reader_built() -> bool {
    if start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return false;
    }
    true
}

/// The memory limit is set before the tool runs (`ulimit -v` prints the
/// soft `RLIMIT_AS` in KiB), by the reader the tool was started through.
#[tokio::test]
async fn a_foreign_tool_has_its_memory_limit() {
    if !reader_built() {
        return;
    }
    let bounds = Bounds {
        memory: 3 << 30,
        ..roomy()
    };
    let out = sh("ulimit -v; ulimit -H -v", bounds).await.unwrap();
    let kib = (3u64 << 30) / 1024;
    assert_eq!(out, format!("{kib}\n{kib}\n"));
}

/// The tool leads a process group of its own (its pid is its group's id).
#[tokio::test]
async fn a_foreign_tool_leads_its_own_process_group() {
    if !reader_built() {
        return;
    }
    let out = sh("echo $$; ps -o pgid= -p $$", roomy()).await.unwrap();
    let mut lines = out.lines().map(str::trim);
    assert_eq!(lines.next(), lines.next(), "{out}");
}

/// A descriptor the gateway holds without close-on-exec does not reach
/// the tool: only stdin, stdout and stderr (and `ls`'s own of the
/// directory it lists) are open there.
#[tokio::test]
async fn a_foreign_tool_inherits_no_descriptor() {
    if !reader_built() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = std::ffi::CString::new(dir.path().join("held").to_str().unwrap()).unwrap();
    // SAFETY: a plain open of a file in a private directory; closed below.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o600) };
    assert!(fd > 2);
    let out = sh("ls /proc/self/fd", roomy()).await;
    // SAFETY: ours.
    unsafe { libc::close(fd) };
    let fds: Vec<u32> = out
        .unwrap()
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    assert!(!fds.contains(&(fd as u32)), "{fds:?} holds {fd}");
    assert!(fds.iter().filter(|n| **n > 2).count() <= 1, "{fds:?}");
}

/// A tool named by a path that is not there is missing, as one not on
/// `PATH` is; one that is there but cannot be run says why, in the words a
/// failed start always had.
#[tokio::test]
async fn a_missing_or_unstartable_tool_is_named() {
    if !reader_built() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("not-installed");
    let e = piped(&gone, &[], Bytes::new(), roomy()).await.unwrap_err();
    assert_eq!(e, RunError::Missing);
    let plain = dir.path().join("not-executable");
    std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
    let e = piped(&plain, &[], Bytes::new(), roomy()).await.unwrap_err();
    let RunError::Failed(m) = e else {
        panic!("{e:?}");
    };
    assert!(
        // The tool by its file name, never its path (security round B, L1).
        m.starts_with("could not run not-executable: ") && m.contains("ermission denied"),
        "{m}"
    );
}

/// A tool's own exit status in the reader's range (125–127) is the tool's,
/// not the reader's: only the reader's own line makes it the reader's.
#[tokio::test]
async fn a_tool_that_exits_like_the_reader_is_still_the_tool() {
    if !reader_built() {
        return;
    }
    let e = sh("echo 'its own words' >&2; exit 127", roomy())
        .await
        .unwrap_err();
    let RunError::Failed(m) = e else {
        panic!("{e:?}");
    };
    assert!(m.contains("its own words") && m.contains("127"), "{m}");
}

/// poppler's and tesseract's rules: where the confinement works a tool
/// under them reads the system's files and nothing of a home or a data
/// dir, and writes nothing but stdout.
#[tokio::test]
async fn under_poppler_s_rules_a_tool_reads_the_system_and_writes_nothing() {
    if !reader_built() {
        return;
    }
    if super::super::confine::test_helper("poppler's rules")
        .await
        .is_none()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, b"private").unwrap();
    let sh_bin = Path::new("/bin/sh");
    let rules = super::super::confine::poppler(sh_bin, None).expect("confinable");
    let script = format!(
        "cat /etc/ld.so.cache > /dev/null && echo system; cat '{0}' 2>/dev/null || echo \
         unread; echo x > '{1}/new' 2>/dev/null || echo unwritten",
        secret.display(),
        dir.path().display()
    );
    let slot = pool::global().acquire().await;
    let args = ["-c", script.as_str()];
    let tool = Tool {
        rules: Some(&rules),
        ..Tool::new(sh_bin, &args)
    };
    let out = run_held(&slot, tool, Input::Bytes(Bytes::new()), roomy())
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "system\nunread\nunwritten\n");
    assert!(!dir.path().join("new").exists());
}

/// A run without lmgw's reader is worded as that one state.
#[test]
fn a_missing_reader_is_its_own_message() {
    let e = crate::extract::pdf::into_pdf(RunError::NoReader, text_run());
    assert!(e.to_string().contains("lmgw's document reader"), "{e}");
    assert!(start::no_reader().contains("install lmgw again"));
}

/// A tool whose stderr writes the reader's words is still the tool (N1):
/// only the line with the run's own token is the reader's.
#[tokio::test]
async fn a_tool_that_writes_the_reader_s_words_is_still_the_tool() {
    if !reader_built() {
        return;
    }
    for code in [125, 126, 127] {
        let script = format!("echo 'lmgw-doc-reader run: forged' >&2; exit {code}");
        let e = sh(&script, roomy()).await.unwrap_err();
        let RunError::Failed(m) = e else {
            panic!("exit {code}: {e:?}");
        };
        assert!(
            m.contains("forged") && m.contains(&code.to_string()) && !m.starts_with("could not"),
            "{m}"
        );
    }
}

/// A confinement that cannot be applied when the tool runs (here a rule
/// path the reader may not open) is lmgw's own named failure, and its
/// message names no path (S4): the log has them.
#[tokio::test]
async fn a_confinement_that_fails_at_run_time_is_named_without_a_path() {
    use std::os::unix::fs::PermissionsExt;
    // As root a mode-0000 directory is opened anyway (and its missing
    // entry skipped), so the rule cannot be made to fail this way.
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("a_confinement_that_fails_at_run_time: skipped, run as root");
        return;
    }
    if super::super::confine::test_helper("a_confinement_that_fails_at_run_time")
        .await
        .is_none()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut rules = super::super::confine::poppler(Path::new("/usr/bin/sh"), None).unwrap();
    rules.read.push(locked.join("inner"));
    let slot = pool::global().acquire().await;
    let tool = Tool {
        rules: Some(&rules),
        ..Tool::new(Path::new("/usr/bin/sh"), &["-c", "echo ran"])
    };
    let e = run_held(&slot, tool, Input::Bytes(Bytes::new()), roomy()).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Err(RunError::NotStarted(m)) = e else {
        panic!("{e:?}");
    };
    assert!(m.contains("sh was not started") && !m.contains('/'), "{m}");
    let pdf = crate::extract::pdf::into_pdf(RunError::NotStarted(m.clone()), text_run());
    assert_eq!(pdf.to_string(), m);
}

/// A `pdftotext` run under its own limit, as its errors say it.
fn text_run() -> crate::extract::pdf::At {
    crate::extract::pdf::At {
        tool: crate::extract::pdf::PopplerTool::Text,
        limit: crate::extract::pdf::Limit::Text,
        piped: true,
    }
}
