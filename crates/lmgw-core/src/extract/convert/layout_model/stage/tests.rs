//! The stage's batches and bounds, with a stand-in for the session.

use super::*;
#[cfg(unix)]
use crate::extract::test_files;

#[test]
fn picked_pages_go_in_runs_of_neighbours_up_to_the_batch() {
    assert_eq!(
        batches(&[1, 2, 3, 5, 6, 40], 2),
        vec![(1, 2), (3, 3), (5, 6), (40, 40)]
    );
    assert_eq!(batches(&[7], 16), vec![(7, 7)]);
    assert!(batches(&[], 16).is_empty());
    let many: Vec<u32> = (1..=40).collect();
    assert_eq!(
        batches(&many, RENDER_BATCH_PAGES),
        vec![(1, 16), (17, 32), (33, 40)]
    );
    assert_eq!(RENDER_BATCH_PAGES, 16);
    assert_eq!(crate::hf::fmt_bytes(RENDER_BATCH_BYTES), "18.8 MiB");
}

#[cfg(unix)]
type Answer = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<super::super::Region>, session::InferError>>>,
>;

/// A stand-in session: no regions, after `each` per page.
#[cfg(unix)]
fn answering(each: Duration) -> impl FnMut(Vec<u8>) -> Answer {
    move |input: Vec<u8>| {
        Box::pin(async move {
            assert_eq!(input.len() as u64, PAGE_PICTURE_BYTES);
            tokio::time::sleep(each).await;
            Ok(Vec::new())
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn every_page_is_read_and_its_picture_deleted() {
    if !crate::extract::pdf::available().await {
        eprintln!("skipped: poppler is not on PATH");
        return;
    }
    let pdf = crate::extract::convert::ocr::spool::Spool::of(
        None,
        bytes::Bytes::from(test_files::pdf(&[Some("One"), Some("Two"), Some("Three")])),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut out = Regions::default();
    let stopped = read_pages(
        &[1, 3],
        &pdf,
        Path::new(pdf::PDFTOPPM),
        dir.path(),
        pdf::PDF_TIMEOUT,
        answering(Duration::ZERO),
        &mut out,
    )
    .await;
    assert_eq!(stopped, None);
    assert_eq!(out.pages.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
    assert!(out
        .pages
        .values()
        .all(|p| matches!(p, PageRegions::Found(_))));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// A render that does not finish within what is left of the renders'
/// budget ends the stage: the pages not read are named with why.
#[cfg(unix)]
#[tokio::test]
async fn a_render_that_runs_out_of_the_budget_ends_the_stage() {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = tempfile::tempdir().unwrap();
    let slow = bin_dir.path().join("pdftoppm");
    std::fs::write(&slow, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut out = Regions::default();
    let started = std::time::Instant::now();
    let stopped = read_pages(
        &[1, 2, 3, 30],
        &crate::extract::convert::ocr::spool::Spool::of(
            None,
            bytes::Bytes::from_static(b"%PDF-1.4"),
        )
        .await,
        &slow,
        dir.path(),
        Duration::from_millis(300),
        answering(Duration::ZERO),
        &mut out,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let why = stopped.expect("the stage ended");
    assert!(why.contains("PDF_TIMEOUT"), "{why}");
    // Nothing was read; the second batch was never started.
    assert!(out.pages.is_empty());
}

/// With the caller's `document_timeout`, the stage stops once the file's
/// clock has used MODEL_DEADLINE_SHARE of it.
#[cfg(unix)]
#[tokio::test]
async fn the_stage_leaves_the_rest_of_the_caller_s_time_to_the_geometry() {
    if !crate::extract::pdf::available().await {
        eprintln!("skipped: poppler is not on PATH");
        return;
    }
    let pages: Vec<Option<&str>> = (0..6).map(|_| Some("Example")).collect();
    let pdf = crate::extract::convert::ocr::spool::Spool::of(
        None,
        bytes::Bytes::from(test_files::pdf(&pages)),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let clock = pool::clock::FileClock::new(Duration::from_secs(2));
    let (c, d) = (clock.clone(), dir.path().to_path_buf());
    let (stopped, out) = pool::clock::scope(c, async move {
        let mut out = Regions::default();
        let stopped = read_pages(
            &[1, 2, 3, 4, 5, 6],
            &pdf,
            Path::new(pdf::PDFTOPPM),
            &d,
            pdf::PDF_TIMEOUT,
            answering(Duration::from_millis(400)),
            &mut out,
        )
        .await;
        (stopped, out)
    })
    .await;
    let why = stopped.expect("the stage ended");
    assert!(
        why.contains("MODEL_DEADLINE_SHARE") && why.contains("document_timeout of 2 s"),
        "{why}"
    );
    // One second of two: two or three pages at 0.4 s, not all six.
    let read = out.pages.len();
    assert!((1..=3).contains(&read), "{read} pages read");
    assert!(
        clock.used() < Duration::from_millis(1600),
        "{:?}",
        clock.used()
    );
}
