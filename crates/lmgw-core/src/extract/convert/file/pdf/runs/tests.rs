//! A PDF's two geometry runs side by side (performance round review S3):
//! when the bbox run fails, the font run is stopped at once and its slot
//! comes back once it is reaped, with the same failure as before.

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::*;
use crate::extract::convert::pool::test_pool;

/// An executable script `name` in `dir` with `body`.
fn stand_in(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test]
async fn a_failed_bbox_run_stops_the_font_run_beside_it() {
    if crate::extract::convert::run::start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bbox = stand_in(
        dir.path(),
        "pdftotext",
        "echo 'Syntax Error: example' >&2; exit 1",
    );
    let fonts = stand_in(dir.path(), "pdftohtml", "exec sleep 60");
    let pool = test_pool::small(8, 2);
    let src = Spool::of(None, Bytes::from_static(b"%PDF-1.4\n")).await;
    let started = Instant::now();
    // An empty pool with memory for both: they run side by side.
    assert!(pool.facts().charged_bytes == 0);
    let ran = test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None)).await;
    assert!(
        matches!(ran.bbox, Err(PdfError::Failed { .. })),
        "{:?}",
        ran.bbox.err()
    );
    assert!(matches!(ran.fonts, Fonts::Missing));
    drop(ran);
    // The sleeping font run was killed and reaped: its slots come back.
    while pool.facts().running > 0 {
        assert!(started.elapsed() < Duration::from_secs(20), "still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// PDF charge after the runs (2026-11-20): side by side (two slots free)
/// and one after the other, the file's slot comes back as one slot charged
/// what it still holds, the runs' outputs and their reading.
#[tokio::test]
async fn after_its_runs_the_file_s_slot_is_charged_what_it_still_holds() {
    if crate::extract::convert::run::start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bbox = stand_in(
        dir.path(),
        "pdftotext",
        "printf '<doc><page width=\"9\" height=\"9\"></page></doc>\\n'",
    );
    let fonts = stand_in(dir.path(), "pdftohtml", "printf '<pdf2xml></pdf2xml>\\n'");
    // Memory for both runs side by side, then for one PDF step only.
    for pdf_steps in [2, 1] {
        let pool = test_pool::small(8, pdf_steps);
        let src = Spool::of(None, Bytes::from_static(b"%PDF-1.4\n")).await;
        let ran = test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None)).await;
        let xml = ran.bbox.as_ref().expect("the stand-in's output");
        let Fonts::Read(fonts_xml) = &ran.fonts else {
            panic!("the font run's output was read");
        };
        assert!(xml.starts_with(b"<doc>") && fonts_xml.starts_with(b"<pdf2xml>"));
        let held = pool::charge::after_runs(&[xml, fonts_xml]);
        let f = pool.facts();
        assert_eq!((f.running, f.charged_bytes), (1, held), "{pdf_steps}");
        assert!(held < 1 << 20);
        drop(ran);
        assert_eq!(pool.facts().running, 0);
    }
}

/// The review of the poppler limit, S2: a layout run that runs out of
/// poppler's text limit is run again, with the font run, under the wide
/// limit, and the file's notes say so; one that runs out of that too is
/// refused naming both limits. The stand-ins read their own address-space
/// limit (`ulimit -v`, KiB) and say "Out of memory" below 1 GiB, or always.
#[tokio::test]
async fn a_layout_run_out_of_the_text_limit_is_run_again_under_the_wide_one() {
    use crate::extract::pdf::wide::{notes, Note, Why};
    use crate::extract::pdf::{Limit, Retry};
    if crate::extract::convert::run::start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let small = "if [ \"$(ulimit -v)\" -lt 1048576 ]; then echo 'Out of memory' >&2; exit 1; fi";
    let bbox = stand_in(
        dir.path(),
        "pdftotext",
        &format!("{small}\nprintf '<doc><page width=\"9\" height=\"9\"></page></doc>\\n'"),
    );
    let fonts = stand_in(dir.path(), "pdftohtml", "printf '<pdf2xml></pdf2xml>\\n'");
    let pool = test_pool::small(8, 3);
    let src = Spool::of(None, Bytes::from_static(b"%PDF-1.4\n")).await;
    let kept = notes::Notes::new();
    let ran = notes::scope(
        kept.clone(),
        test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None)),
    )
    .await;
    assert!(ran.bbox.as_ref().is_ok_and(|x| x.starts_with(b"<doc>")));
    assert!(matches!(ran.fonts, Fonts::Read(_)));
    assert_eq!(
        kept.take(),
        [Note::Widened {
            tool: "pdftotext",
            from: Limit::Text,
            why: Why::Memory,
            span: None,
        }]
    );
    drop(ran);
    // Out of the wide limit too: refused, naming both, and nothing said.
    let always = stand_in(
        dir.path(),
        "pdftotext-big",
        "echo 'Out of memory' >&2; exit 1",
    );
    let ran = notes::scope(
        kept.clone(),
        test_pool::within(pool, geometry(&src, &always, Some(&fonts), None)),
    )
    .await;
    let e = ran.bbox.as_ref().unwrap_err();
    assert!(
        matches!(
            e,
            PdfError::OutOfMemory {
                limit: Limit::Wide,
                retry: Retry::Widened(Limit::Text),
                ..
            }
        ),
        "{e:?}"
    );
    assert!(
        e.to_string()
            .contains("POPPLER_WIDE_MEMORY_LIMIT_BYTES (4.0 GiB)"),
        "{e}"
    );
    assert!(kept.take().is_empty());
}

/// Review 3 of the poppler limit, S1: poppler's "Bogus memory allocation
/// size" (a damaged object's overflowing size, skipped; poppler goes on and
/// exits 0) is not out of memory: the run's output is used, it is not run
/// again wide, and nothing is said of a limit.
#[tokio::test]
async fn a_bogus_allocation_size_on_exit_0_is_not_run_again() {
    use crate::extract::pdf::wide::notes;
    if crate::extract::convert::run::start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bbox = stand_in(
        dir.path(),
        "pdftotext",
        "echo 'Syntax Error: Bogus memory allocation size' >&2\n\
         if [ \"$(ulimit -v)\" -ge 1048576 ]; then echo 'run wide' >&2; exit 1; fi\n\
         printf '<doc><page width=\"9\" height=\"9\"></page></doc>\\n'",
    );
    let fonts = stand_in(dir.path(), "pdftohtml", "printf '<pdf2xml></pdf2xml>\\n'");
    let pool = test_pool::small(8, 3);
    let src = Spool::of(None, Bytes::from_static(b"%PDF-1.4\n")).await;
    let kept = notes::Notes::new();
    let ran = notes::scope(
        kept.clone(),
        test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None)),
    )
    .await;
    assert!(
        ran.bbox.as_ref().is_ok_and(|x| x.starts_with(b"<doc>")),
        "{:?}",
        ran.bbox.as_ref().err()
    );
    assert!(matches!(ran.fonts, Fonts::Read(_)));
    assert!(kept.take().is_empty());
}

/// Review 2 of the poppler limit, nit: a file whose layout outputs need
/// more than its slot holds, where the pool has not got it free, gives its
/// slot back and runs again on a slot charged what they showed, taken in
/// the queue; with no time left on the file's clock it is not run again.
/// The stand-in writes 900 MiB (its reading needs about 2.8 GiB, more than
/// `PDF_FILE`'s 2.5; it runs confined, so it makes the bytes itself);
/// another caller holds 1 GiB of a pool of 3.75, so the grow does not fit
/// while it runs.
#[tokio::test]
async fn outputs_that_cannot_grow_their_slot_run_again_on_one_charged_what_they_showed() {
    use crate::extract::convert::pool::{as_caller_now, clock};
    if crate::extract::convert::run::start::starter().is_none() {
        eprintln!("skipped: lmgw's document reader is not built beside the tests");
        return;
    }
    const GIB: u64 = 1 << 30;
    const OUT: usize = 943_718_400;
    let dir = tempfile::tempdir().unwrap();
    let bbox = stand_in(dir.path(), "pdftotext", &format!("yes 0 | head -c {OUT}"));
    let fonts = stand_in(dir.path(), "pdftohtml", "printf '<pdf2xml></pdf2xml>\\n'");
    let pool = test_pool::small(8, 1);
    let src = Spool::of(None, Bytes::from_static(b"%PDF-1.4\n")).await;

    // No time left: read on the slot it has (charged `PDF_FILE` and the
    // piped file's share, never
    // what the outputs showed), not run again.
    let blocker = as_caller_now("other.example", || pool.try_acquire_charged(1, GIB)).unwrap();
    let ran = clock::scope(
        clock::FileClock::new(Duration::ZERO),
        test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None)),
    )
    .await;
    assert_eq!(
        ran.bbox.as_ref().map(Vec::len).map_err(|e| e.to_string()),
        Ok(OUT)
    );
    let piped = pool::charge::piped(&src);
    assert_eq!(ran.slot.bytes(), pool::charge::PDF_FILE + piped);
    drop(ran);

    // Time left: the second try waits in the queue for a slot charged what
    // the outputs need, and runs once the other caller's step has ended.
    let task = test_pool::within(pool, geometry(&src, &bbox, Some(&fonts), None));
    let watch = async {
        while pool.facts().waiting == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(blocker);
    };
    let (ran, ()) = tokio::join!(task, watch);
    assert_eq!(ran.bbox.as_ref().map(Vec::len).ok(), Some(OUT));
    assert!(
        ran.slot.bytes() > pool::charge::PDF_FILE + GIB / 4,
        "{}",
        ran.slot.bytes()
    );
    let f = pool.facts();
    assert_eq!((f.running, f.charged_bytes), (1, ran.slot.bytes()));
    drop(ran);
    assert_eq!(pool.facts().charged_bytes, 0);
}
