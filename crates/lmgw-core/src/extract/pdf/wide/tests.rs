//! When a run is run again under the wide limit and what it says
//! ([`settle`]), on outcomes made up for the purpose; the real runs are in
//! `pdf/tests_limit.rs`.

use std::time::Duration;

use super::*;
use crate::extract::convert::pool::clock;

fn oom(limit: Limit) -> Result<Ran, PdfError> {
    Err(PdfError::OutOfMemory {
        tool: "pdftotext",
        limit,
        piped: false,
        retry: Retry::No,
    })
}

/// A run's output with `memory` images openjpeg had no memory for and
/// `damaged` damaged ones.
fn ran(memory: u32, damaged: u32) -> Result<Ran, PdfError> {
    Ok(Ran {
        out: b"out".to_vec(),
        blank: jpx::Blank { memory, damaged },
    })
}

const T: Duration = Duration::from_secs(300);

/// `settle` inside a notes scope: its outcome and what the scope kept.
async fn settled(
    tool: PopplerTool,
    first: Result<Ran, PdfError>,
    again: Option<Result<Ran, PdfError>>,
) -> (Result<Got, PdfError>, Vec<Note>) {
    let kept = notes::Notes::new();
    let got = notes::scope(
        kept.clone(),
        settle(tool, Some((3, 3)), first, T, |t, _kept| async move {
            assert_eq!(t, T, "the run again has the run's time");
            again.expect("run again although it was not to be")
        }),
    )
    .await;
    (got, kept.take())
}

#[tokio::test]
async fn a_run_out_of_its_own_limit_is_run_again_wide_and_says_so() {
    let (got, kept) = settled(PopplerTool::Text, oom(Limit::Text), Some(ran(0, 0))).await;
    let got = got.unwrap();
    let widened = Note::Widened {
        tool: "pdftotext",
        from: Limit::Text,
        why: Why::Memory,
        span: Some((3, 3)),
    };
    assert_eq!(
        (got.out.as_slice(), &got.notes),
        (&b"out"[..], &vec![widened.clone()])
    );
    assert_eq!(kept, [widened]);
}

#[tokio::test]
async fn a_run_out_of_the_wide_limit_too_names_both() {
    let (got, kept) = settled(PopplerTool::Text, oom(Limit::Text), Some(oom(Limit::Wide))).await;
    let e = got.unwrap_err();
    assert_eq!(
        e,
        PdfError::OutOfMemory {
            tool: "pdftotext",
            limit: Limit::Wide,
            piped: false,
            retry: Retry::Widened(Limit::Text),
        }
    );
    let s = e.to_string();
    assert!(
        s.contains("POPPLER_WIDE_MEMORY_LIMIT_BYTES (4.0 GiB)")
            && s.contains("POPPLER_TEXT_MEMORY_LIMIT_BYTES (512 MiB)"),
        "{s}"
    );
    assert!(kept.is_empty());
}

/// Review 2, S1 and the nits: a JPEG 2000 image openjpeg had no memory
/// for is run again; still blank there it is only `image_not_decoded` (no
/// `memory_limit_widened`: nothing more was decoded), and one of two that
/// decoded there is both. A damaged image is said and never run again.
#[tokio::test]
async fn a_blank_image_is_run_again_only_for_memory_and_said_as_it_ended() {
    let undecoded = |limit, why| Note::Undecoded {
        tool: "pdftoppm",
        limit,
        why,
        span: Some((3, 3)),
    };
    let widened = Note::Widened {
        tool: "pdftoppm",
        from: Limit::Image,
        why: Why::Jpx,
        span: Some((3, 3)),
    };
    let (got, _) = settled(PopplerTool::Render, ran(1, 0), Some(ran(1, 0))).await;
    let notes = got.unwrap().notes;
    assert_eq!(notes, [undecoded(Limit::Wide, Blank::TooLarge)]);
    let s = notes[0].said("on page 3");
    assert!(
        s.contains("JPEG 2000 image on page 3 within POPPLER_WIDE_MEMORY_LIMIT_BYTES")
            && s.contains("larger than about 340 million pixels"),
        "{s}"
    );
    let (got, _) = settled(PopplerTool::Render, ran(2, 0), Some(ran(1, 0))).await;
    assert_eq!(
        got.unwrap().notes,
        [widened.clone(), undecoded(Limit::Wide, Blank::TooLarge)]
    );
    let (got, _) = settled(PopplerTool::Render, ran(1, 1), Some(ran(0, 1))).await;
    assert_eq!(
        got.unwrap().notes,
        [widened, undecoded(Limit::Wide, Blank::Damaged)]
    );
    // Damaged only: not run again (`settled` panics if it is).
    let (got, kept) = settled(PopplerTool::Render, ran(0, 1), None).await;
    let notes = got.unwrap().notes;
    assert_eq!(notes, [undecoded(Limit::Image, Blank::Damaged)]);
    assert!(notes[0].said("on page 3").contains("damaged"), "{notes:?}");
    assert_eq!(kept, notes);
}

/// Review 2, nit: a run again for a blank image that ends without an
/// answer leaves the first run's usable output standing, said so.
#[tokio::test]
async fn a_failed_run_again_keeps_the_first_run_s_output() {
    let mut first = ran(1, 0).unwrap();
    first.out = b"first".to_vec();
    let (got, kept) = settled(
        PopplerTool::Render,
        Ok(first),
        Some(Err(PdfError::Timeout { tool: "pdftoppm" })),
    )
    .await;
    let got = got.unwrap();
    assert_eq!(got.out, b"first");
    let blank = Note::Undecoded {
        tool: "pdftoppm",
        limit: Limit::Image,
        why: Blank::AgainFailed("did not finish in time"),
        span: Some((3, 3)),
    };
    assert_eq!(got.notes, std::slice::from_ref(&blank));
    assert!(blank
        .said("on page 3")
        .contains("first run's output is kept"));
    assert_eq!(kept, [blank]);
}

#[tokio::test]
async fn other_ends_and_the_wide_run_itself_are_not_run_again() {
    let timeout = Err(PdfError::Timeout { tool: "pdftotext" });
    let (got, kept) = settled(PopplerTool::Text, timeout, None).await;
    assert!(matches!(got, Err(PdfError::Timeout { .. })));
    let (got, _) = settled(PopplerTool::Text, oom(Limit::Wide), None).await;
    assert!(matches!(
        got,
        Err(PdfError::OutOfMemory {
            retry: Retry::No,
            ..
        })
    ));
    let (got, _) = settled(PopplerTool::Text, ran(0, 0), None).await;
    assert!(got.unwrap().notes.is_empty());
    assert!(kept.is_empty());
}

/// With nothing left of the file's clock the first outcome stands, said
/// so: an out-of-memory run names the deadline, a blank image its limit.
#[tokio::test]
async fn with_no_time_left_the_first_outcome_stands_and_says_why() {
    let c = clock::FileClock::new(Duration::ZERO);
    let (got, _) = clock::scope(
        c.clone(),
        settled(PopplerTool::Text, oom(Limit::Text), None),
    )
    .await;
    let e = got.unwrap_err();
    assert!(
        matches!(
            e,
            PdfError::OutOfMemory {
                retry: Retry::Deadline,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains("deadline left no time"), "{e}");
    let (got, kept) = clock::scope(c, settled(PopplerTool::Render, ran(1, 0), None)).await;
    let blank = Note::Undecoded {
        tool: "pdftoppm",
        limit: Limit::Image,
        why: Blank::Deadline,
        span: Some((3, 3)),
    };
    assert_eq!(got.unwrap().notes, std::slice::from_ref(&blank));
    assert!(blank.said("on page 3").contains("deadline left no time"));
    assert_eq!(kept, [blank]);
}
