//! A PDF's pages cropped side by side (§14 performance round): each page's
//! render and cutting in a slot of its own, as many at once as the pool
//! admits, the results taken in page order. Measured 2026-11-19: the 74
//! picture pages of a 137-page file took 11.6 s one after the other.
//!
//! **Which crops fit the request's output stays what it was.** One after
//! the other, page `k`'s crops were cut with the budget left after the
//! pages before it (`left_k`: the reader leaves out a crop that does not fit
//! what is left of it and goes on with the next), then each reserved in
//! order. Side by side, a page is cut with what was left when it started,
//! which is at least `left_k` while nothing gives bytes back; its crops are
//! kept as they are only when the reader made every one of them and they
//! fit `left_k` whole, which is exactly when the reader, given `left_k`,
//! would have made the same. Otherwise (the request's output is running
//! out) the page is cut again, in its turn, with `left_k`, as before. So
//! the crops a request keeps depend on the document's order alone, never on
//! which page finished first.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;

use bytes::Bytes;
use futures::stream::{FuturesOrdered, StreamExt};
use lmgw_doc_reader::wire::{Answer, Op};

use super::super::super::budget::OutputBudget;
use super::super::super::ocr::spool::Spool;
use super::super::super::pool::{self, Slot};
use super::super::super::reader::{self, ReadError};
use super::super::super::run::Input;
use super::super::{held, Settings};
use super::{said, Crop, PageOut, CROP_ANSWER_BYTES};
use crate::extract::pdf::wide::{self, Call, Src, Within};
use crate::extract::pdf::PopplerTool;

/// One page to crop: its render's arguments (without the file's), the
/// render's size, the boxes in it and their captions' lengths.
pub(super) struct Job {
    pub page: u32,
    pub idx: Vec<usize>,
    pub args: Vec<String>,
    pub size: (u32, u32),
    pub boxes: Vec<[u32; 4]>,
    pub captions: Vec<usize>,
}

/// One crop the reader cut: its PNG, width and height.
type Cut = (Bytes, u32, u32);

/// What a page's render and cutting gave.
enum Got {
    /// poppler could not render it: why.
    RenderFailed(String),
    /// The reader could not cut it: why.
    CutFailed(String),
    /// Rendered; not cut, as nothing was left of the output.
    Uncut,
    /// Per box: its crop, or `None` past the budget it was cut with.
    Cut(Vec<Option<Cut>>),
}

/// Render `job`'s page of `pdf` and cut it within `budget` bytes of PNG, in
/// a slot of its own, which comes back with the result: the page's crops
/// are only held while their slot is.
async fn render_cut(
    pdf: &Spool,
    bin: &Path,
    cutter: &Path,
    job: &Job,
    budget: u64,
    bounds: Within,
) -> (Slot, Got) {
    let slot = pool::global()
        .acquire_charged(1, pool::charge::PAGE_CROPS + pool::charge::piped(pdf))
        .await;
    let got = render_cut_on(&slot, pdf, bin, cutter, job, budget, bounds).await;
    (slot, got)
}

/// [`render_cut`] on a slot the page already holds: its cut again in its
/// turn (module doc) needs no slot of its own.
async fn render_cut_on(
    slot: &Slot,
    pdf: &Spool,
    bin: &Path,
    cutter: &Path,
    job: &Job,
    budget: u64,
    bounds: Within,
) -> Got {
    // Under poppler's image limit, and once more under the wide one in this
    // slot where it needs it (`PAGE_CROPS` covers that run): a page never
    // waits for a slot while its siblings hold theirs (`crop_all`).
    let mut call = Call::new(
        PopplerTool::Render,
        bin,
        job.args.clone(),
        &[],
        Src::Spool(pdf),
    );
    call.output = bounds.output;
    call.span = Some((job.page, job.page));
    let raster = match wide::in_slot(slot, &call, bounds.timeout).await {
        Ok(r) => r.out,
        Err(e) => return Got::RenderFailed(said(&e)),
    };
    if budget == 0 {
        return Got::Uncut;
    }
    cut(slot, cutter, raster, job, budget).await
}

/// The reader's crops of `raster` (the page's render) for `job`'s boxes,
/// within `budget` bytes of PNG.
async fn cut(slot: &Slot, cutter: &Path, raster: Vec<u8>, job: &Job, budget: u64) -> Got {
    let n = job.boxes.len();
    let op = Op::Crop {
        width: job.size.0,
        height: job.size.1,
        budget,
        boxes: job.boxes.clone(),
    };
    let output = budget.saturating_add(CROP_ANSWER_BYTES);
    let input = Input::Bytes(Bytes::from(raster));
    let frame = match reader::read_with(slot, cutter, op, input, output).await {
        Ok(f) => f,
        Err(e) => return Got::CutFailed(e.said()),
    };
    let Answer::Crops(list) = &frame.answer else {
        return Got::CutFailed(reader::unexpected().said());
    };
    let lens: Vec<u64> = list.iter().flatten().map(|c| c.len).collect();
    let (Some(blobs), true) = (frame.blobs(&lens), list.len() == n) else {
        return Got::CutFailed(reader::unexpected().said());
    };
    let mut blobs = blobs.into_iter();
    Got::Cut(
        list.iter()
            .map(|c| {
                let c = (*c)?;
                blobs.next().map(|png| (png, c.width, c.height))
            })
            .collect(),
    )
}

/// Each crop of `cuts` reserved from `budget` in order, with its caption's
/// length from `captions`, as `s` says (`super::super::held`); one that
/// does not fit has no bytes.
fn take(
    cuts: Vec<Option<Cut>>,
    captions: &[usize],
    s: &Settings,
    budget: &OutputBudget,
) -> PageOut {
    let out = cuts
        .into_iter()
        .enumerate()
        .map(|(k, c)| {
            let (png, w, h) = c?;
            let held = held(png.len(), captions.get(k).copied().unwrap_or(0), s);
            budget.try_take(held).then_some((png, w, h, held))
        })
        .collect::<Vec<Option<Crop>>>();
    PageOut::Crops(out)
}

/// Every job's crops of `pdf` with the `pdftoppm` at `bin`, cut by the
/// reader at `cutter`, each reserved from `budget` (module doc); in page
/// order.
///
/// **No page waits for a slot while pages of this file hold theirs
/// unpolled** (performance round review B1): at most as many pages at once
/// as the pool admits PDF steps by its memory (`fit`, never more than its
/// size); a page cut again in its turn is cut on the slot it already
/// holds; and while that runs, the pages started after it are still
/// driven, their results kept in order. Before, the cut again waited for a
/// new slot while the pages started after it held the others without
/// being polled, which hung the request and the pool for good wherever the
/// memory admitted fewer PDF steps than the pool's size.
pub(super) async fn crop_all(
    jobs: Vec<Job>,
    pdf: &Spool,
    bin: &Path,
    cutter: &Path,
    s: &Settings,
    budget: &OutputBudget,
    bounds: Within,
) -> Vec<(Job, PageOut)> {
    let width = width(&pool::global().facts());
    let mut done = Vec::with_capacity(jobs.len());
    // Each page in a lane of the file's clock (`pool::clock`): one waiting
    // for its slot does not stop the count of one that works.
    let all = pool::clock::paused();
    let start = |job: Job| {
        pool::clock::beside(async move {
            // The budget it is cut with: the request's output left now.
            let with = budget.left();
            let (slot, got) = render_cut(pdf, bin, cutter, &job, with, bounds).await;
            Ran {
                job,
                with,
                slot,
                got,
            }
        })
    };
    let mut jobs = jobs.into_iter();
    let mut running = FuturesOrdered::new();
    let mut ahead: VecDeque<Ran> = VecDeque::new();
    loop {
        // The pages in hand (running, or done and not taken yet) stay
        // within `width`: each holds a slot, or waits for one.
        while running.len() + ahead.len() < width {
            match jobs.next() {
                Some(job) => running.push_back(start(job)),
                None => break,
            }
        }
        let ran = match ahead.pop_front() {
            Some(r) => r,
            None => match running.next().await {
                Some(r) => r,
                None => break,
            },
        };
        let Ran {
            job,
            with,
            slot,
            got,
        } = ran;
        let n = job.boxes.len();
        let left = budget.left();
        let out = match got {
            Got::RenderFailed(why) => PageOut::Failed(why),
            _ if left == 0 => PageOut::Crops(vec![None; n]),
            // Cut with what is left now: cutting again gives the same.
            Got::CutFailed(why) if with == left => PageOut::Failed(why),
            Got::Cut(cuts) if with == left || whole(&cuts, left) => {
                take(cuts, &job.captions, s, budget)
            }
            Got::Cut(_) | Got::Uncut | Got::CutFailed(_) => {
                // Cut again in its turn, with what is left now (module
                // doc), on the page's own slot, the pages after it driven
                // meanwhile.
                let again = render_cut_on(&slot, pdf, bin, cutter, &job, left, bounds);
                let again = drive(pool::clock::beside(again), &mut running, &mut ahead).await;
                match again {
                    Got::RenderFailed(why) | Got::CutFailed(why) => PageOut::Failed(why),
                    Got::Uncut => PageOut::Crops(vec![None; n]),
                    Got::Cut(cuts) => take(cuts, &job.captions, s, budget),
                }
            }
        };
        drop(slot);
        done.push((job, out));
    }
    drop(all);
    done
}

/// One page's render and cut, with the budget it was cut with and its slot.
struct Ran {
    job: Job,
    with: u64,
    slot: Slot,
    got: Got,
}

/// How many pages are cropped at once: as many PDF steps as one caller's
/// share of the pool admits (`facts`' `per_caller`: its slots, and its
/// memory with one PDF step left free), so no page in hand waits for a slot
/// its own caller's share keeps from it.
fn width(facts: &lmgw_api_types::documents::PoolFacts) -> usize {
    let c = facts.derived_from.charges.page_crops;
    let fit = c.fit.min(c.per_caller).min(facts.slots);
    usize::try_from(fit).unwrap_or(1).max(1)
}

/// `again` to its end, the pages of `running` driven meanwhile and their
/// results kept in `ahead`, in order: none of them stops while it runs (a
/// tool whose output is not read stalls, and its clock goes on).
async fn drive<F, R>(again: F, running: &mut FuturesOrdered<R>, ahead: &mut VecDeque<Ran>) -> Got
where
    F: Future<Output = Got>,
    R: Future<Output = Ran>,
{
    let mut again = std::pin::pin!(again);
    loop {
        tokio::select! {
            biased;
            got = &mut again => return got,
            Some(r) = running.next(), if !running.is_empty() => ahead.push_back(r),
        }
    }
}

/// Every box was cut and all of them fit `left`.
fn whole(cuts: &[Option<Cut>], left: u64) -> bool {
    let mut sum = 0u64;
    for c in cuts {
        let Some((png, _, _)) = c else {
            return false;
        };
        sum = sum.saturating_add(png.len() as u64);
    }
    sum <= left
}

/// Every job named without its crops: lmgw's reader is not there.
pub(super) fn no_cutter(jobs: Vec<Job>) -> Vec<(Job, PageOut)> {
    let why = ReadError::Missing.said();
    jobs.into_iter()
        .map(|j| (j, PageOut::Failed(why.clone())))
        .collect()
}
