//! Text recognition's render with the JPEG 2000 watch (review 2 of the
//! poppler limit, S4), on real poppler: a damaged image is said on its page
//! and not run again; one openjpeg had no memory for (a 2 000² image, 80–96
//! MiB, under a render limit of the test's own of 64 MiB) is rendered again
//! under the wide one and said so.

use std::path::Path;

use bytes::Bytes;

use super::*;
use crate::extract::pdf::wide::{notes, Blank, Note, Why};
use crate::extract::test_files;

const MIB: u64 = 1 << 20;

/// Page 1 of `pdf` rendered inside a notes scope: its PGM and the notes.
async fn rendered(pdf: Vec<u8>) -> (Vec<u8>, Vec<Note>) {
    let spool = Arc::new(Spool::of(None, Bytes::from(pdf)).await);
    let kept = notes::Notes::new();
    let (slot, pgm) = notes::scope(kept.clone(), page(&spool, Path::new(pdf::PDFTOPPM), 1, 72))
        .await
        .unwrap_or_else(|e| panic!("{}", e.said()));
    drop(slot);
    (pgm, kept.take())
}

#[tokio::test]
async fn a_scan_s_jpeg_2000_image_is_watched_like_any_render() {
    if !pdf::available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let (Some(damaged), Some(whole)) = (
        test_files::jpx::page(2000, 0.5),
        test_files::jpx::page(2000, 1.0),
    ) else {
        eprintln!("skipped: opj_compress (openjpeg) is not on PATH");
        return;
    };
    let (pgm, said) = rendered(damaged).await;
    assert!(pgm.starts_with(b"P5"));
    assert_eq!(
        said,
        [Note::Undecoded {
            tool: "pdftoppm",
            limit: Limit::OcrRender,
            why: Blank::Damaged,
            span: Some((1, 1)),
        }]
    );
    let (pgm, said) =
        pdf::limit::with_limits([512 * MIB, 64 * MIB, 256 * MIB], rendered(whole.clone())).await;
    assert!(pgm.starts_with(b"P5"));
    assert_eq!(
        said,
        [Note::Widened {
            tool: "pdftoppm",
            from: Limit::OcrRender,
            why: Why::Jpx,
            span: Some((1, 1)),
        }]
    );
    // Under the real limit it decodes at once and says nothing.
    let (plain, said) = rendered(whole).await;
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(plain, pgm);
}
