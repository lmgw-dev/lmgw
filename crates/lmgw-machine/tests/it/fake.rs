//! The fake machine itself: links resolve as the kernel resolves them, and
//! programs record their runs.

use std::path::{Path, PathBuf};

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::{CmdOut, Facts, Kind};

#[test]
fn relative_and_absolute_links_resolve_through_directories() {
    let f = FakeFacts::new();
    f.file("/usr/lib64/libx.so.1.2", b"data")
        .link("/usr/lib64/libx.so.1", "libx.so.1.2")
        .link("/lib64", "usr/lib64")
        .link("/opt/abs", "/usr/lib64/libx.so.1");
    assert_eq!(
        f.canonicalize(Path::new("/lib64/libx.so.1")).unwrap(),
        PathBuf::from("/usr/lib64/libx.so.1.2")
    );
    assert_eq!(f.read(Path::new("/opt/abs")).unwrap(), b"data");
    assert_eq!(f.lstat(Path::new("/opt/abs")).unwrap().kind, Kind::Symlink);
    assert_eq!(f.stat(Path::new("/opt/abs")).unwrap().kind, Kind::File);
    assert_eq!(
        f.read_link(Path::new("/usr/lib64/libx.so.1")).unwrap(),
        PathBuf::from("libx.so.1.2")
    );
}

#[test]
fn a_link_loop_ends_in_an_error_not_a_hang() {
    let f = FakeFacts::new();
    f.link("/a", "/b").link("/b", "/a");
    assert!(f.stat(Path::new("/a")).is_err());
}

#[test]
fn listings_are_sorted_and_only_direct_children() {
    let f = FakeFacts::new();
    f.file("/d/b", b"").file("/d/a", b"").file("/d/sub/c", b"");
    assert_eq!(f.list(Path::new("/d")).unwrap(), vec!["a", "b", "sub"]);
    assert!(f.list(Path::new("/nope")).is_err());
}

#[test]
fn programs_answer_and_are_recorded_and_unknown_ones_are_not_found() {
    let f = FakeFacts::new();
    f.reply("/usr/bin/tool", CmdOut::success("hi\n"));
    let out = f.run(Path::new("/usr/bin/tool"), &["--x", "y"]).unwrap();
    assert_eq!(out.stdout, "hi\n");
    assert!(
        f.stat(Path::new("/usr/bin/tool")).is_ok(),
        "installed as a file"
    );
    assert_eq!(f.calls_of("/usr/bin/tool"), vec![vec!["--x", "y"]]);
    let missing = f.run(Path::new("/usr/bin/other"), &[]).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    f.uninstall("/usr/bin/tool");
    assert!(f.run(Path::new("/usr/bin/tool"), &[]).is_err());
}

/// Root's environment's view is the same machine (its files, its record of
/// calls), whose programs answer as stated, not as the user's environment
/// moves them, and whose environment is root's fixed one.
#[test]
fn roots_environment_sees_the_stated_machine() {
    let f = FakeFacts::new();
    f.reply("/usr/bin/rpm", CmdOut::success("as stated\n"))
        .program_in_users_env("/usr/bin/rpm", |_| Ok(CmdOut::success("moved\n")))
        .set_env("HOME", "/home/someone");
    let root = f.root_env().expect("a fake has root's view");
    assert!(root.root_env().is_none());
    let rpm = Path::new("/usr/bin/rpm");
    assert_eq!(f.run(rpm, &["-q"]).unwrap().stdout, "moved\n");
    assert_eq!(root.run(rpm, &["-q"]).unwrap().stdout, "as stated\n");
    assert_eq!(f.calls_of("/usr/bin/rpm").len(), 2, "one record");
    f.file("/etc/later", "x");
    assert!(root.stat(Path::new("/etc/later")).is_ok(), "one machine");
    assert_eq!(f.env("HOME").as_deref(), Some("/home/someone"));
    assert_eq!(root.env("HOME").as_deref(), Some("/root"));
}

#[test]
fn a_limited_run_answers_in_time_or_times_out() {
    let f = FakeFacts::new();
    f.reply("/usr/bin/quick", CmdOut::success("ok\n"));
    f.program("/usr/bin/hang", |_| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        Ok(CmdOut::success(""))
    });
    let limit = std::time::Duration::from_millis(50);
    assert_eq!(
        f.run_limited(Path::new("/usr/bin/quick"), &[], limit)
            .unwrap()
            .stdout,
        "ok\n"
    );
    let started = std::time::Instant::now();
    let e = f
        .run_limited(Path::new("/usr/bin/hang"), &["x"], limit)
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(f.calls_of("/usr/bin/hang"), vec![vec!["x"]]);
}
