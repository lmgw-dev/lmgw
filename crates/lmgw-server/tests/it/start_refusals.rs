//! The refusals that end a start before the gateway exists (headless server
//! design §1 steps 3–4, §2.2, D1, D28), on the built binary with a stated
//! environment: nothing here opens a database or reaches podman.
//!
//! A regression that let one of these starts through would be a real start
//! on the developer's machine, so every run carries a net ([`SAFETY`]): a
//! podman that cannot be reached, a loopback address the OS picks, and, for
//! a container-mode run, `LMGW_DEV`, which moves a fresh data dir off the
//! production prefix before anything lists a container.

use std::process::{Command, Output};

use lmgw_core::state::data_dir_lock::{DataDirLock, LOCK_FILE};

/// What every run's environment starts with: any podman call fails
/// (`CONTAINER_HOST` makes podman a remote client of a socket that does not
/// exist), and the listener is a loopback port of the OS's choosing.
const SAFETY: &[(&str, &str)] = &[
    ("CONTAINER_HOST", "unix:///nonexistent-lmgw-wp1"),
    ("LMGW_BIND_ADDR", "127.0.0.1:0"),
];

/// The binary with only [`SAFETY`] and `vars` in its environment (`vars`
/// win), and `LMGW_DEV=1` beside any container-mode run.
fn run(vars: &[(&str, &str)], args: &[&str]) -> Output {
    let container = vars
        .iter()
        .any(|(k, v)| *k == "LMGW_DEPLOYMENT" && *v == "container");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lmgw-server"));
    cmd.env_clear().envs(SAFETY.iter().copied());
    if container {
        cmd.env("LMGW_DEV", "1");
    }
    cmd.envs(vars.iter().copied())
        .args(args)
        .output()
        .expect("the binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn outside_a_container_and_without_lmgw_dev_it_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().display().to_string();
    let out = run(&[("LMGW_DATA_DIR", &data)], &[]);
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(
        said.contains("starts only in the server image (LMGW_DEPLOYMENT=container)"),
        "{said}"
    );
    assert!(
        !dir.path().join("lmgw.sqlite").exists() && !dir.path().join(LOCK_FILE).exists(),
        "the refusal comes before the data dir is touched"
    );
}

#[test]
fn a_variable_that_does_not_parse_ends_the_start_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().display().to_string();
    for (var, bad) in [
        ("LMGW_BIND_ADDR", "everywhere"),
        ("LMGW_DEPLOYMENT", "kubernetes"),
        ("LMGW_STOP_TIMEOUT", "-1"),
        ("LMGW_TRUSTED_PROXIES", "192.0.2.5/24"),
    ] {
        let mut vars = vec![("LMGW_DATA_DIR", data.as_str()), (var, bad)];
        if var != "LMGW_DEPLOYMENT" {
            vars.push(("LMGW_DEPLOYMENT", "container"));
        }
        let out = run(&vars, &[]);
        assert!(!out.status.success(), "{var}");
        let said = stderr(&out);
        assert!(
            said.contains(var) && said.contains(&format!("{bad:?}")),
            "{var}: {said}"
        );
    }
    assert!(!dir.path().join("lmgw.sqlite").exists());
}

#[test]
fn half_a_tls_pair_is_refused_before_anything_opens() {
    let out = run(
        &[
            ("LMGW_DEPLOYMENT", "container"),
            ("LMGW_DATA_DIR", "/nonexistent-lmgw-wp4"),
            ("LMGW_TLS_CERT", "/etc/lmgw/tls/tls.crt"),
        ],
        &[],
    );
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(
        said.contains("LMGW_TLS_CERT is set without LMGW_TLS_KEY"),
        "{said}"
    );
}

#[test]
fn without_a_data_dir_it_refuses() {
    let out = run(&[("LMGW_DEPLOYMENT", "container")], &[]);
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(said.contains("LMGW_DATA_DIR is not set"), "{said}");
}

#[test]
fn a_second_start_on_a_locked_data_dir_refuses_naming_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let held = DataDirLock::acquire(dir.path(), "lmgw-server").unwrap();
    let data = dir.path().display().to_string();
    let out = run(
        &[("LMGW_DEPLOYMENT", "container"), ("LMGW_DATA_DIR", &data)],
        &[],
    );
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(
        said.contains(&format!("lmgw-server (pid {} on ", std::process::id()))
            && said.contains(LOCK_FILE),
        "{said}"
    );
    assert!(
        !dir.path().join("lmgw.sqlite").exists(),
        "the refused start opened the database"
    );
    drop(held);
}

#[test]
fn version_and_help_answer_without_serving() {
    let out = run(&[], &["--version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("lmgw-server {}", env!("CARGO_PKG_VERSION"))
    );
    let out = run(&[], &["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: lmgw-server"));
    let out = run(&[], &["account", "nonsense"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown command line: account nonsense"));
}
