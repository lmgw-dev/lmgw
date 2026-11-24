//! docling's `document_timeout` while a PDF's picture runs are in flight
//! (WP10 on WP8's per-file clock): the file fails by name, the picture
//! run's child is killed and every slot comes back.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lmgw_api_types::documents::convert::{ErrorCode, Pictures};

use super::*;
use crate::extract::convert::{batch, pool};

/// `name` on this machine's `PATH`.
fn real(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[tokio::test]
async fn a_document_timeout_with_picture_runs_in_flight_ends_them_and_frees_their_slots() {
    if with_poppler().await.is_none() {
        return;
    }
    // poppler as it is, except that the picture run (`pdftohtml
    // -dataurls`) hangs.
    let dir = tempfile::tempdir().unwrap();
    for name in ["pdftotext", "pdftohtml", "pdftoppm", "pdfinfo", "pdfimages"] {
        let Some(bin) = real(name) else {
            eprintln!("skipped: {name} is not on PATH");
            return;
        };
        // Confined, the stand-in can write no pid file: its picture run
        // stays the shell that names it, found in `/proc`.
        let hold = if name == "pdftohtml" {
            "case \" $* \" in *\" -dataurls \"*) sleep 60; exit;; esac\n".to_string()
        } else {
            String::new()
        };
        let path = dir.path().join(name);
        let script = format!("#!/bin/sh\n{hold}exec '{}' \"$@\"\n", bin.display());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let facts = DocTools::on_path_with_own(dir.path().as_os_str().to_owned())
        .current()
        .await;
    let pdf = build::pdf(&[Page::new(
        vec![(72.0, 80.0, 12.0, "Example text")],
        vec![Drawn::at(
            72.0,
            140.0,
            300.0,
            200.0,
            (30, 20),
            [10, 60, 200],
        )],
    )]);
    let plan = plan_of(ConvertOptions {
        pictures: Pictures::Referenced,
        ..Default::default()
    });
    let each = batch::Each {
        deadline: Some(Duration::from_secs(1)),
        ..Default::default()
    };
    let started = Instant::now();
    let html = dir.path().join("pdftohtml");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<u32>));
    let watch = {
        let seen = seen.clone();
        tokio::spawn(async move {
            loop {
                if let Some(pid) = picture_run(&html) {
                    *seen.lock().unwrap() = Some(pid);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let (_, answer) = batch::convert_each(
        vec![input(pdf)],
        &plan,
        &facts,
        Vec::new(),
        OutputBudget::request(),
        &each,
    )
    .await;
    watch.abort();
    let d = &answer.documents[0];
    assert_eq!(d.status, DocStatus::Failed, "{d:?}");
    let e = d.error.as_ref().unwrap();
    assert_eq!(e.code, ErrorCode::DeadlineExceeded, "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(30));
    let pid: u32 = seen.lock().unwrap().expect("the picture run started");
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let gone = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, r)| r.starts_with('Z') || r.starts_with('X'))
        });
        if gone && pool::global().facts().running == 0 {
            break;
        }
        assert!(Instant::now() < until, "picture run or slot still held");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The live shell of the `pdftohtml` stand-in at `path` that runs a
/// picture run (`-dataurls` on its command line).
fn picture_run(path: &std::path::Path) -> Option<u32> {
    let mark = path.display().to_string();
    std::fs::read_dir("/proc").ok()?.find_map(|e| {
        let pid: u32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let words: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        let named = words.contains(&mark.as_bytes()) && words.contains(&b"-dataurls".as_slice());
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let live = stat
            .rsplit_once(") ")
            .is_some_and(|(_, r)| !r.starts_with('Z') && !r.starts_with('X'));
        (named && live).then_some(pid)
    })
}
