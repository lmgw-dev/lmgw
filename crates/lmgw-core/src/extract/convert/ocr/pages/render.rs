//! A PDF page rendered for text recognition (`pdftoppm -gray`), on a slot
//! of its own charged [`pool::charge::OCR_PAGE`], with the JPEG 2000 watch
//! every image run has (review 2 of the poppler limit, S4; `pdf::wide`): a
//! scan whose JPEG 2000 image openjpeg had no memory for within
//! `OCR_RENDER_MEMORY_LIMIT_BYTES` gives its slot back and is rendered
//! once more under `POPPLER_WIDE_MEMORY_LIMIT_BYTES` on a slot charged
//! [`pool::charge::OCR_PAGE_WIDE`]; an image still blank, or a damaged one,
//! is an `image_not_decoded` warning on that page. The render's notes go to
//! the file's scope (`wide::notes`), which the conversion's warnings read.

use std::path::Path;
use std::sync::Arc;

use super::super::super::pool::{self, Slot};
use super::super::spool::Spool;
use super::super::PageError;
use super::super::MAX_OCR_PIXELS;
use crate::extract::convert::run::RunError;
use crate::extract::pdf::{self, wide, Limit, PdfError, PopplerTool};

/// Page `n` of `pdf` at `dpi` as a PGM, with the slot it was rendered on
/// (the read of the page goes on in it).
pub(super) async fn page(
    pdf: &Arc<Spool>,
    bin: &Path,
    n: u32,
    dpi: u32,
) -> Result<(Slot, Vec<u8>), PageError> {
    let (d, p) = (dpi.to_string(), n.to_string());
    let args = ["-gray", "-r", &d, "-singlefile", "-f", &p, "-l", &p].map(String::from);
    let mut call = wide::Call::new(
        PopplerTool::OcrRender,
        bin,
        args.to_vec(),
        &[],
        wide::Src::Spool(pdf),
    );
    // The PGM's header and one byte a pixel.
    call.output = MAX_OCR_PIXELS + 1024;
    call.span = Some((n, n));
    let piped = call.src.piped();
    let held = std::sync::Mutex::new(None);
    let run = |limit: Limit, timeout, kept: u64| {
        let (held, call) = (&held, &call);
        async move {
            // The first slot keeps the first run's output charged (no
            // slot) while the wide one is waited for (`wide::own`).
            let keep = lock(held).take().map(|s: Slot| s.shrink(0, kept));
            let charge = match limit {
                Limit::Wide => pool::charge::OCR_PAGE_WIDE,
                _ => pool::charge::OCR_PAGE,
            };
            let slot = pool::global()
                .acquire_charged(1, charge.saturating_add(piped).saturating_add(kept))
                .await;
            drop(keep);
            let r = wide::once(&slot, call, limit, timeout).await;
            *lock(held) = Some(slot);
            r
        }
    };
    let first = run(Limit::OcrRender, pdf::PDF_TIMEOUT, 0).await;
    let got = wide::settle(
        PopplerTool::OcrRender,
        call.span,
        first,
        pdf::PDF_TIMEOUT,
        |t, kept| run(Limit::Wide, t, kept),
    )
    .await
    .map_err(page_error)?;
    let slot = lock(&held).take().ok_or_else(|| {
        PageError::Image("lmgw lost the render's slot (a bug in lmgw)".to_string())
    })?;
    Ok((slot, got.out))
}

/// A render's error as text recognition words it: as before for a run's
/// own failures; out of memory with both limits named.
fn page_error(e: PdfError) -> PageError {
    let run = |r| PageError::Run(pdf::PDFTOPPM, r);
    match e {
        PdfError::ToolMissing => run(RunError::Missing),
        PdfError::Failed { message, .. } => run(RunError::Failed(message)),
        PdfError::Timeout { .. } => run(RunError::Timeout),
        PdfError::OutputTooLarge { .. } => run(RunError::TooLarge),
        PdfError::NoReader => run(RunError::NoReader),
        PdfError::NotStarted(m) => run(RunError::NotStarted(m)),
        e @ (PdfError::OutOfMemory { .. } | PdfError::EmptyPage(_)) => {
            PageError::Image(e.to_string())
        }
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(all(test, unix))]
mod tests;
