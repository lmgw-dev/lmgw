//! poppler's limits on real runs (design §14 "Poppler limit from measured
//! peaks"): a run that needs more than its tool's limit is run again under
//! the wide one and says so, one that needs more than that is refused
//! naming both, a JPEG 2000 image drawn blank is read from stderr, a piped
//! file's share is added to the limit, and the same run reading by position
//! gets none. Where a run would need gigabytes, the test sets limits of its
//! own (`limit::with_limits`).

use std::path::Path;

use bytes::Bytes;

use super::*;
use crate::extract::convert::run::Input;
use crate::extract::test_files;

/// A one-page PDF drawing one `side` × `side` JPEG 2000 image
/// ([`test_files::jpx::page`]).
fn jpeg_2000_page(side: u32) -> Option<Vec<u8>> {
    test_files::jpx::page(side, 1.0)
}

const MIB: u64 = 1 << 20;

#[test]
fn each_tool_runs_under_its_own_limit_and_a_pipe_adds_the_file() {
    let text = POPPLER_TEXT_MEMORY_LIMIT_BYTES;
    use PopplerTool as T;
    for tool in [T::Text, T::Fonts, T::Info, T::ImageList] {
        assert_eq!(
            (tool.limit(), tool.draws()),
            (Limit::Text, false),
            "{tool:?}"
        );
    }
    for tool in [T::Render, T::Pictures] {
        assert_eq!(
            (tool.limit(), tool.draws()),
            (Limit::Image, true),
            "{tool:?}"
        );
    }
    assert_eq!(Limit::Text.bytes(), text);
    assert_eq!(Limit::Image.bytes(), POPPLER_IMAGE_MEMORY_LIMIT_BYTES);
    assert_eq!(Limit::Wide.bytes(), 4 << 30);
    assert_eq!(T::Pictures.name(), "pdftohtml -dataurls");
    let piped = Input::Bytes(Bytes::from(vec![0u8; 1000]));
    let b = reading(poppler(Limit::Text, PDF_TIMEOUT), &piped);
    assert_eq!(b.memory, text + 3000);
    let file = Input::File(tempfile::tempfile().unwrap());
    let b = reading(poppler(Limit::Text, PDF_TIMEOUT), &file);
    assert_eq!(b.memory, text);
}

/// One path of two million lines needs about 0.8 GiB of `pdftotext`
/// (measured 2026-11-20, 400 MiB a million): past
/// `POPPLER_TEXT_MEMORY_LIMIT_BYTES`, so it is run again under
/// `POPPLER_WIDE_MEMORY_LIMIT_BYTES`, which reads it, and says so.
#[tokio::test]
async fn a_text_run_past_its_limit_is_read_under_the_wide_one_and_says_so() {
    if !available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let t = text(Bytes::from(test_files::pdf_one_path(2_000_000)))
        .await
        .unwrap();
    assert_eq!(t.pages.len(), 1);
    assert_eq!(
        t.notes,
        [wide::Note::Widened {
            tool: "pdftotext",
            from: Limit::Text,
            why: wide::Why::Memory,
            span: None,
        }]
    );
    let said = t.notes[0].said("for the file");
    assert!(
        said.contains("POPPLER_TEXT_MEMORY_LIMIT_BYTES (512 MiB)")
            && said.contains("POPPLER_WIDE_MEMORY_LIMIT_BYTES (4.0 GiB)"),
        "{said}"
    );
    // A tenth of it reads under its own limit, and says nothing.
    let t = text(Bytes::from(test_files::pdf_one_path(200_000)))
        .await
        .unwrap();
    assert!(t.notes.is_empty());
}

/// Under limits of the test's own (text 96 MiB, wide 256 MiB; measured
/// 2026-11-20: 200 000 lines need about 120 MiB, a million more than 256):
/// a run past the wide limit too is refused naming both.
#[tokio::test]
async fn a_text_run_past_the_wide_limit_too_is_refused_naming_both() {
    if !available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let limits = [96 * MIB, 2 << 30, 256 * MIB];
    let fits = limit::with_limits(limits, text(Bytes::from(test_files::pdf_one_path(200_000))))
        .await
        .unwrap();
    assert!(
        matches!(
            &fits.notes[..],
            [wide::Note::Widened {
                from: Limit::Text,
                ..
            }]
        ),
        "{:?}",
        fits.notes
    );
    // Said inside the limits it ran under.
    let (e, s) = limit::with_limits(limits, async {
        let e = text(Bytes::from(test_files::pdf_one_path(1_000_000)))
            .await
            .unwrap_err();
        let s = e.to_string();
        (e, s)
    })
    .await;
    assert_eq!(
        e,
        PdfError::OutOfMemory {
            tool: "pdftotext",
            limit: Limit::Wide,
            piped: true,
            retry: Retry::Widened(Limit::Text),
        }
    );
    assert!(
        s.contains("more memory than POPPLER_WIDE_MEMORY_LIMIT_BYTES (256 MiB)")
            && s.contains("after POPPLER_TEXT_MEMORY_LIMIT_BYTES (96.0 MiB)")
            && s.contains("POPPLER_PIPE_READ_PER_BYTE (3)"),
        "{s}"
    );
}

/// 300 × 200 pt at 1 000 dpi (4 167 × 2 778 pixels, 4 bytes a pixel):
/// under an image limit of the test's own of 64 MiB poppler says "Out of
/// memory" and exits 0 with a stub (measured 2026-11-20), so the render is
/// run again under the wide limit (here 128 MiB) and comes back whole.
#[tokio::test]
async fn a_render_past_its_limit_is_rendered_under_the_wide_one() {
    if !available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let pdf = Bytes::from(test_files::pdf(&[Some("Hello")]));
    let limits = [512 * MIB, 64 * MIB, 128 * MIB];
    let r = limit::with_limits(
        limits,
        render_page_with(Path::new(PDFTOPPM), pdf.clone(), 1, 1000, PDF_TIMEOUT),
    )
    .await
    .unwrap();
    assert!(r.png.starts_with(b"\x89PNG") && r.png.len() > 10_000);
    assert!(
        matches!(
            &r.notes[..],
            [wide::Note::Widened {
                tool: "pdftoppm",
                from: Limit::Image,
                why: wide::Why::Memory,
                span: Some((1, 1)),
            }]
        ),
        "{:?}",
        r.notes
    );
}

/// The review of the poppler limit, S1 (probed 2026-11-20 on poppler
/// 26.08.0): a JPEG 2000 image whose decode does not fit the limit is drawn
/// blank and the run exits 0, saying "Size of tile data exceeds system
/// limits" and "Did no succeed opening JPX Stream." on stderr. A 2 000²
/// image needs 80–96 MiB; under an image limit of the test's own of 64 MiB
/// the render is run again under the wide one (256 MiB) and the image is
/// there; with the wide one at 64 MiB too it stays blank and is said so by
/// name, and nothing says it was widened (nothing more was decoded).
#[tokio::test]
async fn a_jpeg_2000_image_left_blank_is_run_again_and_named_when_it_stays_blank() {
    if !available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let Some(pdf) = jpeg_2000_page(2000) else {
        eprintln!("skipped: opj_compress (openjpeg) is not on PATH");
        return;
    };
    let pdf = Bytes::from(pdf);
    let render = |limits| {
        let pdf = pdf.clone();
        limit::with_limits(limits, async move {
            render_page_with(Path::new(PDFTOPPM), pdf, 1, 20, PDF_TIMEOUT).await
        })
    };
    let whole = render([512 * MIB, 64 * MIB, 256 * MIB]).await.unwrap();
    assert!(
        matches!(
            &whole.notes[..],
            [wide::Note::Widened {
                from: Limit::Image,
                why: wide::Why::Jpx,
                ..
            }]
        ),
        "{:?}",
        whole.notes
    );
    let blank = render([512 * MIB, 64 * MIB, 64 * MIB]).await.unwrap();
    assert!(blank.png.starts_with(b"\x89PNG"));
    assert_ne!(blank.png, whole.png, "the image is drawn blank");
    assert!(
        matches!(
            &blank.notes[..],
            [wide::Note::Undecoded {
                tool: "pdftoppm",
                limit: Limit::Wide,
                why: wide::Blank::TooLarge,
                ..
            }]
        ),
        "{:?}",
        blank.notes
    );
    // Under the real limits it decodes at once and says nothing.
    let plain = render_page_with(Path::new(PDFTOPPM), pdf, 1, 20, PDF_TIMEOUT)
        .await
        .unwrap();
    assert!(plain.notes.is_empty(), "{:?}", plain.notes);
    assert_eq!(plain.png, whole.png);
}

/// Review 2 of the poppler limit, S1: a damaged JPEG 2000 image (its
/// stream cut in half) is drawn blank under every limit; it is said
/// (`image_not_decoded`, damaged) under the run's own limit and never run
/// again under the wide one.
#[tokio::test]
async fn a_damaged_jpeg_2000_image_is_said_and_not_run_again() {
    if !available().await {
        eprintln!("skipped: pdftotext/pdftoppm are not on PATH");
        return;
    }
    let Some(pdf) = test_files::jpx::page(2000, 0.5) else {
        eprintln!("skipped: opj_compress (openjpeg) is not on PATH");
        return;
    };
    let kept = wide::notes::Notes::new();
    let got = wide::notes::scope(
        kept.clone(),
        render_page_with(Path::new(PDFTOPPM), Bytes::from(pdf), 1, 20, PDF_TIMEOUT),
    )
    .await
    .unwrap();
    assert!(got.png.starts_with(b"\x89PNG"));
    let blank = wide::Note::Undecoded {
        tool: "pdftoppm",
        limit: Limit::Image,
        why: wide::Blank::Damaged,
        span: Some((1, 1)),
    };
    assert_eq!(got.notes, std::slice::from_ref(&blank));
    assert_eq!(kept.take(), [blank]);
}
