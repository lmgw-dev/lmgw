//! A PDF's two geometry runs (`pdftotext -bbox-layout`, `pdftohtml -xml`)
//! in the file's slot (§14 performance round): side by side when two slots
//! are free now, one after the other in one slot otherwise. Measured
//! 2026-11-19 (poppler 26.08, release build): each run takes about the
//! same time on one core (1.04 and 1.01 s on a 715-page file), so side by
//! side the file's runs take half as long while the pool has cores to
//! spare; a file never waits for a second slot.
//!
//! Side by side, `pdftohtml` is stopped (its process group killed, its
//! slot kept until it is reaped) as soon as `-bbox-layout` fails, and its
//! output is never used then (one after the other it is not run then): the
//! answer is the same either way, and the file fails as soon as on its own
//! (performance round review S3).
//!
//! Once the runs have ended the file's slot is charged what it still holds
//! ([`after`]), no longer poppler's limit: shrunk, or grown where the
//! outputs need more than the runs' charge (two of about 1 GiB need up to
//! 6 GiB to be read), the pool has it free now and nobody waits for it
//! (`pool::fit`). Where not, the file gives its slot and the outputs back
//! and runs again, one run after the other, on a slot charged what the
//! outputs showed, taken in the queue like any step, within what the
//! file's clock has left: no file waits for memory while it holds a slot.
//!
//! A run that ran out of memory under poppler's text limit is run again
//! under the wide one (`pdf::wide`): the file gives everything back, takes
//! a slot charged [`pool::charge::PDF_FILE_WIDE`] and runs both again, one
//! after the other; a run that fails there too is refused by that limit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::super::ocr::spool::Spool;
use super::super::super::pool::{self, Slot};
use super::Fonts;
use crate::extract::pdf::wide::{self, Note, Ran as Out, Why};
use crate::extract::pdf::{self, geometry, Limit, PdfError, Retry};

/// What the runs gave, with the slot (one or two) the file goes on in.
pub(super) struct Ran {
    pub slot: Slot,
    pub bbox: Result<Vec<u8>, PdfError>,
    pub fonts: Fonts,
}

/// The runs' outcomes before they are said: the font run's `None` when it
/// was not run (no `pdftohtml`, or the bbox run failed).
struct Got {
    slot: Slot,
    bbox: Result<Out, PdfError>,
    fonts: Option<Result<Out, PdfError>>,
}

fn fonts_of(r: Option<Result<Out, PdfError>>) -> Fonts {
    match r {
        None | Some(Err(PdfError::ToolMissing)) => Fonts::Missing,
        Some(Ok(xml)) => Fonts::Read(xml.out),
        Some(Err(e)) => Fonts::Failed(e.to_string()),
    }
}

/// The run that ran out of memory under its own limit, if one did.
fn out_of_memory(got: &Got) -> Option<&'static str> {
    let oom = |r: &Result<Out, PdfError>| match r {
        Err(PdfError::OutOfMemory { tool, limit, .. }) if *limit != Limit::Wide => Some(*tool),
        _ => None,
    };
    oom(&got.bbox).or_else(|| got.fonts.as_ref().and_then(oom))
}

/// Both runs of `src` over `span` (module doc).
pub(super) async fn geometry(
    src: &Spool,
    text_bin: &Path,
    html: Option<&PathBuf>,
    span: Option<(u32, u32)>,
) -> Ran {
    let mut limit = Limit::Text;
    // What the outputs of an earlier try showed they need to be read.
    let mut floor = 0;
    let mut timeout = pdf::PDF_TIMEOUT;
    let mut widened = None;
    loop {
        let got = runs(src, text_bin, html, span, (limit, floor, timeout)).await;
        if limit != Limit::Wide {
            if let Some(tool) = out_of_memory(&got) {
                match wide::again_within(pdf::PDF_TIMEOUT) {
                    Some(t) => {
                        tracing::info!(
                            "poppler: {tool} needed more than {} for a PDF's layout; both runs \
                             again under {}",
                            limit.name(),
                            Limit::Wide.name()
                        );
                        widened = Some(Note::Widened {
                            tool,
                            from: limit,
                            why: Why::Memory,
                            span: None,
                        });
                        (limit, timeout) = (Limit::Wide, t);
                        // Everything back before the wide slot is waited for.
                        drop(got);
                        continue;
                    }
                    None => return said(deadline(got), None),
                }
            }
        }
        let (got, need) = after(got);
        match got {
            Ok(got) => return said(got, widened),
            Err(got) if floor == 0 => {
                // The runs again get their time within the file's clock,
                // as a run again under the wide limit does.
                let Some(t) = wide::again_within(pdf::PDF_TIMEOUT) else {
                    tracing::warn!(
                        "a PDF's layout outputs need {} to be read, more than the {} its slot \
                         holds and the pool has free now; the file's deadline left no time to \
                         run them again on a slot charged that, so they are read on this one",
                        crate::hf::fmt_bytes(need),
                        crate::hf::fmt_bytes(got.slot.bytes())
                    );
                    return said(got, widened);
                };
                tracing::info!(
                    "a PDF's layout outputs need {} to be read, more than the {} its slot \
                     holds and the pool has free now: its runs again on a slot charged that",
                    crate::hf::fmt_bytes(need),
                    crate::hf::fmt_bytes(got.slot.bytes())
                );
                (floor, timeout) = (need, t);
                drop(got);
            }
            Err(got) => {
                tracing::warn!(
                    "a PDF's layout outputs need {} to be read, more than the {} its slot \
                     was charged after they showed it",
                    crate::hf::fmt_bytes(need),
                    crate::hf::fmt_bytes(got.slot.bytes())
                );
                return said(got, widened);
            }
        }
    }
}

/// One try at both runs under `limit`: side by side when two slots are
/// free now (a first try under the text limit only), one after the other
/// in one slot charged [`pool::charge::pdf_file`] or `floor`, whichever is
/// more, otherwise.
async fn runs(
    src: &Spool,
    text_bin: &Path,
    html: Option<&PathBuf>,
    span: Option<(u32, u32)>,
    (limit, floor, timeout): (Limit, u64, Duration),
) -> Got {
    let piped = pool::charge::piped(src);
    if let (Some(html), Limit::Text, 0) = (html, limit, floor) {
        let two = 2 * (pool::charge::POPPLER_TEXT_RUN + piped);
        if let Some(slot) = pool::global().try_acquire_charged(2, two) {
            let (bbox, fonts) = side_by_side(
                geometry::bbox_layout_at(&slot, text_bin, src, span, limit, timeout),
                geometry::font_runs_at(&slot, html, src, span, limit, timeout),
            )
            .await;
            return Got { slot, bbox, fonts };
        }
    }
    // One slot for the file: both runs and the layout.
    let charge = pool::charge::pdf_file(limit).saturating_add(piped);
    let slot = pool::global().acquire_charged(1, charge.max(floor)).await;
    let bbox = geometry::bbox_layout_at(&slot, text_bin, src, span, limit, timeout).await;
    let fonts = match (&bbox, html) {
        (Err(_), _) | (_, None) => None,
        (Ok(_), Some(bin)) => {
            Some(geometry::font_runs_at(&slot, bin, src, span, limit, timeout).await)
        }
    };
    Got { slot, bbox, fonts }
}

/// The file's slot once its runs have ended (module doc): one slot charged
/// what it still holds, the runs' outputs and their reading
/// (`pool::charge::after_runs`), so the next PDF is admitted while the
/// layout is read; `Err` when that is more than it holds and the pool has
/// not got it free now. With what they need. Nothing in this slot runs
/// poppler again: the pages read after the layout (text recognition, the
/// page-reading model) take slots of their own.
fn after(got: Got) -> (Result<Got, Got>, u64) {
    let empty = Vec::new();
    let bbox = got.bbox.as_ref().map_or(&empty, |r| &r.out);
    let fonts = match &got.fonts {
        Some(Ok(r)) => &r.out,
        _ => &empty,
    };
    let need = pool::charge::after_runs(&[bbox, fonts]);
    let Got { slot, bbox, fonts } = got;
    let fitted = match slot.fit(need) {
        Ok(slot) => Ok(Got { slot, bbox, fonts }),
        Err(slot) => Err(Got { slot, bbox, fonts }),
    };
    (fitted, need)
}

/// `got`'s run that ran out of memory, said not run again: the file's
/// deadline left no time.
fn deadline(got: Got) -> Got {
    let late = |r: Result<Out, PdfError>| match r {
        Err(PdfError::OutOfMemory {
            tool, limit, piped, ..
        }) => Err(PdfError::OutOfMemory {
            tool,
            limit,
            piped,
            retry: Retry::Deadline,
        }),
        r => r,
    };
    let (got, _) = after(Got {
        bbox: late(got.bbox),
        fonts: got.fonts.map(late),
        slot: got.slot,
    });
    got.unwrap_or_else(|g| g)
}

/// `got` as the handler reads it, with `widened` recorded for the file
/// and a failure that ran out of memory again said so.
fn said(got: Got, widened: Option<Note>) -> Ran {
    let from = match &widened {
        Some(Note::Widened { from, .. }) => Some(*from),
        _ => None,
    };
    let again = |r: Result<Out, PdfError>| match (r, from) {
        (
            Err(PdfError::OutOfMemory {
                tool, limit, piped, ..
            }),
            Some(from),
        ) => Err(PdfError::OutOfMemory {
            tool,
            limit,
            piped,
            retry: Retry::Widened(from),
        }),
        (r, _) => r,
    };
    let bbox = again(got.bbox);
    let fonts = got.fonts.map(again);
    if bbox.is_ok() {
        if let Some(n) = widened {
            wide::notes::record(n);
        }
    }
    Ran {
        slot: got.slot,
        bbox: bbox.map(|r| r.out),
        fonts: fonts_of(fonts),
    }
}

/// The bbox run and the font run side by side; the font run dropped (its
/// tool stopped) as soon as the bbox run fails (module doc).
async fn side_by_side<B, F>(
    bbox: B,
    fonts: F,
) -> (Result<Out, PdfError>, Option<Result<Out, PdfError>>)
where
    B: std::future::Future<Output = Result<Out, PdfError>>,
    F: std::future::Future<Output = Result<Out, PdfError>>,
{
    use futures::future::{select, Either};
    let (bbox, fonts) = (std::pin::pin!(bbox), std::pin::pin!(fonts));
    match select(bbox, fonts).await {
        Either::Left((Err(e), _fonts)) => (Err(e), None),
        Either::Left((Ok(xml), fonts)) => (Ok(xml), Some(fonts.await)),
        Either::Right((fonts, bbox)) => match bbox.await {
            Ok(xml) => (Ok(xml), Some(fonts)),
            Err(e) => (Err(e), None),
        },
    }
}

#[cfg(all(test, unix))]
mod tests;
