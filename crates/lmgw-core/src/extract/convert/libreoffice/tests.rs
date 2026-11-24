//! LibreOffice's runs (WP7): stand-ins for a LibreOffice that hangs, starts
//! children, writes beside its result or fails to open the file, and the
//! real one, where it is found, twice at once and against documents that
//! name files outside their run. Every run is confined by the reader
//! executable beside the tests; each test is skipped by name where it or
//! Landlock is missing.

use std::os::unix::fs::PermissionsExt;

use super::*;

mod linked;
use linked::{answered_without, links};

/// A shell script `soffice` in `dir`; the profile template's run (a run
/// with `--terminate_after_init`) ends at once. LibreOffice's rules execute
/// only its install and its start script's helpers (security round B,
/// hardening 8), so a program the script runs beyond those is a copy in
/// `dir`'s `bin` ([`own`]), inside the install it stands in for.
/// A copy of the system's `name` in `dir/bin`, for a stand-in to run.
fn own(dir: &Path, name: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let p = bin.join(name);
    if !p.exists() {
        std::fs::copy(Path::new("/usr/bin").join(name), &p).unwrap();
    }
    p
}

fn stand_in(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("soffice");
    let init = "case \" $* \" in *\" --terminate_after_init \"*) exit 0;; esac";
    std::fs::write(&p, format!("#!/bin/sh\n{init}\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn short() -> Bounds {
    Bounds {
        timeout: Duration::from_secs(2),
        output: 64 << 10,
        memory: 1 << 30,
    }
}

/// Whether `pid` is a process that has not ended (a zombie has).
fn running(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

/// The run directories left under `root` (the profile template is not
/// one: it stays for the process's next runs).
fn runs_left(root: &Path) -> usize {
    let template = scratch::template_dir(root);
    std::fs::read_dir(root).map_or(0, |d| {
        d.flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(scratch::RUN_PREFIX)
            })
            .filter(|e| e.path() != template)
            .count()
    })
}

/// The reader that confines, or the test is skipped.
async fn helper(test: &str) -> Option<PathBuf> {
    crate::extract::convert::confine::test_helper(test).await
}

/// Waits, a tenth of a second at a time for at most five seconds, until
/// `done` (the reaper task runs on the runtime).
async fn until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..50 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    done()
}

/// A LibreOffice that hangs after starting a child (as `soffice` starts
/// `oosplash`, which starts `soffice.bin`), and the child's command line,
/// unique to the test (a confined run cannot write outside its directory,
/// so the child is found by it).
fn hanging(dir: &Path, n: u32) -> (PathBuf, String) {
    let marker = format!("{}.{n}{:06}", 280 + n, std::process::id() % 1_000_000);
    let sleep = own(dir, "sleep");
    let bin = stand_in(dir, &format!("{} {marker} &\nwait", sleep.display()));
    (bin, marker)
}

/// The pid of the process running `sleep <marker>`.
fn find(marker: &str) -> Option<i32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
        (cmd.trim_end().ends_with(&format!("/bin/sleep {marker}"))).then_some(pid)
    })
}

#[tokio::test]
async fn a_hanging_run_is_stopped_with_its_children_and_its_directory_goes() {
    let Some(helper) = helper("a_hanging_run_is_stopped").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    let (bin, marker) = hanging(t.path(), 1);
    let slot = pool::global().acquire().await;
    let e = convert(
        &slot,
        Office {
            bin: &bin,
            helper: &helper,
        },
        &root,
        Bytes::from_static(b"x"),
        Format::Doc,
        Target::Docx,
        short(),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .unwrap_err();
    drop(slot);
    assert_eq!(e, LoError::Timeout);
    assert!(e.said().contains("LIBREOFFICE_TIMEOUT"), "{}", e.said());
    assert!(
        until(|| find(&marker).is_none_or(|p| !running(p))).await,
        "the child lives on"
    );
    assert!(
        until(|| runs_left(&root) == 0).await,
        "the directory is left"
    );
}

#[tokio::test]
async fn a_dropped_conversion_kills_the_group_and_cleans_up_once_reaped() {
    let Some(helper) = helper("a_dropped_conversion_kills_the_group").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    let (bin, marker) = hanging(t.path(), 2);
    let slot = pool::global().acquire().await;
    let long = Bounds {
        timeout: Duration::from_secs(60),
        ..short()
    };
    let mut run = Box::pin(convert(
        &slot,
        Office {
            bin: &bin,
            helper: &helper,
        },
        &root,
        Bytes::from_static(b"x"),
        Format::Ppt,
        Target::Pptx,
        long,
        MAX_UNCOMPRESSED_BYTES,
    ));
    // The caller goes away while LibreOffice works.
    assert!(tokio::time::timeout(Duration::from_millis(1500), &mut run)
        .await
        .is_err());
    let child = find(&marker).expect("the confined stand-in started its child");
    drop(run);
    drop(slot);
    assert!(
        until(|| !running(child)).await,
        "the child {child} lives on"
    );
    assert!(
        until(|| runs_left(&root) == 0).await,
        "the directory is left"
    );
}

/// What a run writes beside its result, or outside its directory, is never
/// read: only `out/in.<ext>` is, and it must be the format asked for.
#[tokio::test]
async fn only_the_result_is_read_and_it_must_be_what_was_asked_for() {
    let Some(helper) = helper("only_the_result_is_read").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    let docx = t.path().join("result.docx");
    std::fs::write(&docx, crate::extract::test_files::docx(&["Hello"])).unwrap();
    let bin = stand_in(
        t.path(),
        &format!(
            "test -f profile/user/registrymodifications.xcu || exit 9\n\
             grep -q DisableMacrosExecution profile/user/registrymodifications.xcu || exit 8\n\
             test \"$SAL_USE_VCLPLUGIN\" = svp || exit 7\n\
             test \"$HOME\" = \"$PWD\" || exit 6\n\
             echo outside > ../outside.txt\n\
             {} {} out/in.docx\necho other > out/other.docx",
            own(t.path(), "cp").display(),
            docx.display()
        ),
    );
    let slot = pool::global().acquire().await;
    let got = convert(
        &slot,
        Office {
            bin: &bin,
            helper: &helper,
        },
        &root,
        Bytes::from_static(b"x"),
        Format::Doc,
        Target::Docx,
        short(),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(got, Bytes::from(std::fs::read(&docx).unwrap()));
    assert_eq!(runs_left(&root), 0);
    // Outside its own directory the confined run writes nothing.
    assert!(!root.join("outside.txt").exists());
    // A result of another kind is refused.
    let wrong = stand_in(t.path(), "echo '%PDF-1.7' > out/in.docx");
    let e = convert(
        &slot,
        Office {
            bin: &wrong,
            helper: &helper,
        },
        &root,
        Bytes::from_static(b"x"),
        Format::Doc,
        Target::Docx,
        short(),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .unwrap_err();
    assert_eq!(e, LoError::NotOpened);
    // So is a file LibreOffice says it could not load (a password).
    let locked = stand_in(
        t.path(),
        "echo 'Error: source file could not be loaded' >&2\nexit 1",
    );
    let e = convert(
        &slot,
        Office {
            bin: &locked,
            helper: &helper,
        },
        &root,
        Bytes::from_static(b"x"),
        Format::Doc,
        Target::Docx,
        short(),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .unwrap_err();
    assert_eq!(e, LoError::NotOpened);
    assert!(e.said().contains("password"), "{}", e.said());
    assert_eq!(runs_left(&root), 0);
}

#[test]
fn a_result_past_its_bound_is_refused_by_its_size() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("in.docx");
    std::fs::write(&p, vec![b'x'; 100]).unwrap();
    let rel = Path::new("in.docx");
    assert_eq!(read_bounded(t.path(), rel, 10), Err(LoError::TooLarge(100)));
    assert_eq!(read_bounded(t.path(), rel, 100).unwrap().len(), 100);
    // A link in its place is no result (security round B, L3).
    std::fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", &p).unwrap();
    assert_eq!(read_bounded(t.path(), rel, 100), Err(LoError::NotOpened));
    assert!(LoError::TooLarge(100)
        .said()
        .contains("MAX_UNCOMPRESSED_BYTES"));
}

/// The real LibreOffice and the reader that confines it, or the test is
/// skipped by name.
async fn real(test: &str) -> Option<(PathBuf, PathBuf)> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(found) = super::super::probe::lookup("soffice", &path, &[]) else {
        eprintln!("{test}: skipped, LibreOffice (soffice) is not on PATH");
        return None;
    };
    let helper = helper(test).await?;
    Some((found.path, helper))
}

/// `source` (an RTF) made into a `.doc` by an unconfined LibreOffice of the
/// test's own, in `dir`.
fn make_doc(soffice: &Path, dir: &Path, source: &[u8]) -> Bytes {
    let src = dir.join("source.rtf");
    std::fs::write(&src, source).unwrap();
    let made = std::process::Command::new(soffice)
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation={}",
            super::super::probe::file_url(&dir.join("maker"))
        ))
        .args(["--convert-to", "doc", "--outdir"])
        .arg(dir)
        .arg(&src)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    Bytes::from(std::fs::read(dir.join("source.doc")).unwrap())
}

/// The real LibreOffice, where it is installed: two runs at once, each
/// with its own profile, neither locking the other; the template's socket
/// in `/tmp` is gone after it was made.
#[tokio::test]
async fn two_real_runs_at_once_each_read_a_doc() {
    let Some((soffice, helper)) = real("two_real_runs_at_once_each_read_a_doc").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    let doc = make_doc(
        &soffice,
        t.path(),
        b"{\\rtf1\\ansi Hello from Word 97\\par}",
    );
    assert_eq!(
        crate::extract::sniff::sniff_convert(&doc).map(|s| s.sub),
        Ok("doc")
    );
    let office = Office {
        bin: &soffice,
        helper: &helper,
    };
    let pool = pool::global();
    let (a, b) = (pool.acquire().await, pool.acquire().await);
    let lim = MAX_UNCOMPRESSED_BYTES;
    let one = convert(
        &a,
        office,
        &root,
        doc.clone(),
        Format::Doc,
        Target::Docx,
        bounds(),
        lim,
    );
    let two = convert(
        &b,
        office,
        &root,
        doc,
        Format::Doc,
        Target::Docx,
        bounds(),
        lim,
    );
    let (one, two) = tokio::join!(one, two);
    for got in [one.unwrap(), two.unwrap()] {
        let text = crate::extract::office::extract(crate::extract::OfficeFormat::Docx, &got)
            .unwrap()
            .markdown();
        assert!(text.contains("Hello from Word 97"), "{text}");
    }
    assert_eq!(runs_left(&root), 0);
    let template = scratch::template_dir(&root).join("profile");
    assert!(!scratch::socket_path(&template).exists());
}

/// A PNG of `w`×`h` red pixels.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgb([255, 0, 0]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Every stored part of a zip (a docx) whose bytes are `needle`.
fn holds(zip: &[u8], needle: &[u8]) -> bool {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    (0..z.len()).any(|i| {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b).unwrap();
        b == needle
    })
}

/// The review's probe (B1): an RTF that names a picture by its absolute
/// path. Unconfined, LibreOffice either embeds the picture in the docx it
/// writes (older versions) or writes an external link to the named path
/// (26.8.3, measured 2026-11-20: `r:link` and a relationship with
/// `TargetMode="External"`, no `word/media/` entry); confined, neither a
/// picture in another directory nor one beside the run under the data
/// dir's spool reaches the result, while the text still converts. The
/// confined result then goes through lmgw's own docx route with pictures
/// embedded and referenced, markdown and json: none of the answer holds
/// the picture's bytes, and no picture artifact exists for it (a link has
/// no zip entry to read).
#[tokio::test]
async fn a_document_that_names_a_file_outside_its_run_gets_nothing_of_it() {
    let Some((soffice, helper)) = real("a_document_that_names_a_file_outside_its_run").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    std::fs::create_dir_all(&root).unwrap();
    let picture = png(37, 23);
    let outside = t.path().join("secret-outside.png");
    std::fs::write(&outside, &picture).unwrap();
    // A file in the spool directory itself, beside the run's directory.
    let beside = root.join("secret-beside.png");
    std::fs::write(&beside, &picture).unwrap();
    let rtf = |p: &Path| {
        format!(
            "{{\\rtf1\\ansi{{\\fonttbl{{\\f0 Arial;}}}}\\f0 Probe text.\\par {{\\field{{\\*\\fldinst \
             INCLUDEPICTURE \"{}\" \\\\d}}{{\\fldrslt }}}}\\par End.\\par}}",
            p.display()
        )
    };
    // The probe means something: unconfined, LibreOffice embeds the picture
    // or links the named path (26.8.3 links; older versions embed).
    let open = t.path().join("open");
    std::fs::create_dir_all(&open).unwrap();
    std::fs::write(open.join("in.rtf"), rtf(&outside)).unwrap();
    let made = std::process::Command::new(&soffice)
        .current_dir(&open)
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation={}",
            super::super::probe::file_url(&open.join("profile"))
        ))
        .args([
            "--infilter=Rich Text Format",
            "--convert-to",
            "docx:MS Word 2007 XML",
        ])
        .args(["--outdir", "out", "in.rtf"])
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let unconfined = std::fs::read(open.join("out/in.docx")).unwrap();
    assert!(
        holds(&unconfined, &picture) || links(&unconfined, &outside),
        "the probe embeds or links nothing unconfined"
    );
    let office = Office {
        bin: &soffice,
        helper: &helper,
    };
    let slot = pool::global().acquire().await;
    for named in [&outside, &beside] {
        let got = convert(
            &slot,
            office,
            &root,
            Bytes::from(rtf(named)),
            Format::Rtf,
            Target::Docx,
            bounds(),
            MAX_UNCOMPRESSED_BYTES,
        )
        .await
        .unwrap();
        assert!(
            !holds(&got, &picture),
            "{} reached the result",
            named.display()
        );
        let text = crate::extract::office::extract(crate::extract::OfficeFormat::Docx, &got)
            .unwrap()
            .markdown();
        assert!(text.contains("Probe text."), "{text}");
        answered_without(&got, &picture).await;
    }
    // The same through a .doc whose picture is a link: made while the file
    // it names did not exist, so the link stays a link, then the file is
    // put there.
    let later = t.path().join("secret-later.png");
    let doc = make_doc(&soffice, t.path(), rtf(&later).as_bytes());
    std::fs::write(&later, &picture).unwrap();
    assert!(!doc.windows(picture.len()).any(|w| w == &picture[..]));
    let got = convert(
        &slot,
        office,
        &root,
        doc,
        Format::Doc,
        Target::Docx,
        bounds(),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .unwrap();
    assert!(
        !holds(&got, &picture),
        "a .doc's linked picture reached the result"
    );
    assert_eq!(runs_left(&root), 0);
}

/// A result past the file-size limit stops LibreOffice by name.
#[tokio::test]
async fn a_result_past_the_file_size_limit_is_stopped_by_name() {
    let Some((soffice, helper)) = real("a_result_past_the_file_size_limit").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    let office = Office {
        bin: &soffice,
        helper: &helper,
    };
    let slot = pool::global().acquire().await;
    let long = format!(
        "{{\\rtf1\\ansi {}\\par}}",
        "Many words of text. ".repeat(20_000)
    );
    let e = convert(
        &slot,
        office,
        &root,
        Bytes::from(long),
        Format::Rtf,
        Target::Docx,
        bounds(),
        // The profile LibreOffice writes at start is larger than this.
        2048,
    )
    .await
    .unwrap_err();
    assert_eq!(e, LoError::FileTooLarge(2048));
    assert!(e.said().contains("file-size limit"), "{}", e.said());
    assert_eq!(runs_left(&root), 0);
}

/// Security round B, L5 and hardening 6: a socket already where the
/// template's LibreOffice would put its own fails the run as lmgw's
/// (`SocketTaken`), and a run that ends by itself without a result is not
/// worded as the file's fault.
#[tokio::test]
async fn a_taken_socket_or_a_run_without_result_is_not_the_file_s_fault() {
    let Some(helper) = helper("a_taken_socket_or_a_run_without_result").await else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("spool");
    std::fs::create_dir_all(&root).unwrap();
    let bin = stand_in(t.path(), "exit 0");
    let socket = scratch::socket_path(&scratch::template_dir(&root).join("profile"));
    let squat = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let run = |bin: PathBuf| {
        let (root, helper) = (root.clone(), helper.clone());
        async move {
            let slot = pool::global().acquire().await;
            convert(
                &slot,
                Office {
                    bin: &bin,
                    helper: &helper,
                },
                &root,
                Bytes::from_static(b"x"),
                Format::Doc,
                Target::Docx,
                short(),
                MAX_UNCOMPRESSED_BYTES,
            )
            .await
        }
    };
    let e = run(bin.clone()).await.unwrap_err();
    assert_eq!(e, LoError::SocketTaken);
    assert!(e.said().contains("not the file's fault"), "{}", e.said());
    assert!(!e.said().contains("password"), "{}", e.said());
    // The squatter gone: the stand-in ends at once with no result.
    drop(squat);
    let _ = std::fs::remove_file(&socket);
    let e = run(bin).await.unwrap_err();
    assert_eq!(e, LoError::NoResult);
    assert!(e.said().contains("not the file's fault"), "{}", e.said());
    assert_eq!(runs_left(&root), 0);
}
