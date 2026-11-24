//! What a confined tool's rules may reach beyond `/usr` (`reach.rs`, review
//! S1, S2) and the probe's confined check run (`verdict.rs`, S5).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::super::reach;
use super::*;
use crate::extract::convert::probe::Located;

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

/// An executable script `name` in `dir` with `body`.
fn stand_in(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn no_rule_holds_the_root_a_home_or_lmgw_s_own_directories() {
    assert!(reach::too_wide(Path::new("/")).is_some());
    assert!(reach::too_wide(Path::new("/home")).is_some());
    assert!(reach::too_wide(Path::new("/home/someone.example")).is_some());
    assert!(reach::too_wide(Path::new("/root")).is_some());
    if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_absolute()) {
        assert!(reach::too_wide(Path::new(&home)).is_some());
    }
    // A data dir the gateway runs with: it and every directory above it.
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("share/lmgw");
    std::fs::create_dir_all(&data).unwrap();
    reach::protect(&data);
    assert!(reach::too_wide(&t.path().join("share")).is_some());
    assert!(reach::too_wide(t.path()).is_some());
    assert!(reach::too_wide(&t.path().join("bin")).is_none());
    assert!(reach::too_wide(Path::new("/opt/example")).is_none());
}

#[test]
fn a_tool_outside_usr_reaches_its_own_directories_never_their_prefix() {
    let t = tempfile::tempdir().unwrap();
    let prefix = t.path().join("local");
    let bin = stand_in(&prefix.join("bin"), "pdftotext", "exit 0");
    for sub in ["lib", "share/lmgw"] {
        std::fs::create_dir_all(prefix.join(sub)).unwrap();
    }
    // `share` holds a data dir: as `~/.local/share` holds lmgw's.
    reach::protect(&prefix.join("share/lmgw"));
    let dirs = reach::install_dirs(&bin).unwrap();
    assert_eq!(
        dirs,
        vec![canon(&prefix.join("bin")), canon(&prefix.join("lib"))]
    );
    // A tool directly in a directory that holds the data dir: not confined.
    let loose = stand_in(&prefix, "pdftoppm", "exit 0");
    let why = reach::install_dirs(&loose).unwrap_err();
    assert!(why.contains("holds lmgw's data"), "{why}");
    let why = super::super::poppler_rules(&loose, None).unwrap_err();
    assert!(why.contains("holds lmgw's data"), "{why}");
    assert!(super::super::poppler(&loose, None).is_none());
    // The system's own: nothing beside `/usr`.
    assert!(reach::install_dirs(Path::new("/usr/bin/sh"))
        .unwrap()
        .is_empty());
}

#[test]
fn tesseract_s_language_data_is_read_only_where_it_is_language_data() {
    let t = tempfile::tempdir().unwrap();
    let empty = t.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let why = reach::tessdata(&empty).unwrap_err();
    assert!(why.contains("no *.traineddata"), "{why}");
    let data = t.path().join("tessdata");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("eng.traineddata"), b"example").unwrap();
    assert_eq!(reach::tessdata(&data).unwrap(), Some(canon(&data)));
    assert!(reach::tessdata(Path::new("/")).is_err());
    assert_eq!(
        reach::tessdata(Path::new("/usr/share/tesseract/tessdata")).unwrap(),
        None
    );
    let bin = Path::new("/usr/bin/sh");
    assert!(super::super::tesseract_rules(bin, Some(&empty)).is_err());
    let r = super::super::tesseract_rules(bin, Some(&data)).unwrap();
    assert!(r.read.contains(&canon(&data)), "{:?}", r.read);
}

fn found(bin: &Path, version: &str) -> Found {
    Found {
        located: Located {
            path: bin.to_path_buf(),
            stamp: crate::extract::convert::probe::stamp(bin).unwrap(),
        },
        version: version.to_string(),
        extras: Vec::new(),
        watched: Vec::new(),
    }
}

#[tokio::test]
async fn a_tool_that_fails_only_when_confined_is_found_at_its_probe() {
    let Some(helper) = super::super::test_helper("a_tool_that_fails_only_when_confined").await
    else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    // It reads a file no rule allows: fine unconfined, refused confined.
    let outside = t.path().join("elsewhere/example.conf");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"x").unwrap();
    let bin = stand_in(
        &t.path().join("tool/bin"),
        "pdftotext",
        &format!("cat '{}' > /dev/null || exit 3", outside.display()),
    );
    assert!(std::process::Command::new(&bin).status().unwrap().success());
    check(ToolId::Poppler, &found(&bin, "0.0.0-example"), &helper).await;
    let why = super::super::poppler_rules(&bin, None).unwrap_err();
    assert!(why.contains("its confined run failed"), "{why}");
    assert!(super::super::poppler(&bin, None).is_none());
    // One that runs confined stays confined.
    let fine = stand_in(
        &t.path().join("fine/bin"),
        "pdftotext",
        "echo 'pdftotext version 0.0'",
    );
    check(ToolId::Poppler, &found(&fine, "0.0.0-example"), &helper).await;
    assert!(super::super::poppler_rules(&fine, None).is_ok());
}

/// The review of the poppler limit, S3: a check run that runs out of its
/// memory limit says nothing of the confinement, which stays on.
#[tokio::test]
async fn a_check_run_out_of_memory_keeps_the_confinement() {
    let Some(helper) = super::super::test_helper("a_check_run_out_of_memory").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let bin = stand_in(
        &t.path().join("tool/bin"),
        "pdftotext",
        "echo 'Out of memory' >&2; exit 1",
    );
    let rules = super::super::poppler_rules(&bin, None).unwrap();
    let ran = run(&helper, &bin, &["-v"], 512 << 20, &rules).await;
    assert_eq!(ran, Err(Failed::Memory));
    check(ToolId::Poppler, &found(&bin, "0.0.0-example"), &helper).await;
    assert!(super::super::poppler_rules(&bin, None).is_ok());
    assert!(failure(ToolId::Poppler, &bin).is_none());
}

/// Verification of the performance fix round, SF3 and N-g: `/home`,
/// `/var/home` and what is above them never pass, whatever `HOME` says;
/// nor does anything inside lmgw's own directories. `HOME` is given
/// (`reach::Places`), never set for the test binary: a home outside
/// `/home`, and none at all (the review of round B, N6).
#[test]
fn homes_and_lmgw_s_own_insides_never_pass_whatever_home_says() {
    let t = tempfile::tempdir().unwrap();
    for home in [Some(t.path().join("svc-home")), None] {
        let places = reach::Places {
            home: home.clone(),
            own: Vec::new(),
        };
        for p in ["/home", "/var/home", "/var", "/home/someone.example"] {
            assert!(
                reach::too_wide_in(Path::new(p), &places).is_some(),
                "{p} with HOME {home:?}"
            );
        }
    }
    let data = t.path().join("data/lmgw");
    std::fs::create_dir_all(data.join("tools/bin")).unwrap();
    let places = reach::Places {
        home: None,
        own: vec![canon(&data)],
    };
    assert!(reach::too_wide_in(&data.join("tools/bin"), &places).is_some());
    let bin = stand_in(&data.join("tools/bin"), "pdftotext", "exit 0");
    assert!(reach::install_dirs_in(&bin, &places).is_err());
}

/// SF2: a `bin` directly in a home gets none of the home's own folders,
/// though the home has a `lib` and a `share` (N6: a home that has them, so
/// the test fails without the rule).
#[test]
fn a_bin_in_a_home_gets_no_siblings() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home-example");
    for sub in ["lib", "share", "bin"] {
        std::fs::create_dir_all(home.join(sub)).unwrap();
    }
    let places = reach::Places {
        home: Some(home.clone()),
        own: Vec::new(),
    };
    assert!(reach::siblings_in(&home, &places).is_empty());
    let bin = stand_in(&home.join("bin"), "pdftotext", "exit 0");
    assert_eq!(
        reach::install_dirs_in(&bin, &places).unwrap(),
        vec![canon(&home.join("bin"))]
    );
    assert!(reach::siblings(Path::new("/home/someone.example")).is_empty());
    assert!(reach::siblings(Path::new("/var/home/someone.example")).is_empty());
    assert!(reach::siblings(Path::new("/root")).is_empty());
    // The same prefix when it is no home: its existing siblings.
    let other = reach::Places::default();
    assert_eq!(
        reach::siblings_in(&home, &other),
        vec![home.join("lib"), home.join("share")]
    );
}

/// The review of round B, S5: LibreOffice's install directory passes the
/// same check; a `soffice` wrapper in `~/.local/bin` (whose prefix holds
/// lmgw's data) or in `~/bin` (the home) is refused by name.
#[test]
fn libreoffice_from_a_home_wrapper_is_refused_by_name() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home-example");
    let data = home.join(".local/share/lmgw");
    std::fs::create_dir_all(&data).unwrap();
    let places = reach::Places {
        home: Some(home.clone()),
        own: vec![canon(&data)],
    };
    let local = stand_in(&home.join(".local/bin"), "soffice", "exit 0");
    let why = reach::libreoffice_install(&local, &places).unwrap_err();
    assert!(
        why.starts_with("LibreOffice's soffice at") && why.contains("holds lmgw's data"),
        "{why}"
    );
    let own_bin = stand_in(&home.join("bin"), "soffice", "exit 0");
    let why = reach::libreoffice_install(&own_bin, &places).unwrap_err();
    assert!(why.contains("a home directory or holds one"), "{why}");
    // An install of its own (TDF's `/opt/libreoffice…` layout) passes.
    let opt = stand_in(
        &t.path().join("opt/libreoffice26.8/program"),
        "soffice",
        "exit 0",
    );
    assert_eq!(
        reach::libreoffice_install(&opt, &places).unwrap(),
        Some(canon(&t.path().join("opt/libreoffice26.8")))
    );
}

/// SF1: tools in one directory keep a verdict each, so one tool's failed
/// check neither turns another off nor is erased by another's success.
#[test]
fn each_tool_keeps_its_own_verdict_in_a_shared_directory() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("bin");
    let pdftotext = stand_in(&dir, "pdftotext", "exit 0");
    let pandoc = stand_in(&dir, "pandoc", "exit 0");
    let tesseract = stand_in(&dir, "tesseract", "exit 0");
    record(
        ToolId::Tesseract,
        &tesseract,
        Err("listed 3 of its 5 packs".into()),
    );
    record(ToolId::Pandoc, &pandoc, Err("example failure".into()));
    assert!(failure(ToolId::Poppler, &pdftotext).is_none());
    record(ToolId::Poppler, &pdftotext, Ok(()));
    assert_eq!(
        failure(ToolId::Tesseract, &tesseract).as_deref(),
        Some("listed 3 of its 5 packs")
    );
    assert_eq!(
        failure(ToolId::Pandoc, &pandoc).as_deref(),
        Some("example failure")
    );
    record(ToolId::Pandoc, &pandoc, Ok(()));
    assert!(failure(ToolId::Pandoc, &pandoc).is_none());
    assert!(failure(ToolId::Tesseract, &tesseract).is_some());
}
