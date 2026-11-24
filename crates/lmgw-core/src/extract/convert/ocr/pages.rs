//! Where text recognition is called from (document-conversion design §4.2,
//! §7.1, WP4): a PDF's pages ([`pdf_pages`], the one hook in `file/pdf.rs`)
//! and an image file ([`image_file`], the `ocr.tesseract` handler).
//!
//! `ocr` decides the pages: `auto` reads the pages without a text layer,
//! `force` every page (a page whose recognition fails keeps its text
//! layer), `off` none. Pages go through the tool pool in page order, at
//! most as many at once as the pool has slots, each holding its slot only
//! for its own render and read (module doc of [`super`]). A file with more
//! than one page to read is spooled once ([`Spool`]); each page's tool
//! reads its part of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use lmgw_api_types::documents::convert::{ConvertWarning, ErrorCode, Ocr, WarningCode};
use lmgw_api_types::documents::InstallPlatform;
use lmgw_doc_reader::wire::{Answer, ImagePage, Op};

use super::super::caps::ToolId;
use super::super::doc::{Doc, Part};
use super::super::options::Plan;
use super::super::pages::page_ranges;
use super::super::pool;
use super::super::reader::{self, ReadError};
use super::super::report;
use super::super::run::{Input, RunError};
use super::super::tools::{Fact, ToolFacts};
use super::super::watch::Watched;
use super::image::{self, PageImage};
use super::spool::{Shared, Spool};
use super::{
    decoded, langs, read_page, tesseract, warning, PageError, PageRead, Setup, Turned,
    MAX_OCR_PIXELS, OCR_DPI, OCR_LOW_CONFIDENCE,
};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::pdf;

/// What recognition did to a PDF's pages.
#[derive(Debug, Default)]
pub struct PdfOcr {
    /// It took charge of the pages without a text layer: each is read or
    /// named in [`PdfOcr::warnings`]. When false, the PDF reader names them
    /// itself (`ocr: off`, tesseract missing).
    pub handled: bool,
    /// Pages read by recognition.
    pub read: u32,
    /// tesseract ran.
    pub ran: bool,
    pub warnings: Vec<ConvertWarning>,
    /// The pages recognition read poorly: their words' mean confidence is
    /// below [`OCR_LOW_CONFIDENCE`], or their lines read as vertical and no
    /// turn read better (the pages of its `ocr_low_confidence` warnings).
    /// What the page-reading model is asked about (`model_help::select`).
    pub poor: std::collections::BTreeSet<u32>,
    /// The pages it read without finding a word: nothing was read there.
    pub empty: std::collections::BTreeSet<u32>,
    /// Its per-page outcomes, written into [`PdfOcr::warnings`] by
    /// [`PdfOcr::finish`] once a later reader may have read some of the
    /// pages again.
    tally: Option<(Tally, bool, &'static str, InstallPlatform)>,
}

impl PdfOcr {
    /// Write the per-page warnings, leaving out the pages in `read_again`
    /// (the page-reading model read them after recognition: what
    /// tesseract made of them is no longer in the output).
    pub fn finish(&mut self, read_again: &std::collections::BTreeSet<u32>) {
        if let Some((mut tally, osd, kept, platform)) = self.tally.take() {
            tally.forget(read_again);
            self.warnings.extend(tally.warnings(osd, kept, platform));
        }
    }
}

/// The `pdftoppm` beside `pdftotext`, when the probe found one.
fn pdftoppm(facts: &ToolFacts) -> Option<PathBuf> {
    let Some(Fact::Found(f)) = facts.fact(ToolId::Poppler) else {
        return None;
    };
    f.watched.iter().find_map(|w| match w {
        Watched::Binary { name, at: Some(at) } if name == pdf::PDFTOPPM => Some(at.path.clone()),
        _ => None,
    })
}

/// The resolution a page of `size` points is rendered at: [`OCR_DPI`], or
/// less when that would pass [`MAX_OCR_PIXELS`], with the warning's words.
fn dpi_for(size: Option<(f32, f32)>) -> (u32, Option<String>) {
    let Some((w, h)) = size.filter(|(w, h)| *w > 0.0 && *h > 0.0) else {
        return (OCR_DPI, None);
    };
    let (w, h) = (f64::from(w), f64::from(h));
    let px = |dpi: f64| (w / 72.0 * dpi) * (h / 72.0 * dpi);
    let at = px(f64::from(OCR_DPI));
    if at <= MAX_OCR_PIXELS as f64 {
        return (OCR_DPI, None);
    }
    let dpi = (72.0 * (MAX_OCR_PIXELS as f64 / (w * h)).sqrt())
        .floor()
        .max(1.0) as u32;
    let said = format!(
        "it is {:.0} × {:.0} pt; at OCR_DPI ({OCR_DPI}) it would be {:.0} million pixels, past \
         MAX_OCR_PIXELS ({} million), so it was rendered at {dpi} DPI and small text may be \
         missed",
        w,
        h,
        at / 1e6,
        MAX_OCR_PIXELS / 1_000_000
    );
    (dpi, Some(said))
}

/// The outcomes of a file's pages, gathered into its warnings.
#[derive(Debug, Default)]
struct Tally {
    read: u32,
    low: Vec<u32>,
    /// Pages turned before they were read, by how and by how far.
    turned: BTreeMap<(bool, u32), Vec<u32>>,
    /// Pages whose lines read as vertical and that no turn read better.
    sideways: Vec<u32>,
    failed: BTreeMap<String, Vec<u32>>,
    reduced: Vec<(u32, String)>,
}

impl Tally {
    /// Leave `pages` out of every outcome.
    fn forget(&mut self, pages: &std::collections::BTreeSet<u32>) {
        if pages.is_empty() {
            return;
        }
        let keep = |n: &u32| !pages.contains(n);
        self.low.retain(keep);
        self.sideways.retain(keep);
        self.reduced.retain(|(n, _)| keep(n));
        for list in self.turned.values_mut().chain(self.failed.values_mut()) {
            list.retain(keep);
        }
        self.turned.retain(|_, l| !l.is_empty());
        self.failed.retain(|_, l| !l.is_empty());
    }

    fn page(&mut self, n: u32, r: &Result<PageRead, PageError>) {
        match r {
            Ok(p) => {
                self.read += 1;
                if p.confidence.is_some_and(|c| c < OCR_LOW_CONFIDENCE) {
                    self.low.push(n);
                }
                if let Some((degrees, how)) = p.turned {
                    self.turned
                        .entry((how == Turned::Guessed, degrees))
                        .or_default()
                        .push(n);
                }
                if p.sideways {
                    self.sideways.push(n);
                }
            }
            Err(e) => self.failed.entry(e.said()).or_default().push(n),
        }
    }

    fn warnings(self, osd: bool, kept: &str, platform: InstallPlatform) -> Vec<ConvertWarning> {
        let mut out = Vec::new();
        for (n, why) in self.reduced {
            out.push(warning(
                WarningCode::OcrPageReduced,
                format!("page {n}: {why}"),
                vec![n],
            ));
        }
        for (why, pages) in self.failed {
            out.push(warning(
                WarningCode::PartFailed,
                format!(
                    "text recognition failed on {} {}: {why}{kept}",
                    if pages.len() == 1 { "page" } else { "pages" },
                    page_ranges(&pages)
                ),
                pages,
            ));
        }
        for ((guessed, degrees), pages) in self.turned {
            let (which, were) = if pages.len() == 1 {
                ("page", "was")
            } else {
                ("pages", "were")
            };
            let why = if guessed {
                format!(
                    "the lines read as vertical, which a page scanned on its side gives, and \
                     the turn that read well was kept; tesseract's orientation data would tell \
                     this from the page itself ({})",
                    langs::osd_hint(platform)
                )
            } else {
                "tesseract's orientation data said so".to_string()
            };
            out.push(warning(
                WarningCode::OcrPageTurned,
                format!(
                    "{which} {} {were} turned {degrees}° clockwise before the text was \
                     recognised: {why}",
                    page_ranges(&pages)
                ),
                pages,
            ));
        }
        if !self.sideways.is_empty() {
            out.push(warning(
                WarningCode::OcrLowConfidence,
                format!(
                    "the lines tesseract recognised on {} {} read as vertical, and the page \
                     turned either way did not read better, so the text may be wrong: a page \
                     of vertical text, or one scanned on its side that is hard to read{}",
                    if self.sideways.len() == 1 {
                        "page"
                    } else {
                        "pages"
                    },
                    page_ranges(&self.sideways),
                    if osd {
                        String::new()
                    } else {
                        format!(
                            " (tesseract's orientation data is not installed: {})",
                            langs::osd_hint(platform)
                        )
                    }
                ),
                self.sideways,
            ));
        }
        if !self.low.is_empty() {
            let turned = if osd {
                "a poor scan, handwriting or a picture of text".to_string()
            } else {
                format!(
                    "a page scanned upside down (tesseract's orientation data, which turns \
                     such pages upright, is not installed: {}) or a poor scan",
                    langs::osd_hint(platform)
                )
            };
            out.push(warning(
                WarningCode::OcrLowConfidence,
                format!(
                    "tesseract was unsure of the text it recognised on {} {} (its words' mean \
                     confidence is below OCR_LOW_CONFIDENCE, {OCR_LOW_CONFIDENCE} of 100), so it \
                     may be wrong; the likely cause is {turned}",
                    if self.low.len() == 1 { "page" } else { "pages" },
                    page_ranges(&self.low)
                ),
                self.low,
            ));
        }
        out
    }
}

/// The languages and the binary, or why recognition cannot run.
fn setup(plan: &Plan, facts: &ToolFacts) -> Result<(Setup, Vec<ConvertWarning>), String> {
    let Some(bin) = tesseract(facts) else {
        return Err(format!(
            "tesseract is not installed ({})",
            report::hint(super::super::caps::tool(ToolId::Tesseract), plan.platform).text
        ));
    };
    // Resolved by the request's first file that needs it, read by the rest.
    let (l, notes) = plan
        .ocr_chosen
        .get(plan.ocr_languages.as_deref(), facts, plan.platform)
        .clone()?;
    let tessdata = match facts.fact(ToolId::Tesseract) {
        Some(Fact::Found(f)) => super::super::confine::verdict::tessdata_of(f),
        _ => None,
    };
    let rules = super::super::confine::tesseract(&bin, tessdata.as_deref()).map(Arc::new);
    Ok((
        Setup {
            tesseract: bin,
            langs: l,
            osd: langs::osd_installed(facts),
            reader: reader::path(facts),
            rules,
        },
        notes,
    ))
}

/// One PDF page rendered and read, on a slot of its own. The PDF is the
/// renderer's stdin: the spool file, which poppler opens and reads the
/// page's objects from by position, or (no spool) a pipe carrying all of it.
/// The render watches for JPEG 2000 images like every image run
/// ([`render`]).
async fn pdf_page(
    pdf: Arc<Spool>,
    bin: PathBuf,
    setup: Setup,
    n: u32,
    dpi: u32,
) -> Result<PageRead, PageError> {
    let (slot, pgm) = render::page(&pdf, &bin, n, dpi).await?;
    let img = image::parse_pgm(pgm).map_err(PageError::Image)?;
    read_page(slot, &setup, img, Some(dpi), n).await
}

/// Text recognition on the PDF pages of `parts` that `plan.ocr` asks for
/// (module doc), each read page replacing its part.
pub async fn pdf_pages(
    spooled: &Shared,
    parts: &mut [Part],
    plan: &Plan,
    facts: &ToolFacts,
) -> PdfOcr {
    let targets: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| match plan.ocr {
            Ocr::Off => false,
            Ocr::Auto => p.source.is_none(),
            Ocr::Force => true,
        })
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return PdfOcr::default();
    }
    let force = plan.ocr == Ocr::Force;
    let pages: Vec<u32> = targets.iter().map(|i| parts[*i].number).collect();
    // Without tesseract, `auto` leaves the pages to the PDF reader's own
    // warning (the table's row names the package); `force` says what it
    // could not do.
    if tesseract(facts).is_none() && !force {
        return PdfOcr::default();
    }
    let (setup, warnings) = match (setup(plan, facts), pdftoppm(facts)) {
        (Ok(s), Some(_)) => s,
        (Err(why), _) => {
            let w = warning(
                if tesseract(facts).is_some() {
                    WarningCode::OcrLanguageMissing
                } else {
                    WarningCode::ToolMissing
                },
                format!(
                    "{} {} {} not recognised: {why}{}",
                    if pages.len() == 1 { "page" } else { "pages" },
                    page_ranges(&pages),
                    if pages.len() == 1 { "was" } else { "were" },
                    if force {
                        "; pages with a text layer kept it"
                    } else {
                        ""
                    }
                ),
                pages,
            );
            return PdfOcr {
                handled: true,
                warnings: vec![w],
                ..Default::default()
            };
        }
        (Ok(_), None) => {
            let mut w = warning(
                WarningCode::ToolMissing,
                format!(
                    "pages {} were not recognised: pdftoppm (part of poppler-utils), which \
                     renders a page for text recognition, was not found",
                    page_ranges(&pages)
                ),
                pages,
            );
            w.tool = Some(ToolId::Poppler.as_str().to_string());
            return PdfOcr {
                handled: true,
                warnings: vec![w],
                ..Default::default()
            };
        }
    };
    let bin = pdftoppm(facts).unwrap_or_else(|| PathBuf::from(pdf::PDFTOPPM));
    let width = pool::global().facts().slots.max(1) as usize;
    let mut tally = Tally::default();
    let specs: Vec<_> = targets
        .iter()
        .map(|&i| (i, parts[i].number, parts[i].size))
        .collect();
    // Several pages: the PDF goes to disk once, not to a pipe once a page.
    let spooled = spooled.get().await;
    let jobs = specs.into_iter().map(|(i, n, size)| {
        let (dpi, reduced) = dpi_for(size);
        let (pdf, bin, setup) = (spooled.clone(), bin.clone(), setup.clone());
        // Each page in a lane of the file's clock (`pool::clock`): a page
        // waiting for its slot does not stop the count of one that works.
        pool::clock::beside(async move { (i, n, reduced, pdf_page(pdf, bin, setup, n, dpi).await) })
    });
    let mut results = futures::stream::iter(jobs).buffered(width);
    // The lane that drives the pages waits while they run.
    let waits = pool::clock::paused();
    let (mut poor, mut empty) = (Default::default(), Default::default());
    while let Some((i, n, reduced, r)) = results.next().await {
        tally.page(n, &r);
        if let Some(why) = reduced {
            tally.reduced.push((n, why));
        }
        if let Ok(p) = r {
            judge(&p, n, &mut poor, &mut empty);
            parts[i] = p.part;
        }
    }
    drop(waits);
    let read = tally.read;
    let kept = if force {
        "; those pages kept their text layer where they had one"
    } else {
        ""
    };
    PdfOcr {
        handled: true,
        read,
        ran: true,
        warnings,
        poor,
        empty,
        tally: Some((tally, setup.osd, kept, plan.platform)),
    }
}

/// Whether recognition read page `n` poorly, or found no word on it
/// ([`PdfOcr::poor`], [`PdfOcr::empty`]).
fn judge(
    p: &PageRead,
    n: u32,
    poor: &mut std::collections::BTreeSet<u32>,
    empty: &mut std::collections::BTreeSet<u32>,
) {
    match p.confidence {
        None => {
            empty.insert(n);
        }
        Some(c) if c < OCR_LOW_CONFIDENCE || p.sideways => {
            poor.insert(n);
        }
        Some(_) => {}
    }
}

/// What an image file gave.
#[derive(Debug)]
pub struct ImageRead {
    pub doc: Doc,
    pub read: u32,
    pub warnings: Vec<ConvertWarning>,
    /// Pages were left out.
    pub partial: bool,
}

/// One page of an image file, on a slot of its own: a picture as its own
/// bytes, a TIFF page decoded by lmgw's reader from the spooled file.
async fn image_page(
    file: Arc<Spool>,
    bytes: Bytes,
    setup: Setup,
    page: ImagePage,
    n: u32,
) -> Result<PageRead, PageError> {
    image::within(u64::from(page.width), u64::from(page.height)).map_err(PageError::Image)?;
    // Decoded by lmgw's reader first: its run's charge covers the read.
    let slot = pool::global()
        .acquire_charged(1, pool::charge::READER_RUN)
        .await;
    let (img, dpi) = match page.ifd {
        None => (PageImage::file(bytes).map_err(PageError::Image)?, None),
        Some(ifd) => {
            let Some(bin) = &setup.reader else {
                return Err(PageError::Reader(ReadError::Missing));
            };
            decoded(&slot, bin, file.input(), Some(ifd)).await?
        }
    };
    read_page(slot, &setup, img, dpi, n).await
}

/// An image file (PNG, JPEG, GIF, WebP, TIFF) read by tesseract, a TIFF
/// page by page. Its pages are listed from their headers by lmgw's reader
/// in one run (a TIFF's chain of directories walked once); the error names
/// the tool it is about.
pub async fn image_file(
    bytes: Bytes,
    plan: &Plan,
    facts: &ToolFacts,
) -> Result<ImageRead, (ErrorCode, String, ToolId)> {
    if plan.ocr == Ocr::Off {
        return Err((
            ErrorCode::NoReader,
            "ocr is off, and a picture has no text without text recognition; send ocr auto or \
             force to read it"
                .to_string(),
            ToolId::Tesseract,
        ));
    }
    let (setup, mut warnings) =
        setup(plan, facts).map_err(|why| (ErrorCode::ToolMissing, why, ToolId::Tesseract))?;
    let by_reader = |e: ReadError| {
        let (code, why) = e.failure();
        (code, why, ToolId::Reader)
    };
    let listed = reader::read(
        facts,
        Op::ImageInfo,
        Input::Bytes(bytes.clone()),
        MAX_UNCOMPRESSED_BYTES,
    )
    .await
    .map_err(by_reader)?;
    let Answer::Images(pages) = listed.answer else {
        return Err(by_reader(reader::unexpected()));
    };
    let count = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    // Several pages: the file goes to disk once, not to a pipe once a page.
    let dir = plan.spool_dir.as_deref().filter(|_| pages.len() > 1);
    let spooled = Arc::new(Spool::of(dir, bytes.clone()).await);
    let width = pool::global().facts().slots.max(1) as usize;
    let jobs = pages.into_iter().zip(1u32..).map(|(page, n)| {
        let (file, bytes, setup) = (spooled.clone(), bytes.clone(), setup.clone());
        // A lane of the file's clock per page, as a PDF's pages have.
        pool::clock::beside(image_page(file, bytes, setup, page, n))
    });
    let mut results = futures::stream::iter(jobs).buffered(width);
    let waits = pool::clock::paused();
    let mut tally = Tally::default();
    let mut doc = Doc::default();
    let mut first_error = None;
    let mut n = 0u32;
    while let Some(r) = results.next().await {
        n += 1;
        tally.page(n, &r);
        match r {
            Ok(p) => doc.parts.push(p.part),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    drop(waits);
    let read = tally.read;
    if read == 0 {
        return Err(match first_error {
            Some(PageError::Reader(e)) => by_reader(e),
            Some(e @ PageError::Run(_, RunError::NotStarted(_))) => {
                (ErrorCode::ToolNotStarted, e.said(), ToolId::Tesseract)
            }
            Some(e) => (
                ErrorCode::Unreadable,
                format!("text recognition failed: {}", e.said()),
                ToolId::Tesseract,
            ),
            None => (
                ErrorCode::Unreadable,
                "text recognition failed: nothing was recognised".to_string(),
                ToolId::Tesseract,
            ),
        });
    }
    let partial = read < count;
    warnings.extend(tally.warnings(setup.osd, "", plan.platform));
    Ok(ImageRead {
        doc,
        read,
        warnings,
        partial,
    })
}

mod render;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absurd_media_box_is_rendered_at_a_lower_resolution_that_fits() {
        assert_eq!(dpi_for(Some((595.0, 842.0))), (300, None));
        assert_eq!(dpi_for(None), (300, None));
        assert_eq!(dpi_for(Some((0.0, 842.0))), (300, None));
        let (dpi, said) = dpi_for(Some((14_400.0, 14_400.0)));
        let px = (14_400.0 / 72.0 * f64::from(dpi)).powi(2);
        assert!(px <= MAX_OCR_PIXELS as f64, "{dpi} {px}");
        assert!(dpi >= 49, "{dpi}");
        let said = said.unwrap();
        assert!(
            said.contains("MAX_OCR_PIXELS") && said.contains(&format!("{dpi} DPI")),
            "{said}"
        );
        // A page past every bound still gets a resolution of at least 1.
        assert_eq!(dpi_for(Some((f32::MAX, f32::MAX))).0, 1);
    }
}
