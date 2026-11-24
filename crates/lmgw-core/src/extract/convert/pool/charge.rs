//! What a step of the tool pool is charged (document-conversion design
//! §7.7, §14 performance round): the most memory it can use, from the
//! constants that already bound it, so the pool admits steps while the sum
//! of their charges fits the memory it was derived from ([`super`]). Each
//! figure is a bound, never an estimate: a tool's address-space limit
//! (`RLIMIT_AS`, which it cannot pass) plus the outputs the gateway holds
//! for it, each bounded by its run's output bound.
//!
//! | step | charge | from |
//! |---|---|---|
//! | [`PDF_FILE`] | 2.5 GiB | poppler's limit for the runs that need no image's pixels (`POPPLER_TEXT_MEMORY_LIMIT_BYTES`) and the two geometry outputs a PDF holds while its layout is read; also the picture gate (two runs' outputs) |
//! | [`POPPLER_TEXT_RUN`] | 1.5 GiB | `POPPLER_TEXT_MEMORY_LIMIT_BYTES` and one output: a PDF's text for Chat and knowledge bases, the words the layout model looks for, each of the layout's two runs side by side |
//! | [`POPPLER_IMAGE_RUN`] | 3 GiB | `POPPLER_IMAGE_MEMORY_LIMIT_BYTES` and one output: a page rendered for the page-reading or the layout model or for Chat |
//! | [`PICTURE_RUN`] | 4 GiB | `POPPLER_IMAGE_MEMORY_LIMIT_BYTES`, a picture run's output and the images read from it |
//! | [`PAGE_CROPS`] | 6 GiB | a page's crops: the reader's limit (`READER_MEMORY_LIMIT_BYTES`), the page's render and its crops, or the render's own run again under the wide limit, whichever is more |
//! | [`POPPLER_WIDE_RUN`], [`PDF_FILE_WIDE`], [`picture_run`] | 5, 6, 6 GiB | a poppler step run again under `POPPLER_WIDE_MEMORY_LIMIT_BYTES` (`pdf::wide`) after its own limit was not enough, on a slot of its own taken once the first was given back |
//! | [`READER_RUN`] | 5 GiB | the reader's limit and its answer (`MAX_UNCOMPRESSED_BYTES`) |
//! | [`OCR_PAGE`] | 2 GiB + 2 × the largest page + the hOCR | the page's render (`OCR_RENDER_MEMORY_LIMIT_BYTES`), its PGM and a turned copy held while tesseract (`TESSERACT_MEMORY_LIMIT_BYTES`) reads it, and its hOCR |
//! | [`OCR_PAGE_WIDE`] | 4 GiB + the same | that page with its render run again under `POPPLER_WIDE_MEMORY_LIMIT_BYTES` (a JPEG 2000 scan openjpeg had no memory for), on a slot of its own taken once the first was given back |
//! | [`HEAVIEST`] | the largest of them | every step that names no charge of its own (in-process readers, pandoc, ffmpeg, LibreOffice: each limit is at most 4 GiB, the reader's, and their outputs at most `MAX_UNCOMPRESSED_BYTES` and the two a PDF holds) |
//! | [`piped`] | + `POPPLER_PIPE_READ_PER_BYTE` × the file | a poppler step whose file is sent through a pipe (not spooled), which poppler reads whole into memory: added to its charge, as its limit grows by it |
//! | [`after_runs`] | the outputs' buffers + [`READ_PER_OUTPUT_BYTE`] × their bytes | a step whose tool runs have ended and that only reads their outputs in-process now (a PDF's layout, a picture run's images): its slot is fitted to this (`Slot::fit`: shrunk, or grown when the outputs need more than the runs' charge and the pool has it free), so the next PDF is admitted while it reads |
//!
//! poppler's limits are two since 2026-11-20 (design §14 "Poppler limit
//! from measured peaks"): a run that needs no image's pixels needs far less
//! than one that decodes images; before, one 4 GiB limit made a PDF 6 GiB.
//!
//! [`PDF_FILE`] stays what a PDF is admitted with: until its runs have ended
//! poppler's limit applies. After them nothing in its slot runs poppler
//! again, and what it holds is known exactly (PDF charge after the runs,
//! 2026-11-20). Two outputs of about 1 GiB need more to be read than
//! `PDF_FILE` (up to 6 GiB): the slot is grown to it where the pool has it
//! free, and otherwise the file gives its slot and outputs back and runs
//! again on a slot charged what the outputs showed (`file/pdf/runs.rs`), so
//! no step ever waits for memory while it holds a slot.
//!
//! A step heavier than all the memory still runs alone (the pool admits
//! one step whatever its charge), as a pool of one did before.

use crate::extract::convert::ocr::spool::Spool;
use crate::extract::convert::ocr::{
    HOCR_OUTPUT_BYTES, MAX_OCR_PIXELS, OCR_RENDER_MEMORY_LIMIT_BYTES, TESSERACT_MEMORY_LIMIT_BYTES,
};
use crate::extract::convert::reader::READER_MEMORY_LIMIT_BYTES;
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;
use crate::extract::pdf::{
    Limit, POPPLER_IMAGE_MEMORY_LIMIT_BYTES, POPPLER_PIPE_READ_PER_BYTE,
    POPPLER_TEXT_MEMORY_LIMIT_BYTES, POPPLER_WIDE_MEMORY_LIMIT_BYTES,
};

/// A PDF's own runs and its layout (module doc): 2.5 GiB.
pub const PDF_FILE: u64 = POPPLER_TEXT_MEMORY_LIMIT_BYTES + 2 * MAX_UNCOMPRESSED_BYTES;

/// [`PDF_FILE`] with its runs under `limit`: run again under the wide one
/// (`pdf::wide`), 6 GiB, the figure every PDF had before.
pub fn pdf_file(limit: Limit) -> u64 {
    limit.bytes().saturating_add(2 * MAX_UNCOMPRESSED_BYTES)
}

/// A PDF's runs again under the wide limit: 6 GiB.
pub const PDF_FILE_WIDE: u64 = POPPLER_WIDE_MEMORY_LIMIT_BYTES + 2 * MAX_UNCOMPRESSED_BYTES;

/// One poppler run that needs no image's pixels and its output: 1.5 GiB.
pub const POPPLER_TEXT_RUN: u64 = POPPLER_TEXT_MEMORY_LIMIT_BYTES + MAX_UNCOMPRESSED_BYTES;

/// One poppler run that decodes images and its output: 3 GiB.
pub const POPPLER_IMAGE_RUN: u64 = POPPLER_IMAGE_MEMORY_LIMIT_BYTES + MAX_UNCOMPRESSED_BYTES;

/// One picture run (`pdftohtml -dataurls`), its output and the images
/// read from it: 4 GiB.
pub const PICTURE_RUN: u64 = POPPLER_IMAGE_MEMORY_LIMIT_BYTES + 2 * MAX_UNCOMPRESSED_BYTES;

/// [`PICTURE_RUN`] under `limit` (6 GiB under the wide one).
pub fn picture_run(limit: Limit) -> u64 {
    limit.bytes().saturating_add(2 * MAX_UNCOMPRESSED_BYTES)
}

/// One poppler run under the wide limit and its output: 5 GiB (a run of
/// its own run again, `pdf::wide::own`).
pub const POPPLER_WIDE_RUN: u64 = POPPLER_WIDE_MEMORY_LIMIT_BYTES + MAX_UNCOMPRESSED_BYTES;

/// One run of lmgw's reader and its answer: 5 GiB.
pub const READER_RUN: u64 = READER_MEMORY_LIMIT_BYTES + MAX_UNCOMPRESSED_BYTES;

/// One page's crops (module doc): the page's render (its run and the
/// raster, at most `MAX_UNCOMPRESSED_BYTES`; run again in this slot under
/// the wide limit where it needs it, [`POPPLER_WIDE_RUN`]), then the reader
/// cutting it while the gateway holds the raster and the crops: 6 GiB.
pub const PAGE_CROPS: u64 = max(
    POPPLER_WIDE_RUN,
    READER_MEMORY_LIMIT_BYTES + 2 * MAX_UNCOMPRESSED_BYTES,
);

/// What a poppler step whose file is not spooled adds to its charge: the
/// file read whole from the pipe ([`POPPLER_PIPE_READ_PER_BYTE`] a byte);
/// nothing for a spooled file.
pub fn piped(src: &Spool) -> u64 {
    src.piped_bytes().saturating_mul(POPPLER_PIPE_READ_PER_BYTE)
}

/// One page's PGM: its header and one byte a pixel.
const PAGE_PGM: u64 = MAX_OCR_PIXELS + 1024;

/// One page of text recognition (module doc): the larger of its render's
/// and its read's limit, the page and a turned copy, and its hOCR.
pub const OCR_PAGE: u64 = max(OCR_RENDER_MEMORY_LIMIT_BYTES, TESSERACT_MEMORY_LIMIT_BYTES)
    + 2 * PAGE_PGM
    + HOCR_OUTPUT_BYTES;

/// [`OCR_PAGE`] with its render run again under the wide limit (`pdf::wide`:
/// a JPEG 2000 scan openjpeg had no memory for within the render's own).
pub const OCR_PAGE_WIDE: u64 = max(
    POPPLER_WIDE_MEMORY_LIMIT_BYTES,
    TESSERACT_MEMORY_LIMIT_BYTES,
) + 2 * PAGE_PGM
    + HOCR_OUTPUT_BYTES;

/// What lmgw's in-process read of a poppler output allocates at most, per
/// byte of that output: 2. The reads are linear in their input (the layout
/// pass, `pdf_layout`; a picture run's images, `pictures::pdf::images`).
/// Measured 2026-11-20 with every block rounded up as glibc's malloc
/// rounds it, the pass's peak above its inputs was 0.25–0.50 times their
/// bytes on real files (the corpus, `text5k.pdf`, `bigtable.pdf`,
/// `dense200.pdf`) and 1.04 at most on the shapes with the most structure
/// per byte poppler can write (one one-letter word a line, a block each);
/// `pool::tests_read` holds both reads to this bound on those shapes.
pub const READ_PER_OUTPUT_BYTE: u64 = 2;

/// What a step still holds once its tool runs have ended and it only reads
/// their `outputs` in-process: the outputs' buffers as allocated (their
/// capacity), and what reading them allocates ([`READ_PER_OUTPUT_BYTE`]).
/// No tool limit: no tool runs in its slot any more.
pub fn after_runs(outputs: &[&Vec<u8>]) -> u64 {
    outputs.iter().fold(0u64, |sum, o| {
        let (cap, len) = (o.capacity() as u64, o.len() as u64);
        sum.saturating_add(cap)
            .saturating_add(len.saturating_mul(READ_PER_OUTPUT_BYTE))
    })
}

/// What a step without a charge of its own is charged.
pub const HEAVIEST: u64 = max(
    max(
        max(max(PDF_FILE, PDF_FILE_WIDE), POPPLER_TEXT_RUN),
        max(
            max(POPPLER_IMAGE_RUN, PICTURE_RUN),
            max(PAGE_CROPS, POPPLER_WIDE_RUN),
        ),
    ),
    max(READER_RUN, max(OCR_PAGE, OCR_PAGE_WIDE)),
);

const fn max(a: u64, b: u64) -> u64 {
    if a > b {
        a
    } else {
        b
    }
}
