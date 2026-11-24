use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A fake tool: an executable shell script running `script`.
fn fake(dir: &Path, name: &str, script: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_of(d: &Path) -> OsString {
    std::env::join_paths([d]).unwrap()
}

#[tokio::test]
async fn an_empty_path_finds_nothing() {
    let t = DocTools::on_path(OsString::new());
    let f = t.check().await;
    assert_eq!(f.tools.len(), TOOLS.len());
    assert!(f.tools.values().all(|t| t.fact == Fact::Missing));
}

#[tokio::test]
async fn fakes_are_found_with_their_versions_and_extras() {
    let d = tempfile::tempdir().unwrap();
    fake(
        d.path(),
        "pdftotext",
        "echo 'pdftotext version 0.0.0-example' >&2",
    );
    fake(d.path(), "pdftoppm", "exit 0");
    fake(
        d.path(),
        "tesseract",
        "case \"$1\" in --version) echo 'tesseract 0.0.1';; \
         --list-langs) printf 'List of available languages in \"/x\" (2):\\neng\\nosd\\n';; esac",
    );
    let t = DocTools::on_path(path_of(d.path()));
    let f = t.check().await;
    let Some(Fact::Found(p)) = f.fact(ToolId::Poppler) else {
        panic!("{:?}", f.fact(ToolId::Poppler));
    };
    assert_eq!(p.version, "0.0.0-example");
    assert_eq!(p.extras[0].present, vec!["pdftotext", "pdftoppm"]);
    assert_eq!(p.extras[0].absent, vec!["pdftohtml"]);
    let Some(Fact::Found(ts)) = f.fact(ToolId::Tesseract) else {
        panic!("{:?}", f.fact(ToolId::Tesseract));
    };
    assert_eq!(ts.version, "0.0.1");
    assert_eq!(ts.extras[0].present, vec!["eng"]);
    assert!(ts.extras[0].absent.is_empty(), "osd is installed");
    assert_eq!(f.fact(ToolId::Pandoc), Some(&Fact::Missing));
}

#[tokio::test]
async fn a_tool_that_prints_no_version_fails_its_probe_with_its_stderr() {
    let d = tempfile::tempdir().unwrap();
    fake(
        d.path(),
        "pandoc",
        "echo 'error while loading shared libraries' >&2; exit 127",
    );
    let f = DocTools::on_path(path_of(d.path())).check().await;
    let Some(Fact::ProbeFailed { message, .. }) = f.fact(ToolId::Pandoc) else {
        panic!("{:?}", f.fact(ToolId::Pandoc));
    };
    assert!(message.contains("shared libraries"), "{message}");
    assert!(message.contains("exit status: 127"), "{message}");
}

/// A hung version run is stopped at its limit, and the message names
/// the constant (§4.3). The limit is shortened here; the run is the
/// production one.
#[tokio::test]
async fn a_hung_probe_is_stopped_and_the_limit_named() {
    let d = tempfile::tempdir().unwrap();
    fake(d.path(), "pandoc", "exec sleep 30");
    let tool = super::super::caps::tool(ToolId::Pandoc);
    let started = Instant::now();
    let fact = probe::probe_within(
        tool,
        &path_of(d.path()),
        std::time::Duration::from_millis(300),
    )
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let Fact::ProbeFailed { message, .. } = fact else {
        panic!("{fact:?}");
    };
    assert!(
        message.contains("did not answer within 300 ms (TOOL_PROBE_TIMEOUT)"),
        "{message}"
    );
}

/// Installed after the check: the next use probes it, without a check.
/// Removed again: it is missing on the next use.
#[tokio::test]
async fn an_install_or_a_removal_shows_on_the_next_use() {
    let d = tempfile::tempdir().unwrap();
    let t = DocTools::on_path(path_of(d.path()));
    assert_eq!(t.check().await.fact(ToolId::Pandoc), Some(&Fact::Missing));

    fake(d.path(), "pandoc", "echo 'pandoc 0.0.0-example'");
    let f = t.current().await;
    let Some(Fact::Found(p)) = f.fact(ToolId::Pandoc) else {
        panic!("{:?}", f.fact(ToolId::Pandoc));
    };
    assert_eq!(p.version, "0.0.0-example");

    std::fs::remove_file(d.path().join("pandoc")).unwrap();
    assert_eq!(t.current().await.fact(ToolId::Pandoc), Some(&Fact::Missing));
}

/// Replaced by an update (a new inode): probed again, the new version
/// read.
#[tokio::test]
async fn an_update_is_probed_again() {
    let d = tempfile::tempdir().unwrap();
    fake(d.path(), "pandoc", "echo 'pandoc 0.0.1'");
    let t = DocTools::on_path(path_of(d.path()));
    t.check().await;
    let tmp = d.path().join("pandoc.new");
    std::fs::write(&tmp, "#!/bin/sh\necho 'pandoc 0.0.2'\n").unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, d.path().join("pandoc")).unwrap();
    let f = t.current().await;
    let Some(Fact::Found(p)) = f.fact(ToolId::Pandoc) else {
        panic!("{:?}", f.fact(ToolId::Pandoc));
    };
    assert_eq!(p.version, "0.0.2");
}

/// Whether process `pid` is gone (a zombie waiting for its reaper counts).
fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')),
    }
}

/// `soffice` is a script that starts `soffice.bin` behind it: killing the
/// direct child alone would leave the grandchild running. The probe stops
/// the whole group.
#[tokio::test]
async fn a_hung_probe_takes_the_processes_it_started_with_it() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let pidfile = out.path().join("grandchild.pid");
    fake(
        d.path(),
        "pandoc",
        &format!("sleep 300 &\necho $! > {}\nwait", pidfile.display()),
    );
    let tool = super::super::caps::tool(ToolId::Pandoc);
    let fact = probe::probe_within(
        tool,
        &path_of(d.path()),
        std::time::Duration::from_millis(500),
    )
    .await;
    assert!(matches!(fact, Fact::ProbeFailed { .. }), "{fact:?}");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SIGKILL is delivered at once; the reaper of an orphan may take a moment.
    for _ in 0..100 {
        if gone(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the grandchild {pid} is still running after the probe timed out");
}

/// A tool whose probe counts its version runs and takes a moment. pandoc's
/// probe also runs its sandbox check (WP7), which is not counted: a probe is
/// one version run.
fn counting_pandoc(d: &Path, count: &Path) {
    fake(
        d,
        "pandoc",
        &format!(
            "if [ \"$1\" = --version ]; then echo run >> {}; sleep 0.4; fi\n\
             echo 'pandoc 0.0.0-example'",
            count.display()
        ),
    );
}

fn runs(count: &Path) -> usize {
    std::fs::read_to_string(count).map_or(0, |s| s.lines().count())
}

/// Two calls made while another check is running both come after its start;
/// the second joins the first's check instead of probing again.
#[tokio::test]
async fn a_check_asked_before_another_began_is_joined() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let count = out.path().join("count");
    counting_pandoc(d.path(), &count);
    let t = Arc::new(DocTools::on_path(path_of(d.path())));

    let first = tokio::spawn({
        let t = t.clone();
        async move { t.check().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // Asked while `first` runs, so neither joins it: one runs, one joins that.
    let (a, b) = tokio::join!(t.check(), t.check());
    first.await.unwrap();
    assert_eq!(runs(&count), 2, "the first check and one for the two");
    assert!(
        Arc::ptr_eq(&a, &b),
        "the second call got the first's answer"
    );
}

/// A call made after a check began runs its own, since that check may have
/// read the tools before what the caller changed.
#[tokio::test]
async fn a_call_made_after_a_check_began_runs_its_own() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let count = out.path().join("count");
    counting_pandoc(d.path(), &count);
    let t = Arc::new(DocTools::on_path(path_of(d.path())));
    let first = tokio::spawn({
        let t = t.clone();
        async move { t.check().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    t.check().await;
    first.await.unwrap();
    assert_eq!(runs(&count), 2);
}

/// The boot check holds the state; a use that finds no snapshot waits for it
/// instead of running a second whole check.
#[tokio::test]
async fn a_use_during_the_boot_check_does_not_check_again() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let count = out.path().join("count");
    counting_pandoc(d.path(), &count);
    let t = Arc::new(DocTools::on_path(path_of(d.path())));
    let boot = tokio::spawn({
        let t = t.clone();
        async move { t.check().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let facts = t.current().await;
    boot.await.unwrap();
    assert!(matches!(facts.fact(ToolId::Pandoc), Some(Fact::Found(_))));
    assert_eq!(runs(&count), 1);
}

/// A check dropped midway leaves the snapshot as it was.
#[tokio::test]
async fn a_dropped_check_leaves_the_snapshot_alone() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let count = out.path().join("count");
    counting_pandoc(d.path(), &count);
    let t = DocTools::on_path(path_of(d.path()));
    let first = t.check().await;
    let dropped = tokio::time::timeout(std::time::Duration::from_millis(100), t.check()).await;
    assert!(dropped.is_err(), "the second check was still running");
    let after = t.snapshot().unwrap();
    assert!(Arc::ptr_eq(&first, &after));
    // The `began` that goes with it is the first's: a caller who asked in
    // between does not join a check that never finished.
    t.check().await;
    assert_eq!(runs(&count), 3);
}

const LIBREOFFICE_FAKE: &str = "case \"$1\" in --version) \
    echo 'LibreOffice 0.0.1-example'; pwd > \"$OUT/cwd\"; printf '%s\\n' \"$@\" > \"$OUT/args\";; esac";

fn fake_soffice(d: &Path, out: &Path, tail: &str) {
    fake(
        d,
        "soffice",
        &format!("OUT={}\n{LIBREOFFICE_FAKE}\n{tail}", out.display()),
    );
}

#[tokio::test]
async fn the_libreoffice_probe_uses_a_private_profile_and_removes_it() {
    // LibreOffice is found only where it can be confined (`confine.rs`).
    if super::super::confine::test_helper("the_libreoffice_probe_uses_a_private_profile")
        .await
        .is_none()
    {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    fake_soffice(d.path(), out.path(), "");
    fake(d.path(), "swriter", "exit 0");
    fake(d.path(), "simpress", "exit 0");
    let f = DocTools::on_path_with_own(path_of(d.path())).check().await;
    let Some(Fact::Found(lo)) = f.fact(ToolId::LibreOffice) else {
        panic!("{:?}", f.fact(ToolId::LibreOffice));
    };
    assert_eq!(lo.version, "0.0.1-example");
    let parts = lo
        .extras
        .iter()
        .find(|e| e.kind == lmgw_api_types::documents::ExtraKind::Components)
        .unwrap();
    assert_eq!(parts.present, vec!["Writer", "Impress"]);
    assert_eq!(parts.absent, vec!["Calc"]);

    let cwd = std::fs::read_to_string(out.path().join("cwd")).unwrap();
    let cwd = cwd.trim();
    let args = std::fs::read_to_string(out.path().join("args")).unwrap();
    assert!(
        args.lines()
            .any(|a| a == format!("-env:UserInstallation=file://{cwd}/profile")),
        "{args}"
    );
    assert!(
        Path::new(cwd)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("lmgw-probe-"),
        "{cwd}"
    );
    assert!(!Path::new(cwd).exists(), "the private dir was left behind");
}

#[tokio::test]
async fn the_private_profile_is_removed_after_a_timeout_too() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    fake_soffice(d.path(), out.path(), "");
    // Prints its cwd, then hangs on every other call.
    fake(
        d.path(),
        "soffice",
        &format!("pwd > {}/cwd\nsleep 30", out.path().display()),
    );
    let tool = super::super::caps::tool(ToolId::LibreOffice);
    let fact = probe::probe_within(
        tool,
        &path_of(d.path()),
        std::time::Duration::from_millis(500),
    )
    .await;
    assert!(matches!(fact, Fact::ProbeFailed { .. }), "{fact:?}");
    let cwd = std::fs::read_to_string(out.path().join("cwd")).unwrap();
    assert!(
        !Path::new(cwd.trim()).exists(),
        "the private dir was left behind"
    );
}

#[tokio::test]
async fn ffmpeg_reads_its_version_and_its_audio_decoders() {
    let d = tempfile::tempdir().unwrap();
    fake(
        d.path(),
        "ffmpeg",
        "case \"$*\" in\n\
         *-version*) echo 'ffmpeg version 0.0.0-example Copyright the FFmpeg developers';;\n\
         *-decoders*) printf ' A....D = Audio\\n ------\\n V....D h264 H.264\\n \
         A....D aac AAC\\n A....D flac FLAC\\n';;\n\
         esac",
    );
    let f = DocTools::on_path(path_of(d.path())).check().await;
    let Some(Fact::Found(ff)) = f.fact(ToolId::Ffmpeg) else {
        panic!("{:?}", f.fact(ToolId::Ffmpeg));
    };
    assert_eq!(ff.version, "0.0.0-example");
    assert_eq!(ff.extras[0].present, vec!["aac", "flac"]);
    assert_eq!(
        ff.extras[0].absent,
        vec!["mp3", "opus", "vorbis", "ac3", "eac3"]
    );
}

#[tokio::test]
async fn the_first_directory_on_the_path_wins() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    fake(first.path(), "pandoc", "echo 'pandoc 1.0.0-example'");
    fake(second.path(), "pandoc", "echo 'pandoc 2.0.0-example'");
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();
    let f = DocTools::on_path(path).check().await;
    let Some(Fact::Found(p)) = f.fact(ToolId::Pandoc) else {
        panic!("{:?}", f.fact(ToolId::Pandoc));
    };
    assert_eq!(p.version, "1.0.0-example");
    assert_eq!(p.located.path, first.path().join("pandoc"));
}

fn components_of(facts: &ToolFacts) -> Vec<String> {
    let Some(Fact::Found(lo)) = facts.fact(ToolId::LibreOffice) else {
        panic!("{:?}", facts.fact(ToolId::LibreOffice));
    };
    lo.extras
        .iter()
        .find(|e| e.kind == lmgw_api_types::documents::ExtraKind::Components)
        .unwrap()
        .present
        .clone()
}

/// A LibreOffice part installed or removed after the probe: the binary is
/// the same, but the next use probes again.
#[tokio::test]
async fn a_libreoffice_part_that_appears_or_goes_is_seen() {
    if super::super::confine::test_helper("a_libreoffice_part_that_appears_or_goes_is_seen")
        .await
        .is_none()
    {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    fake_soffice(d.path(), out.path(), "");
    let t = DocTools::on_path_with_own(path_of(d.path()));
    assert!(components_of(&*t.check().await).is_empty());
    fake(d.path(), "scalc", "exit 0");
    assert_eq!(components_of(&*t.current().await), vec!["Calc"]);
    std::fs::remove_file(d.path().join("scalc")).unwrap();
    assert!(components_of(&*t.current().await).is_empty());
}

fn languages_of(facts: &ToolFacts) -> Vec<String> {
    let Some(Fact::Found(ts)) = facts.fact(ToolId::Tesseract) else {
        panic!("{:?}", facts.fact(ToolId::Tesseract));
    };
    ts.extras[0].present.clone()
}

/// A language pack added to tessdata after the probe: its directory's mtime
/// moved, and the next use reads the list again.
#[tokio::test]
async fn a_new_language_pack_is_seen() {
    let d = tempfile::tempdir().unwrap();
    let tessdata = d.path().join("tessdata");
    std::fs::create_dir(&tessdata).unwrap();
    std::fs::write(tessdata.join("eng.traineddata"), "").unwrap();
    let dir = tessdata.display();
    fake(
        d.path(),
        "tesseract",
        &format!(
            "case \"$1\" in --version) echo 'tesseract 0.0.1';; \
             --list-langs) printf 'List of available languages in \"{dir}/\" (1):\\n'; \
             ls {dir} | sed 's/.traineddata//';; esac"
        ),
    );
    let t = DocTools::on_path(path_of(d.path()));
    assert_eq!(languages_of(&*t.check().await), vec!["eng"]);
    std::fs::write(tessdata.join("deu.traineddata"), "").unwrap();
    assert_eq!(languages_of(&*t.current().await), vec!["deu", "eng"]);
}

/// poppler's other binaries are part of its answer too.
#[tokio::test]
async fn a_poppler_binary_that_appears_is_seen() {
    let d = tempfile::tempdir().unwrap();
    fake(
        d.path(),
        "pdftotext",
        "echo 'pdftotext version 0.0.0-example' >&2",
    );
    let t = DocTools::on_path(path_of(d.path()));
    let present = |f: &ToolFacts| match f.fact(ToolId::Poppler) {
        Some(Fact::Found(p)) => p.extras[0].present.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(present(&*t.check().await), vec!["pdftotext"]);
    fake(d.path(), "pdftoppm", "exit 0");
    assert_eq!(present(&*t.current().await), vec!["pdftotext", "pdftoppm"]);
}

/// LibreOffice found where lmgw's reader is not: not used, its probe
/// failing by name (it runs only confined, `confine.rs`).
#[tokio::test]
async fn a_libreoffice_that_cannot_be_confined_is_not_used() {
    let d = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    fake_soffice(d.path(), out.path(), "");
    let f = DocTools::on_path(path_of(d.path())).check().await;
    let Some(Fact::ProbeFailed { message, .. }) = f.fact(ToolId::LibreOffice) else {
        panic!("{:?}", f.fact(ToolId::LibreOffice));
    };
    assert!(
        message.starts_with(super::super::confine::NEEDS_LANDLOCK),
        "{message}"
    );
    assert!(message.contains("lmgw-doc-reader"), "{message}");
    // A file it would read says why, not how to install it.
    let said = super::super::report::needs_said(
        &[ToolId::LibreOffice],
        &f,
        lmgw_api_types::documents::InstallPlatform::Fedora,
    );
    assert!(
        said.starts_with("not read: LibreOffice runs only confined"),
        "{said}"
    );
}
