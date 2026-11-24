//! The model stage of a PDF ([`stage`], the parent module's doc): which
//! pages to read, their pictures rendered in batches into a private
//! folder, each read back and given to the session, within the stage's
//! bounds.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::ocr::spool::Spool;

use super::super::caps::ToolId;
use super::super::pages::PageSet;
use super::super::pool::{self, clock};
use super::super::tools::{Fact, ToolFacts};
use super::super::watch::Watched;
use super::{files, prep, session, PageRegions, Regions, SIDE};
use crate::extract::pdf::{self, square, PdfError};

/// What one page's picture takes: [`SIDE`] × [`SIDE`] pixels of three
/// bytes (and a header of a few bytes), 1.17 MiB.
pub const PAGE_PICTURE_BYTES: u64 = SIDE as u64 * SIDE as u64 * 3;

/// The most the private folder holds at once: 16 pages' pictures, 18.75
/// MiB. The system's temporary folder may be memory.
pub const RENDER_BATCH_BYTES: u64 = 16 * PAGE_PICTURE_BYTES;

/// The pages one `pdftoppm` run renders: [`RENDER_BATCH_BYTES`] over
/// [`PAGE_PICTURE_BYTES`].
pub const RENDER_BATCH_PAGES: u32 = (RENDER_BATCH_BYTES / PAGE_PICTURE_BYTES) as u32;

/// The share of the caller's `document_timeout` the model stage may use;
/// the rest is the geometry's (parent module doc).
pub const MODEL_DEADLINE_SHARE: f64 = 0.5;

/// The `pdftotext` the probe found, and the `pdftoppm` beside it.
fn poppler(facts: &ToolFacts) -> Option<(PathBuf, Option<PathBuf>)> {
    let Some(Fact::Found(f)) = facts.fact(ToolId::Poppler) else {
        return None;
    };
    let ppm = f.watched.iter().find_map(|w| match w {
        Watched::Binary { name, at: Some(at) } if name == pdf::PDFTOPPM => Some(at.path.clone()),
        _ => None,
    });
    Some((f.located.path.clone(), ppm))
}

/// `picked` (ascending) as runs of neighbouring pages, none longer than
/// `most`: first and last page of each.
pub(super) fn batches(picked: &[u32], most: u32) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &n in picked {
        match out.last_mut() {
            Some((a, b)) if n == b.saturating_add(1) && n - *a < most => *b = n,
            _ => out.push((n, n)),
        }
    }
    out
}

/// Why the stage ended before the pages it did not read.
fn budget_spent() -> String {
    format!(
        "the page pictures for this file took PDF_TIMEOUT ({} s) together, so the model \
         stopped before these pages",
        pdf::PDF_TIMEOUT.as_secs()
    )
}

fn deadline_reached(limit: Duration) -> String {
    format!(
        "the file had used MODEL_DEADLINE_SHARE ({:.0} %) of the caller's document_timeout of \
         {} s, so the model stopped before these pages and left them to the rest of the time",
        MODEL_DEADLINE_SHARE * 100.0,
        limit.as_secs_f64()
    )
}

/// Page `n`'s picture from the folder, read back and deleted, as the
/// model's input; `run_failed` is why the run that should have written it
/// failed, if it did.
fn picture(
    files: &std::collections::BTreeMap<u32, PathBuf>,
    n: u32,
    run_failed: Option<&str>,
) -> Result<Vec<u8>, String> {
    let Some(p) = files.get(&n) else {
        return Err(run_failed.map_or_else(
            || "pdftoppm wrote no picture of it".to_string(),
            str::to_string,
        ));
    };
    let read = square::open_rendered(p, SIDE);
    let _ = std::fs::remove_file(p);
    let ppm = read.map_err(|e| format!("its picture could not be read back ({})", e.kind()))?;
    prep::input(&ppm)
}

/// The model stage of the PDF spooled in `src` (parent module doc): `None` when
/// the model is not chosen, or poppler cannot say what pages the file has
/// (the geometry runs then say why). `pages` is the request's.
pub async fn stage(src: &Spool, pages: Option<&PageSet>, facts: &ToolFacts) -> Option<Regions> {
    #[cfg(test)]
    if let Ok(r) = super::RECORDED.try_with(Clone::clone) {
        return Some(r);
    }
    let choice = super::choice();
    if !choice.on {
        return None;
    }
    let path = match files::usable(&choice.models_dir) {
        Ok(p) => p,
        Err(why) => return Some(Regions::unusable(why)),
    };
    let (text_bin, ppm_bin) = poppler(facts)?;
    let Some(ppm_bin) = ppm_bin else {
        return Some(Regions::unusable(
            "pdftoppm (part of poppler-utils) was not found, so no page could be rendered for it"
                .into(),
        ));
    };
    // Which pages of the request's span there are and which have words:
    // only those are rendered.
    let span = pages.and_then(PageSet::span);
    let first = span.map_or(1, |s| s.0);
    let has = square::pages_with_text_with(&text_bin, src, span, pdf::PDF_TIMEOUT)
        .await
        .ok()?;
    let last = first
        .saturating_add(u32::try_from(has.len()).unwrap_or(u32::MAX))
        .saturating_sub(1);
    let has_text = |n: &u32| {
        n.checked_sub(first)
            .and_then(|i| has.get(i as usize))
            .copied()
            .unwrap_or(false)
    };
    let picked: Vec<u32> = match pages {
        Some(set) => set.pick(last).pages,
        None => (first..=last).collect(),
    };
    let picked: Vec<u32> = picked.into_iter().filter(has_text).collect();
    drop(has);
    let mut out = Regions::default();
    if picked.is_empty() {
        return Some(out);
    }
    let dir = match src.private_dir(crate::extract::convert::libreoffice::scratch::LAYOUT_PREFIX) {
        Ok(d) => d,
        Err(e) => {
            return Some(Regions::unusable(format!(
                "no private folder for the page pictures could be made ({})",
                e.kind()
            )))
        }
    };
    let infer = |input| session::infer(&path, input);
    let stopped = read_pages(
        &picked,
        src,
        &ppm_bin,
        dir.path(),
        pdf::PDF_TIMEOUT,
        infer,
        &mut out,
    )
    .await;
    if let Some(why) = stopped {
        for n in picked {
            out.pages
                .entry(n)
                .or_insert_with(|| PageRegions::Failed(why.clone()));
        }
    }
    Some(out)
}

/// The batches of `picked` rendered into `dir` within `budget` together
/// and read through `infer`, into `out`; why the stage ended early, when
/// it did (the pages without an entry then were not read).
pub(super) async fn read_pages<F, Fut>(
    picked: &[u32],
    src: &Spool,
    ppm_bin: &Path,
    dir: &Path,
    budget: Duration,
    mut infer: F,
    out: &mut Regions,
) -> Option<String>
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<super::Region>, session::InferError>>,
{
    let clock = clock::current();
    let stop_at = clock
        .as_ref()
        .map(|c| (c.limit(), c.limit().mul_f64(MODEL_DEADLINE_SHARE)));
    // The renders' time left, of the budget for all of them.
    let mut left = budget;
    // The time left before the stage's share of the file's clock is used.
    let share_left = || match (&clock, stop_at) {
        (Some(c), Some((_, stop))) => Some(stop.saturating_sub(c.used())),
        _ => None,
    };
    let deadline = || deadline_reached(stop_at.map_or(Duration::ZERO, |(limit, _)| limit));
    for (a, b) in batches(picked, RENDER_BATCH_PAGES) {
        let by_share = share_left().is_some_and(|s| s < left);
        let timeout = share_left().map_or(left, |s| s.min(left));
        if timeout.is_zero() {
            return Some(if by_share { deadline() } else { budget_spent() });
        }
        let (ran, took) = squares(src, ppm_bin, (a, b), dir, timeout, share_left).await;
        left = left.saturating_sub(took);
        let run_failed = match ran {
            Ok(_) => None,
            Err(PdfError::Timeout { .. }) => {
                return Some(if by_share { deadline() } else { budget_spent() });
            }
            Err(e) => Some(e.to_string()),
        };
        let files = square::rendered(dir).unwrap_or_default();
        for n in a..=b {
            if share_left().is_some_and(|s| s.is_zero()) {
                return Some(deadline());
            }
            let input = match picture(&files, n, run_failed.as_deref()) {
                Ok(i) => i,
                Err(why) => {
                    out.pages.insert(n, PageRegions::Failed(why));
                    continue;
                }
            };
            match infer(input).await {
                Ok(regions) => {
                    out.pages.insert(n, PageRegions::Found(regions));
                }
                Err(session::InferError::Page(why)) => {
                    out.pages.insert(n, PageRegions::Failed(why));
                }
                // The file or the session: no later page would fare better.
                Err(session::InferError::Unusable(why)) => {
                    out.unusable = Some(why);
                    return None;
                }
            }
        }
    }
    None
}

/// Pages `a..=b` rendered to squares in `dir` on a slot of their own (the
/// image limit and a render's output), once more under the wide limit on a
/// new one where they need it (`pdf::wide`), within `timeout` and, for the
/// run again, what is left of it and of the stage's share (`share_left`).
/// The time the runs took, their waits for slots not counted.
async fn squares(
    src: &Spool,
    ppm_bin: &Path,
    span: (u32, u32),
    dir: &Path,
    timeout: Duration,
    share_left: impl Fn() -> Option<Duration>,
) -> (Result<pdf::wide::Got, PdfError>, Duration) {
    let call = square::square_call(ppm_bin, src, span, SIDE, dir);
    let took = std::sync::Mutex::new(Duration::ZERO);
    let ran = |limit: pdf::Limit, t: Duration| {
        let (call, took) = (&call, &took);
        async move {
            let slot = pool::global().acquire_charged(1, call.charge(limit)).await;
            let started = Instant::now();
            let r = pdf::wide::once(&slot, call, limit, t).await;
            *took.lock().unwrap_or_else(|e| e.into_inner()) += started.elapsed();
            r
        }
    };
    let first = ran(pdf::Limit::Image, timeout).await;
    let spent = *took.lock().unwrap_or_else(|e| e.into_inner());
    let again = timeout.saturating_sub(spent);
    let again = share_left().map_or(again, |s| s.min(again));
    // The squares are files in `dir`, not stdout: nothing is kept outside
    // a slot. A run again that ends early leaves the pages it rewrote whole
    // and the one it was writing cut, which its read fails by name.
    let got = pdf::wide::settle(
        pdf::PopplerTool::Render,
        Some(span),
        first,
        again,
        |t, _| ran(pdf::Limit::Wide, t),
    )
    .await;
    let took = *took.lock().unwrap_or_else(|e| e.into_inner());
    (got, took)
}

#[cfg(test)]
mod tests;
