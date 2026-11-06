//! The real host (§3.1): what a program the helper starts gets — the
//! environment cleared to root's fixed one, no inherited descriptor, every
//! line of both outputs — and how it writes a file. Only harmless programs
//! run here (`env`, `sh`), and files are written in a temporary directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use lmgw_machine::facts::ROOT_ENV;
use lmgw_machine::helper::Stream;
use lmgw_machine_helper::files::{create_new, replace_own};
use lmgw_machine_helper::real::{command, journal_datagram, RealHost};
use lmgw_machine_helper::{Ended, Host};

fn run(argv: &[&str]) -> (Vec<(Stream, String)>, Ended) {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let mut lines = Vec::new();
    let ended = RealHost
        .run(&argv, &mut |stream, line| {
            lines.push((stream, line.to_string()))
        })
        .unwrap();
    (lines, ended)
}

/// The test process has a whole environment; the program sees root's fixed
/// three variables and nothing else.
#[test]
fn a_program_gets_roots_fixed_environment_only() {
    assert!(std::env::vars_os().count() > 3);
    let (lines, ended) = run(&["/usr/bin/env"]);
    assert!(ended.ok());
    let mut got: Vec<String> = lines
        .into_iter()
        .map(|(s, l)| {
            assert_eq!(s, Stream::Stdout);
            l
        })
        .collect();
    got.sort();
    let mut want: Vec<String> = ROOT_ENV.iter().map(|(k, v)| format!("{k}={v}")).collect();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn both_outputs_arrive_line_by_line_with_the_exit() {
    let (lines, ended) = run(&[
        "/bin/sh",
        "-c",
        "echo out; echo err >&2; printf 'crlf\\r\\n'; printf 'last without newline'; exit 3",
    ]);
    assert_eq!(ended, Ended::exited(3));
    let stdout: Vec<&str> = lines
        .iter()
        .filter(|(s, _)| *s == Stream::Stdout)
        .map(|(_, l)| l.as_str())
        .collect();
    assert_eq!(stdout, ["out", "crlf", "last without newline"]);
    assert!(lines.contains(&(Stream::Stderr, "err".into())));
    // Its working directory is /.
    let (lines, _) = run(&["/bin/sh", "-c", "pwd"]);
    assert_eq!(lines, [(Stream::Stdout, "/".to_string())]);
}

/// A program that leaves a process behind holding its pipes (a scriptlet's
/// daemon): its run ends when the program does, with everything it wrote,
/// the last line without a newline and a long one included, not when the
/// process it left closes the pipes (D101).
#[test]
fn a_run_ends_with_its_program_not_with_what_it_left_behind() {
    let started = std::time::Instant::now();
    let (lines, ended) = run(&[
        "/bin/sh",
        "-c",
        "sleep 60 & echo $!; echo left >&2; head -c 200000 /dev/zero | tr '\\0' a; exit 3",
    ]);
    let elapsed = started.elapsed();
    let pid: libc::pid_t = lines[0].1.parse().expect("the pid it left behind");
    // SAFETY: a signal to the sleep this test started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "waited {elapsed:?} for the process left behind"
    );
    assert_eq!(ended, Ended::exited(3));
    assert!(lines.contains(&(Stream::Stderr, "left".to_string())));
    let last = lines.iter().rfind(|(s, _)| *s == Stream::Stdout).unwrap();
    assert_eq!(last.1.len(), 200_000);
    assert!(last.1.bytes().all(|b| b == b'a'));
}

#[test]
fn a_signal_is_said_as_one() {
    let (_, ended) = run(&["/bin/sh", "-c", "kill -KILL $$"]);
    assert_eq!(
        ended,
        Ended {
            code: None,
            signal: Some(9)
        }
    );
}

#[test]
fn a_program_is_never_looked_up_in_path() {
    let e = RealHost
        .run(&["env".to_string()], &mut |_, _| {})
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    assert!(e.to_string().contains("never searches PATH"), "{e}");
    assert!(command(&[]).is_err());
}

/// A descriptor the helper inherited without close-on-exec (pkexec hands on
/// what lmgw had open) does not reach the program; a plain spawn would pass
/// it on.
#[test]
fn a_program_inherits_no_descriptor_beyond_the_standard_three() {
    // SAFETY: dup of stdin gives a new descriptor without close-on-exec;
    // it is closed again below.
    let fd = unsafe { libc::dup(0) };
    assert!(fd > 2);
    let probe = format!("if [ -e /proc/self/fd/{fd} ]; then echo open; else echo closed; fi");
    let plain = std::process::Command::new("/bin/sh")
        .args(["-c", &probe])
        .output()
        .unwrap();
    let (lines, _) = run(&["/bin/sh", "-c", &probe]);
    // SAFETY: the descriptor dup returned above, closed once.
    unsafe {
        libc::close(fd);
    }
    assert_eq!(String::from_utf8_lossy(&plain.stdout).trim(), "open");
    assert_eq!(lines, [(Stream::Stdout, "closed".to_string())]);
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".lmgw-"))
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn a_new_file_gets_its_content_and_mode_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lmgw-nvidia-container-toolkit.repo");
    create_new(&path, b"# Written by lmgw.\n", 0o644).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"# Written by lmgw.\n");
    assert_eq!(mode(&path), 0o644);
    assert!(leftovers(dir.path()).is_empty());

    // Something there already, lmgw's or not: left exactly as it is.
    let e = create_new(&path, b"other", 0o644).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(e.to_string().contains("never overwrites"), "{e}");
    assert_eq!(fs::read(&path).unwrap(), b"# Written by lmgw.\n");
    assert!(leftovers(dir.path()).is_empty());

    // A symbolic link at the path is not followed.
    let target = dir.path().join("target");
    let link = dir.path().join("link.repo");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(create_new(&link, b"x", 0o644).is_err());
    assert!(!target.exists());
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn lmgws_own_file_is_replaced_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("RPM-GPG-KEY-lmgw-nvidia-container-toolkit");
    fs::write(&path, b"an older key").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    replace_own(&path, b"the current key", 0o644).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"the current key");
    assert_eq!(mode(&path), 0o644);
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_file_in_a_missing_directory_fails_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing/file.repo");
    assert!(create_new(&path, b"x", 0o644).is_err());
    assert!(replace_own(&path, b"x", 0o644).is_err());
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn journal_entries_use_the_native_protocol() {
    assert_eq!(
        journal_datagram(&[("PRIORITY", "6"), ("MESSAGE", "one line")]),
        b"PRIORITY=6\nMESSAGE=one line\n"
    );
    // A value with a newline: the key, a newline, its length (64-bit
    // little-endian), the bytes, a newline.
    let mut want = b"MESSAGE\n".to_vec();
    want.extend_from_slice(&5u64.to_le_bytes());
    want.extend_from_slice(b"a\nb c\n");
    assert_eq!(journal_datagram(&[("MESSAGE", "a\nb c")]), want);
}
