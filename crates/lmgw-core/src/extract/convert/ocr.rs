//! Text recognition (document-conversion design §5.1, tier 0, WP4): pages
//! without a text layer, every page with `ocr: force`, and image files, read
//! by tesseract. Its hOCR (words and lines with boxes and confidences)
//! becomes blocks of the structure model ([`super::doc`]) through the same
//! layout pass a PDF's text layer takes ([`super::pdf_layout`]), with
//! source `ocr`, so headings, columns, tables and the three outputs apply.
//!
//! One page is one step on the tool pool: its slot is taken, the page is
//! rendered (`pdftoppm -gray`, a PGM on stdout) or decoded (a TIFF page, by
//! lmgw's reader executable, [`super::reader`]), turned upright ([`osd`]
//! when tesseract's orientation data is installed, else by the shape of its
//! lines, [`read_page`]), read by tesseract (PGM or the picture's own bytes
//! on stdin, hOCR on stdout), laid out, and the slot is given back. The
//! slot is the page's own from start to end ([`read_page`] takes it): each
//! child holds a lease of it until it has been reaped, and the gateway's own
//! work on the page (turning its pixels, reading the hOCR) runs on a
//! blocking thread that holds it, so a conversion that is dropped (the
//! client went, `document_timeout`, a cancelled job) never frees a slot
//! whose work still runs.
//!
//! **What a page costs in memory.** In the gateway, one byte a pixel (the
//! PGM a child wrote, shared with tesseract's stdin, not copied), two while
//! a page is turned; at most [`MAX_OCR_PIXELS`] pixels. In the reader child
//! that decodes a TIFF page, up to ten bytes a pixel (four 16-bit samples,
//! the grey copy, the PGM), under its own memory limit. An image file is
//! never decoded in the gateway.
//!
//! A file's pages go through the pool in page order, at most as many at
//! once as the pool has slots ([`pages`]), so a 500-page scan holds one slot
//! per page in flight, never one for its whole length, and other callers'
//! steps get their turns (round robin, `pool.rs`). A page of a large file
//! is not sent that file once per page: the file is written once to an
//! unlinked spool file under the data dir ([`spool`]) and each page's tool
//! reads what it needs of it by position.
//!
//! - [`langs`]: which languages tesseract reads, judged when the request is
//!   planned and resolved once per request;
//! - [`osd`]: page orientation (`--psm 0`, needs `osd.traineddata`);
//! - [`hocr`]: the hOCR read, bounded;
//! - [`blocks`]: hOCR lines into the layout pass, as blocks;
//! - [`image`]: a page's pixels as tesseract reads them, a PGM's header;
//! - [`spool`]: a file on disk for tools that read parts of it;
//! - [`pages`]: the PDF hook (`file/pdf.rs`) and the image-file handler.

use std::path::PathBuf;
use std::time::Duration;

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};

use lmgw_doc_reader::wire::{Answer, Op};

use super::caps::ToolId;
use super::doc::Part;
use super::pool::{self, Slot};
use super::reader::{self, ReadError};
use super::run::{self, Bounds, Input, RunError};
use super::tools::{Fact, ToolFacts};

pub mod blocks;
pub mod hocr;
pub mod image;
pub mod langs;
pub mod osd;
pub mod pages;
pub mod spool;

/// The resolution a PDF page is rendered at for tesseract: 300 DPI, the
/// resolution tesseract's documentation asks for ("at least 300 dpi") and
/// the one WP0a measured it at (§14: 0.637 3-gram Jaccard on rendered
/// text PDFs, 1.0 on the upright scan, 2.28 s a page). An A4 page is 2481 ×
/// 3509 pixels, 8.3 MiB of grey.
pub const OCR_DPI: u32 = 300;

/// The most pixels one page may have: 100 million (an A1 sheet at 300 DPI
/// is 70 million). tesseract held 336 MiB for a 60-million-pixel page
/// (measured 2026-11-18, tesseract 5.5.4); what the gateway and the reader
/// child hold for one is in the module doc. A PDF page larger than this is
/// rendered at the resolution that fits and the page's warning says so; an
/// image or a TIFF page larger than this is not read, with a warning naming
/// it, so a 30,000 × 30,000 pixel image is refused by its header, before any
/// pixel is decoded.
pub const MAX_OCR_PIXELS: u64 = 100_000_000;

/// How long one tesseract run may take once it has its slot: 120 s. A
/// rendered A4 page took 0.4 to 2.3 s single-threaded in WP0a and a
/// 60-million-pixel page 4.9 s; the bound is for a dense poster-sized page
/// on a slow machine, and its warning names it.
pub const TESSERACT_TIMEOUT: Duration = Duration::from_secs(120);

/// tesseract's address space (`RLIMIT_AS`): 2 GiB, about six times what it
/// held for a 60-million-pixel page (336 MiB resident, measured
/// 2026-11-18), for its models (`eng` and `deu` are 15 MiB each, mapped)
/// and a page at [`MAX_OCR_PIXELS`]. Part of the pool's charge for a page
/// (`pool::charge::OCR_PAGE`).
pub const TESSERACT_MEMORY_LIMIT_BYTES: u64 = 2 << 30;

/// The address space of `pdftoppm` rendering one page for text
/// recognition (`RLIMIT_AS`): 2 GiB, as tesseract's. Lowered from poppler's
/// 4 GiB in the performance round so the tool pool charges a page what it
/// can use (`pool::charge::OCR_PAGE`), on measurements of 2026-11-19
/// (poppler 26.08): one page at 150 to 300 dpi peaks at 27 to 54 MiB
/// resident, and a page rendered at the largest size recognition asks for
/// ([`MAX_OCR_PIXELS`], 100 million grey pixels) renders within 384 MiB of
/// address space. A page past it fails alone, its warning naming this
/// constant.
pub const OCR_RENDER_MEMORY_LIMIT_BYTES: u64 = 2 << 30;

/// The most hOCR one page may give: 64 MiB, about 400,000 words at the
/// 160 bytes a word tesseract writes; the output budget a run is stopped
/// at.
pub const HOCR_OUTPUT_BYTES: u64 = 64 << 20;

/// A page whose words tesseract is on average less sure of than this (its
/// `x_wconf`, 0 to 100) gets an `ocr_low_confidence` warning: 60. Measured
/// on the WP0a corpus' scans (2026-11-18): upright pages 89 to 96, the same
/// pages turned 90° or 180° and read without orientation 27 to 33.
pub const OCR_LOW_CONFIDENCE: f32 = 60.0;

/// tesseract's environment: one thread a run, so a run is one slot's worth
/// of CPU (design §7.7).
const TESSERACT_ENV: &[(&str, &str)] = &[("OMP_THREAD_LIMIT", "1")];

/// What a run of text recognition needs: the binary, the languages,
/// whether the orientation data is installed, and lmgw's reader (which
/// decodes a picture that has to be turned).
#[derive(Debug, Clone)]
pub struct Setup {
    pub tesseract: PathBuf,
    /// `eng+deu`.
    pub langs: String,
    pub osd: bool,
    pub reader: Option<PathBuf>,
    /// What tesseract may reach where it runs confined
    /// (`confine::tesseract`, its language data's directory from the probe).
    /// `None`: it runs with its limits alone (the reason is in the log and
    /// on Settings › Documents).
    pub rules: Option<std::sync::Arc<lmgw_doc_reader::sandbox::Confine>>,
}

/// The tesseract the snapshot found.
pub fn tesseract(facts: &ToolFacts) -> Option<PathBuf> {
    match facts.fact(ToolId::Tesseract) {
        Some(Fact::Found(f)) => Some(f.located.path.clone()),
        _ => None,
    }
}

/// Why a page was turned before it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turned {
    /// tesseract's orientation data said so.
    Detected,
    /// Its lines read as vertical, and turned it read well: a guess from
    /// the lines' shape, made where the orientation data is not installed.
    Guessed,
}

/// What one page gave.
#[derive(Debug)]
pub struct PageRead {
    /// The page as a part (source `ocr`), numbered by the caller.
    pub part: Part,
    /// Its words' mean confidence, `None` without words.
    pub confidence: Option<f32>,
    /// It was turned by this many degrees clockwise before it was read, and
    /// why.
    pub turned: Option<(u32, Turned)>,
    /// Its lines read as vertical and no turn read better: it may be on its
    /// side.
    pub sideways: bool,
}

/// How one page failed.
#[derive(Debug, Clone, PartialEq)]
pub enum PageError {
    /// A tool run failed: its message, and the tool's name.
    Run(&'static str, RunError),
    /// lmgw's reader could not give the page's pixels.
    Reader(ReadError),
    /// The image could not be read or was past a bound: why, in words.
    Image(String),
}

impl PageError {
    /// In words, for a `part_failed` warning.
    pub fn said(&self) -> String {
        match self {
            Self::Run(tool, RunError::Missing) => format!("{tool} was not found"),
            Self::Run(_, RunError::NoReader) => super::run::start::no_reader(),
            Self::Run(_, RunError::NotStarted(m)) => m.clone(),
            Self::Run(tool, RunError::Timeout) => {
                let bound = if *tool == "tesseract" {
                    format!("TESSERACT_TIMEOUT ({} s)", TESSERACT_TIMEOUT.as_secs())
                } else {
                    format!(
                        "PDF_TIMEOUT ({} s)",
                        crate::extract::pdf::PDF_TIMEOUT.as_secs()
                    )
                };
                format!("{tool} did not finish within {bound} and was stopped")
            }
            Self::Run(tool, RunError::TooLarge | RunError::FileTooLarge(_)) => {
                format!("{tool}'s output passed its bound and the run was stopped")
            }
            Self::Run(tool, RunError::OutOfMemory) => {
                let limit = if *tool == "tesseract" {
                    format!(
                        "TESSERACT_MEMORY_LIMIT_BYTES ({})",
                        crate::hf::fmt_bytes(TESSERACT_MEMORY_LIMIT_BYTES)
                    )
                } else {
                    format!(
                        "OCR_RENDER_MEMORY_LIMIT_BYTES ({})",
                        crate::hf::fmt_bytes(OCR_RENDER_MEMORY_LIMIT_BYTES)
                    )
                };
                format!("{tool} ran out of its memory limit {limit}")
            }
            Self::Run(tool, RunError::Failed(m)) => format!("{tool} failed: {m}"),
            Self::Reader(e) => e.said(),
            Self::Image(why) => why.clone(),
        }
    }
}

/// The gateway's own work on a page stopped (a bug): the slot went back as
/// the thread unwound.
fn stopped(what: &str, e: tokio::task::JoinError) -> PageError {
    tracing::error!("text recognition stopped while {what}: {e}");
    PageError::Image(format!(
        "lmgw stopped unexpectedly while {what} (a bug in lmgw)"
    ))
}

/// What tesseract made of a page.
struct Recognised {
    part: Part,
    confidence: Option<f32>,
    /// Its lines read as vertical ([`hocr::Page::sideways`]).
    sideways: bool,
}

/// `page` read by tesseract and laid out, on `slot`, which comes back.
async fn recognise(
    slot: Slot,
    setup: &Setup,
    page: &image::PageImage,
    dpi: Option<u32>,
    number: u32,
) -> Result<(Slot, Recognised), PageError> {
    let dpi_s = dpi.map(|d| d.to_string());
    let mut args: Vec<&str> = vec!["stdin", "stdout"];
    if let Some(d) = &dpi_s {
        args.extend(["--dpi", d]);
    }
    args.extend(["-l", &setup.langs, "-c", "tessedit_create_hocr=1"]);
    let bounds = Bounds {
        timeout: TESSERACT_TIMEOUT,
        output: HOCR_OUTPUT_BYTES,
        memory: TESSERACT_MEMORY_LIMIT_BYTES,
    };
    let hocr = run::tesseract_held(
        &slot,
        &setup.tesseract,
        &args,
        TESSERACT_ENV,
        setup.rules.as_deref(),
        page.bytes(),
        bounds,
    )
    .await
    .map_err(|e| PageError::Run("tesseract", e))?;
    // The hOCR's read and the layout: the slot is the thread's meanwhile.
    let (slot, read) = pool::blocking_keep(slot, move || {
        let page = hocr::parse(&hocr);
        blocks::part(&page, dpi, number).map(|part| Recognised {
            part,
            confidence: page.confidence(),
            sideways: page.sideways(),
        })
    })
    .await
    .map_err(|e| stopped("reading the recognised text", e))?;
    Ok((slot, read.map_err(PageError::Image)?))
}

/// `page` turned `degrees` clockwise, on `slot`, which comes back. A
/// picture's own bytes are decoded to grey by lmgw's reader first; the
/// pixels are turned here, on a blocking thread that holds the slot.
async fn turn(
    slot: Slot,
    setup: &Setup,
    page: image::PageImage,
    degrees: u32,
) -> Result<(Slot, image::PageImage), PageError> {
    let page = match page {
        image::PageImage::File(bytes) => {
            let Some(bin) = &setup.reader else {
                return Err(PageError::Reader(ReadError::Missing));
            };
            decoded(&slot, bin, Input::Bytes(bytes), None).await?.0
        }
        pgm => pgm,
    };
    let (slot, turned) = pool::blocking_keep(slot, move || page.rotated(degrees))
        .await
        .map_err(|e| stopped("turning a page", e))?;
    Ok((slot, turned.map_err(PageError::Image)?))
}

/// A picture, or the TIFF page whose directory is at `ifd`, as grey pixels
/// with its resolution, decoded by lmgw's reader on `slot`.
pub async fn decoded(
    slot: &Slot,
    reader_bin: &std::path::Path,
    input: Input,
    ifd: Option<u64>,
) -> Result<(image::PageImage, Option<u32>), PageError> {
    let op = Op::ImagePage {
        max_pixels: MAX_OCR_PIXELS,
        ifd,
    };
    // The PGM's header, one byte a pixel, and the frame around them.
    let bound = MAX_OCR_PIXELS + 4096;
    let frame = reader::read_with(slot, reader_bin, op, input, bound)
        .await
        .map_err(PageError::Reader)?;
    let Answer::Pixels(px) = frame.answer else {
        return Err(PageError::Reader(reader::unexpected()));
    };
    let pgm = frame
        .rest()
        .ok_or_else(|| PageError::Reader(reader::unexpected()))?;
    let page = image::parse_pgm(pgm).map_err(PageError::Image)?;
    Ok((page, px.dpi))
}

/// Read one page's pixels (a PGM) or an image file's own bytes with
/// tesseract, on `slot`, the page's own until it is read (module doc).
///
/// Turned upright first when the orientation data is installed and says so.
/// Without that data, a page whose lines read as vertical
/// ([`hocr::Page::sideways`]) is turned a quarter turn and read again:
/// 270° clockwise first when tesseract was sure of the vertical lines (it
/// read them the right way, so the page lies on its right side), 90° first
/// when it was not; then the other way if that reading is still vertical
/// or tesseract is unsure of it ([`OCR_LOW_CONFIDENCE`]). The first turn
/// that reads as lines across with confidence is kept, and when neither
/// does, the first reading is, marked [`PageRead::sideways`]. That costs a
/// sideways page one more read (two if the first turn was the wrong one);
/// an upright page none.
///
/// `dpi` is the render's resolution (a PDF page); an image file's own
/// resolution, or tesseract's estimate, applies without it.
pub async fn read_page(
    slot: Slot,
    setup: &Setup,
    page: image::PageImage,
    dpi: Option<u32>,
    number: u32,
) -> Result<PageRead, PageError> {
    let (mut slot, mut page) = (slot, page);
    let mut turned = None;
    if setup.osd {
        if let Some(rotate) = osd::detect(&slot, setup, page.bytes()).await {
            (slot, page) = turn(slot, setup, page, rotate).await?;
            turned = Some((rotate, Turned::Detected));
        }
    }
    let (slot, first) = recognise(slot, setup, &page, dpi, number).await?;
    let done = |r: Recognised, turned, sideways| PageRead {
        part: r.part,
        confidence: r.confidence,
        turned,
        sideways,
    };
    if setup.osd || !first.sideways {
        return Ok(done(first, turned, false));
    }
    let sure = first.confidence.is_some_and(|c| c >= OCR_LOW_CONFIDENCE);
    let try_first = if sure { 270 } else { 90 };
    // On its side, as far as its lines tell: the likelier turn, then the
    // other. A turn or a read that fails leaves the first reading.
    let mut slot = slot;
    for degrees in [try_first, 360 - try_first] {
        let Ok((s, img)) = turn(slot, setup, page.clone(), degrees).await else {
            break;
        };
        let Ok((s, r)) = recognise(s, setup, &img, dpi, number).await else {
            break;
        };
        slot = s;
        if !r.sideways && r.confidence.is_some_and(|c| c >= OCR_LOW_CONFIDENCE) {
            return Ok(done(r, Some((degrees, Turned::Guessed)), false));
        }
    }
    Ok(done(first, None, true))
}

/// A warning about text recognition, with tesseract named.
pub fn warning(code: WarningCode, message: String, parts: Vec<u32>) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: Some(ToolId::Tesseract.as_str().to_string()),
        parts,
        option: None,
    }
}
