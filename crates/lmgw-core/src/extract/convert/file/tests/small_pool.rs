//! The side-by-side paths through a pool whose memory admits fewer PDF
//! steps than it has slots (`pool::test_pool::small`), the normal case
//! since the performance round (performance round review B1): a PDF's
//! picture crops whose request output runs out at the second page (a page
//! cut again in its turn), its two geometry runs, and its pages of text
//! recognition, each with several files at once. None of them may wait for
//! a slot while it holds one: each test ends within a timeout.

use std::time::Duration;

use lmgw_api_types::documents::convert::{Ocr, Pictures};

use super::ocr::{image_pdf, scanned, with_ocr, with_ocr_plan};
use super::pictures::build::{self, Drawn, Page};
use super::pictures::mode;
use super::*;
use crate::extract::convert::pool::test_pool;

/// Far longer than any of these takes (a few seconds); a hang ends here.
const HANG: Duration = Duration::from_secs(120);

/// `plan` with its files spooled under `dir`, as the gateway's are.
fn spooled(mut plan: Plan, dir: &std::path::Path) -> Plan {
    plan.spool_dir = Some(dir.to_path_buf());
    plan
}

/// Six pages, each with a large picture of its own colour.
fn six_pictures() -> Vec<u8> {
    let pages: Vec<Page<'_>> = (0..6u8)
        .map(|i| {
            Page::new(
                vec![(72.0, 60.0, 12.0, "Example text")],
                vec![Drawn::at(
                    60.0,
                    100.0,
                    450.0,
                    600.0,
                    (30 + u32::from(i), 40),
                    [200, 20 + 30 * i, 20],
                )],
            )
        })
        .collect();
    build::pdf(&pages)
}

#[tokio::test]
async fn crops_cut_again_in_their_turn_finish_on_a_pool_with_memory_for_two_pdfs() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let plan = spooled(mode(Pictures::Embedded, Output::Markdown), dir.path());
    let pdf = six_pictures();
    let pool = test_pool::small(8, 2);
    // What the file needs whole: its text and six crops.
    let all = Arc::new(OutputBudget::request());
    let whole = test_pool::within(
        pool,
        super::super::convert(input(pdf.clone()), &plan, &facts, &all),
    );
    let d = tokio::time::timeout(HANG, whole).await.expect("hung").doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert!(d.pictures.iter().all(|p| p.bytes.is_some()), "{d:?}");
    // A quarter of it: the first crop fits, the second (cut with all of it,
    // side by side) is cut again on its own slot, the rest do not fit.
    let quarter = Arc::new(OutputBudget::with_limit(all.used() / 4));
    let cut = test_pool::within(
        pool,
        super::super::convert(input(pdf), &plan, &facts, &quarter),
    );
    let d = tokio::time::timeout(HANG, cut).await.expect("hung").doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let kept = d.pictures.iter().filter(|p| p.bytes.is_some()).count();
    assert_eq!(kept, 1, "{:?}", d.warnings);
    assert!(d
        .warnings
        .iter()
        .any(|w| w.message.contains("MAX_UNCOMPRESSED_BYTES")));
    let f = pool.facts();
    assert_eq!((f.running, f.waiting, f.charged_bytes), (0, 0, 0));
}

#[tokio::test]
async fn several_pdfs_with_pictures_at_once_finish_on_a_pool_with_memory_for_one() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let plan = spooled(mode(Pictures::Embedded, Output::Markdown), dir.path());
    let pool = test_pool::small(8, 1);
    let pdf = six_pictures();
    let files = (0..4).map(|_| {
        let (plan, facts, pdf) = (&plan, &facts, pdf.clone());
        async move {
            // A budget a file of its own, each running out partway.
            let b = Arc::new(OutputBudget::with_limit(64 << 10));
            super::super::convert(input(pdf), plan, facts, &b).await.doc
        }
    });
    let all = test_pool::within(pool, futures::future::join_all(files));
    for d in tokio::time::timeout(HANG, all).await.expect("hung") {
        assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    }
    assert_eq!(pool.facts().running, 0);
}

#[tokio::test]
async fn scanned_pages_of_several_files_finish_on_a_pool_with_memory_for_one_pdf() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let plan = spooled(with_ocr_plan(Ocr::Auto, None), dir.path());
    // Four pages of text recognition and one PDF fit its memory.
    let pool = test_pool::small(8, 1);
    let page = scanned(&["Example scanned page", "The widgets sold well."]).await;
    let pdf = image_pdf(&[Some(&page), Some(&page), Some(&page), None, Some(&page)]);
    let files = (0..3).map(|_| {
        let (plan, facts, pdf) = (&plan, &facts, pdf.clone());
        async move { convert(input(pdf), plan, facts).await.doc }
    });
    let all = test_pool::within(pool, futures::future::join_all(files));
    for d in tokio::time::timeout(HANG, all).await.expect("hung") {
        assert_eq!(d.status, DocStatus::Converted, "{d:?}");
        assert_eq!(d.stats.parts_by_source.ocr, 4, "{d:?}");
        assert!(d.content.unwrap().contains("widgets sold well"));
    }
    let f = pool.facts();
    assert_eq!((f.running, f.waiting, f.charged_bytes), (0, 0, 0));
}
