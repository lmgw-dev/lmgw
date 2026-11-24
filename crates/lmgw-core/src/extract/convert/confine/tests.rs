//! The confinement on trivial tools (bash, cat, head) through the real
//! reader executable, which `cargo test --workspace` builds beside the
//! tests. Each test is skipped by name where the reader or Landlock is
//! missing, as the suite's other tool-dependent tests are.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;

use super::*;
use crate::extract::convert::pool;
use crate::extract::convert::run::{self, Bounds, Confined, Input, RunError, Tool};

async fn helper(test: &str) -> Option<PathBuf> {
    test_helper(test).await
}

fn bounds() -> Bounds {
    Bounds {
        timeout: Duration::from_secs(30),
        output: 1 << 20,
        memory: 4 << 30,
    }
}

const BASH: &str = "/usr/bin/bash";

/// `bash -c script` confined by `rules`.
async fn bash(helper: &Path, rules: &Confine, script: &str) -> Result<String, RunError> {
    let slot = pool::global().acquire().await;
    let args = ["-c", script];
    let tool = Tool {
        confine: Some(Confined { helper, rules }),
        ..Tool::new(Path::new(BASH), &args)
    };
    run::run_held(&slot, tool, Input::Bytes(Bytes::new()), bounds())
        .await
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

#[tokio::test]
async fn a_confined_tool_reads_its_own_directory_and_nothing_outside() {
    let Some(helper) = helper("a_confined_tool_reads_its_own_directory").await else {
        return;
    };
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "not for the tool").unwrap();
    let run_dir = tempfile::tempdir().unwrap();
    let inside = run_dir.path().join("in.txt");
    std::fs::write(&inside, "the tool's own").unwrap();
    let rules = filter(Some(run_dir.path()), 1 << 20);
    let own = bash(&helper, &rules, &format!("cat '{}'", inside.display())).await;
    assert_eq!(own.unwrap(), "the tool's own");
    let theirs = bash(&helper, &rules, &format!("cat '{}'", secret.display())).await;
    let Err(RunError::Failed(m)) = theirs else {
        panic!("the outside file was read: {theirs:?}");
    };
    assert!(m.contains("Permission denied"), "{m}");
    // Writing beside the secret fails too; inside it works.
    let w = bash(
        &helper,
        &rules,
        &format!("echo x > '{}/new'", outside.path().display()),
    )
    .await;
    assert!(w.is_err(), "{w:?}");
    assert!(!outside.path().join("new").exists());
    let w = bash(
        &helper,
        &rules,
        &format!("echo x > '{}/new'", run_dir.path().display()),
    )
    .await;
    assert!(w.is_ok(), "{w:?}");
    // What it wrote it cannot execute.
    let x = bash(
        &helper,
        &rules,
        &format!(
            "printf '#!/usr/bin/bash\\necho ran' > '{0}/x' && chmod +x '{0}/x' && '{0}/x'",
            run_dir.path().display()
        ),
    )
    .await;
    assert!(x.is_err(), "{x:?}");
}

/// A descriptor the gateway holds without close-on-exec does not reach
/// the tool: the reader closes everything above stdio before the exec.
#[tokio::test]
async fn a_confined_tool_inherits_no_descriptor() {
    let Some(helper) = helper("a_confined_tool_inherits_no_descriptor").await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("held.txt");
    std::fs::write(&file, "held by the gateway").unwrap();
    let held = std::fs::File::open(&file).unwrap();
    // `dup` leaves the new descriptor without close-on-exec.
    let fd = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&held)) };
    assert!(fd > 2);
    let rules = filter(None, 0);
    let got = bash(&helper, &rules, &format!("cat <&{fd}")).await;
    unsafe { libc::close(fd) };
    let Err(RunError::Failed(m)) = got else {
        panic!("the descriptor was inherited: {got:?}");
    };
    assert!(m.contains("Bad file descriptor"), "{m}");
}

/// TCP to a loopback listener is refused from Landlock ABI 4.
#[tokio::test]
async fn a_confined_tool_cannot_connect_over_tcp() {
    let Some(helper) = helper("a_confined_tool_cannot_connect_over_tcp").await else {
        return;
    };
    if available(&helper).await.unwrap_or(0) < 4 {
        eprintln!("a_confined_tool_cannot_connect_over_tcp: skipped, Landlock ABI below 4");
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = format!("echo hello > /dev/tcp/127.0.0.1/{port}");
    // Unconfined, the same script connects (the test means something).
    let mut open = std::process::Command::new(BASH)
        .args(["-c", &script])
        .spawn()
        .unwrap();
    listener.set_nonblocking(false).unwrap();
    let (_conn, _) = listener.accept().unwrap();
    assert!(open.wait().unwrap().success());
    let rules = filter(None, 0);
    let got = bash(&helper, &rules, &script).await;
    let Err(RunError::Failed(m)) = got else {
        panic!("the confined tool connected: {got:?}");
    };
    assert!(m.contains("Permission denied"), "{m}");
}

/// A file written past the file-size limit ends the tool by name.
#[tokio::test]
async fn a_file_past_the_file_size_limit_ends_the_run() {
    let Some(helper) = helper("a_file_past_the_file_size_limit_ends_the_run").await else {
        return;
    };
    let run_dir = tempfile::tempdir().unwrap();
    let rules = filter(Some(run_dir.path()), 4096);
    // bash writes the file itself, so the signal is the run's own.
    let script = format!(
        "printf '%*s' 100000 '' > '{}/big'",
        run_dir.path().display()
    );
    let got = bash(&helper, &rules, &script).await;
    assert_eq!(got.unwrap_err(), RunError::FileTooLarge(4096));
    // Below the limit it is written.
    let script = format!(
        "printf '%*s' 4000 '' > '{}/small'",
        run_dir.path().display()
    );
    assert!(bash(&helper, &rules, &script).await.is_ok());
}

/// A missing program is the runner's `Missing`, as for an unconfined tool.
#[tokio::test]
async fn a_missing_program_through_the_reader_is_missing() {
    let Some(helper) = helper("a_missing_program_through_the_reader_is_missing").await else {
        return;
    };
    let slot = pool::global().acquire().await;
    let rules = filter(None, 0);
    let tool = Tool {
        confine: Some(Confined {
            helper: &helper,
            rules: &rules,
        }),
        ..Tool::new(Path::new("/usr/bin/a-tool-that-is-not-installed"), &[])
    };
    let got = run::run_held(&slot, tool, Input::Bytes(Bytes::new()), bounds()).await;
    assert_eq!(got.unwrap_err(), RunError::Missing);
}

/// The process group kill reaches what the confined tool started: the
/// reader `exec`s the tool, so the group the runner made is the tool's.
#[tokio::test]
async fn a_stopped_confined_run_takes_what_it_started_with_it() {
    let Some(helper) = helper("a_stopped_confined_run_takes_what_it_started_with_it").await else {
        return;
    };
    let run_dir = tempfile::tempdir().unwrap();
    let pid_file = run_dir.path().join("child.pid");
    let rules = filter(Some(run_dir.path()), 1 << 20);
    let script = format!(
        "/usr/bin/sleep 30 & echo $! > '{}'; wait",
        pid_file.display()
    );
    let slot = pool::global().acquire().await;
    let args = ["-c", script.as_str()];
    let tool = Tool {
        confine: Some(Confined {
            helper: &helper,
            rules: &rules,
        }),
        ..Tool::new(Path::new(BASH), &args)
    };
    let short = Bounds {
        timeout: Duration::from_millis(500),
        ..bounds()
    };
    let got = run::run_held(&slot, tool, Input::Bytes(Bytes::new()), short).await;
    assert_eq!(got.unwrap_err(), RunError::Timeout);
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The kill was sent to the group before the run returned.
    let gone = (0..50).any(|_| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        let ended = stat.map_or(true, |s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
        });
        if !ended {
            std::thread::sleep(Duration::from_millis(20));
        }
        ended
    });
    assert!(gone, "the confined tool's child {pid} still runs");
}

#[test]
fn libreoffices_rules_name_no_home_no_data_dir_and_only_a_socket_in_tmp() {
    let run_dir = Path::new("/srv/lmgw-data.example/convert-spool/lo-1-abc");
    let r = libreoffice(Path::new("/usr/bin/soffice"), run_dir, 1 << 30).unwrap();
    assert_eq!(
        r.read_write,
        vec![run_dir.to_path_buf(), PathBuf::from("/dev/null")]
    );
    assert_eq!(r.make_sock, vec![PathBuf::from("/tmp")]);
    assert!(r.deny_net);
    // Hardening 8: `/usr` is read, not executed; what runs is named.
    assert!(r.read.contains(&PathBuf::from("/usr")));
    assert!(!r.read_exec.contains(&PathBuf::from("/usr")));
    assert!(r.read_exec.contains(&PathBuf::from("/usr/bin/sh")));
    assert!(!r.read_exec.iter().any(|p| p.starts_with("/usr/lib/jvm")));
    for p in r.read_exec.iter().chain(&r.read) {
        assert!(
            !p.starts_with("/home") && !p.starts_with("/tmp") && !p.starts_with("/run"),
            "{}",
            p.display()
        );
        assert!(
            !run_dir.starts_with(p),
            "{} holds the data dir",
            p.display()
        );
    }
}

/// Security round B, M3: LibreOffice needs the network denied, which a
/// kernel below Landlock ABI 4 cannot do; its refusal names the ABI found.
#[test]
fn libreoffice_is_refused_below_the_abi_that_denies_tcp() {
    for abi in 1..LIBREOFFICE_MIN_ABI {
        let why = libreoffice_floor(abi).unwrap_err();
        assert!(why.contains(&format!("Landlock ABI is {abi}")), "{why}");
        assert!(why.contains("LIBREOFFICE_MIN_ABI, 4"), "{why}");
    }
    for abi in LIBREOFFICE_MIN_ABI..=11 {
        assert_eq!(libreoffice_floor(abi), Ok(()));
    }
    assert!(NEEDS_LANDLOCK.contains("ABI 4"));
}

/// Security round B, L3: the run directory's write right makes no symbolic
/// link and links nothing in from elsewhere, so the gateway never finds one
/// where a tool's output should be.
#[tokio::test]
async fn a_confined_tool_makes_no_link_in_its_run_directory() {
    let Some(helper) = helper("a_confined_tool_makes_no_link_in_its_run_directory").await else {
        return;
    };
    let run_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(run_dir.path().join("out")).unwrap();
    std::fs::write(run_dir.path().join("in.txt"), "x").unwrap();
    let rules = filter(Some(run_dir.path()), 1 << 20);
    let d = run_dir.path().display();
    let sym = bash(
        &helper,
        &rules,
        &format!("ln -s /etc/hostname '{d}/out/in.docx'"),
    )
    .await;
    assert!(sym.is_err(), "{sym:?}");
    assert!(std::fs::symlink_metadata(run_dir.path().join("out/in.docx")).is_err());
    // A hard link into another directory of the run (`refer`) neither.
    let linked = bash(
        &helper,
        &rules,
        &format!("ln '{d}/in.txt' '{d}/out/in.txt'"),
    )
    .await;
    assert!(linked.is_err(), "{linked:?}");
    // Writing and renaming within one directory still work.
    let own = bash(
        &helper,
        &rules,
        &format!("echo y > '{d}/out/a' && mv '{d}/out/a' '{d}/out/b' && cat '{d}/out/b'"),
    )
    .await;
    assert_eq!(own.unwrap().trim(), "y");
}
