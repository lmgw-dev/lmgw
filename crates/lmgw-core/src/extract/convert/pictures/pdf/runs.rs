//! A PDF's picture runs (design §5.3): `pdftohtml -xml -dataurls` over the
//! pages the [`gate`] picked, in ranges ([`gate::ranges`]), each range in a
//! slot of its own beside the file's text runs, which never wait for them.
//! The runs go through the tool pool like every step (fair per caller), and
//! no more of them are started at once than the pool had free slots when
//! the file's pictures were looked for (at least one).
//!
//! A range whose run fails, passes `PDF_TIMEOUT` or `MAX_UNCOMPRESSED_BYTES`
//! loses its own pictures only, named in a `pictures_partial` note
//! ([`Gone`]); the text and its fonts come from the file's own runs. A run
//! that needed more than poppler's image limit, or could not decode a JPEG
//! 2000 image within it, gives its slot back and is run again under the
//! wide limit on a new one (`pdf::wide`, said in the file's
//! `memory_limit_widened` note): the runs beside it hold no finished slot
//! while it waits (each range's images are read and its slot given back
//! before the next is taken in).

use std::path::PathBuf;

use std::sync::Arc;

use futures::stream::{self, StreamExt};

use super::super::super::ocr::spool::Spool;

use super::super::super::pages::PageSet;
use super::super::super::pool;
use super::gate::{self, Gated};
use super::{images, Gone, Images};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::pdf::wide::{self, Within};
use crate::extract::pdf::{geometry, Limit, PopplerTool, PDF_TIMEOUT};

/// The poppler tools the pictures need.
#[derive(Debug, Clone)]
pub struct Bins {
    pub html: PathBuf,
    pub info: PathBuf,
    pub list: PathBuf,
}

/// One picture run's time and output (its memory is poppler's image
/// limit, then the wide one).
pub fn bounds() -> Within {
    Within {
        timeout: PDF_TIMEOUT,
        output: MAX_UNCOMPRESSED_BYTES,
    }
}

/// The images of `src`'s pages in `span` that `pages` picks, as far as
/// they may be exported (module doc), with what the gate left out.
pub async fn find(
    bins: &Bins,
    src: Arc<Spool>,
    span: Option<(u32, u32)>,
    pages: Option<PageSet>,
    min_area: f64,
) -> Images {
    find_within(bins, src, span, pages, min_area, bounds()).await
}

/// [`find`] with each picture run held to `run_bounds` (a test's tight
/// ones).
pub async fn find_within(
    bins: &Bins,
    src: Arc<Spool>,
    span: Option<(u32, u32)>,
    pages: Option<PageSet>,
    min_area: f64,
    run_bounds: Within,
) -> Images {
    let gated = gate::gate(&bins.info, &bins.list, &src, span, pages, min_area).await;
    let facts = pool::global().facts();
    let mut out = Images::default();
    let ranges: Vec<Option<(u32, u32)>> = match gated {
        Gated::Pages(g) => {
            out.left = g.left;
            out.left_pages = g.left_pages;
            let slots = usize::try_from(facts.slots).unwrap_or(1);
            gate::ranges(&g.pages, slots)
                .into_iter()
                .map(Some)
                .collect()
        }
        Gated::Unsure => vec![span],
    };
    if ranges.is_empty() {
        return out;
    }
    let free = facts.slots.saturating_sub(facts.running).max(1);
    let free = usize::try_from(free).unwrap_or(1);
    let html = &bins.html;
    // Each run in a lane of the file's clock (`pool::clock`): one waiting
    // for its slot does not stop the count of one that works.
    let all = pool::clock::paused();
    let done: Vec<Images> = stream::iter(ranges)
        .map(|r| {
            let src = src.clone();
            pool::clock::beside(async move {
                let run = Box::pin(one(html, src, r, run_bounds));
                // Once the page-reading model's stage has taken the file's
                // end over, its deadline stops this run too.
                match pool::clock::current() {
                    None => run.await,
                    Some(c) => tokio::select! {
                        found = run => found,
                        () = c.handed_expiry() => cut(r, c.limit()),
                    },
                }
            })
        })
        .buffer_unordered(free)
        .collect()
        .await;
    drop(all);
    for d in done {
        out.pages.extend(d.pages);
        out.gone.extend(d.gone);
    }
    out.pages.sort_by_key(|p| p.number);
    out.gone.sort_by_key(|g| g.range);
    out
}

/// A range whose run the file's deadline stopped: its pages have no
/// pictures, said as a failed run's are.
fn cut(range: Option<(u32, u32)>, limit: std::time::Duration) -> Images {
    Images {
        gone: vec![Gone {
            range,
            why: format!(
                "the caller's document_timeout of {} s passed while the page-reading model \
                 read the file's pages, and the run was stopped",
                limit.as_secs_f64()
            ),
            partial: false,
        }],
        ..Default::default()
    }
}

/// One range's run and the reading of its output, in one slot: once more
/// under the wide limit on a new slot where it needs it (module doc).
async fn one(
    html: &std::path::Path,
    src: Arc<Spool>,
    range: Option<(u32, u32)>,
    run_bounds: Within,
) -> Images {
    let gone = |why: String, partial: bool| Images {
        gone: vec![Gone {
            range,
            why,
            partial,
        }],
        ..Default::default()
    };
    // The slot of the try that ran last, for the reading of its output.
    let held = std::sync::Mutex::new(None);
    let run = |limit: Limit, timeout, kept: u64| {
        let (held, src) = (&held, &src);
        async move {
            // The first try's slot back before the wide one is waited for,
            // still charged the first run's output where that is kept (no
            // slot; `wide::own`).
            let keep = lock(held).take().map(|s: pool::Slot| s.shrink(0, kept));
            let charge = pool::charge::picture_run(limit) + pool::charge::piped(src) + kept;
            let slot = pool::global().acquire_charged(1, charge).await;
            drop(keep);
            let within = Within {
                timeout,
                ..run_bounds
            };
            let r = geometry::image_runs_at(&slot, html, src, range, limit, within).await;
            *lock(held) = Some(slot);
            r
        }
    };
    let first = run(Limit::Image, run_bounds.timeout, 0).await;
    let settled = wide::settle(
        PopplerTool::Pictures,
        range,
        first,
        run_bounds.timeout,
        |t, kept| run(Limit::Wide, t, kept),
    )
    .await;
    let slot = lock(&held).take();
    let (xml, Some(slot)) = (settled, slot) else {
        return gone("lmgw lost the run's slot".into(), false);
    };
    let xml = match xml {
        Ok(got) => got.out,
        Err(e) => return gone(e.to_string(), false),
    };
    // The run has ended: the slot keeps what its reading still holds
    // (`pool::charge::after_runs`, at most 3 GiB: within the 4 GiB of
    // `PICTURE_RUN`), not poppler's limit.
    let slot = slot.shrink(1, pool::charge::after_runs(&[&xml]));
    match pool::blocking_on(slot, move || images(&xml)).await {
        Ok(mut got) => {
            if let Some(why) = got.error.take() {
                got.gone.push(Gone {
                    range,
                    why,
                    partial: true,
                });
            }
            got
        }
        Err(_) => gone(
            "lmgw's reading of its output stopped unexpectedly".into(),
            false,
        ),
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
