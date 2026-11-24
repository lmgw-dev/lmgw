//! poppler's memory limits (design §14 "Poppler limit from measured
//! peaks"): one for the runs that need no image's pixels, one for the runs
//! that decode images, each from measured peaks, and the wide limit a run
//! that needed more is given once more ([`super::wide`]), so nothing that
//! read under the one 4 GiB limit poppler had before is refused now.
//!
//! Which run gets which is [`PopplerTool::limit`], one exhaustive match, so
//! a new kind of run has to say its limit.

use std::time::Duration;

use super::super::convert::run::{self, Bounds};
use super::MAX_UNCOMPRESSED_BYTES;

/// The address space (`RLIMIT_AS`) of a poppler run that needs no image's
/// pixels: `pdftotext` (the text, the layout's words), `pdftohtml -i` (the
/// layout's fonts), `pdfinfo` and `pdfimages -list`. 512 MiB.
///
/// A hostile PDF can ask for more memory than the machine has (a stream
/// that inflates without end, a path of millions of pieces); with no limit
/// the child grows until the kernel's OOM killer picks a victim, and that
/// may be the gateway or the desktop. The limit makes the child fail alone,
/// and the refusal names this constant ([`super::PdfError::OutOfMemory`]);
/// such a run is run once more under [`POPPLER_WIDE_MEMORY_LIMIT_BYTES`].
///
/// Measured 2026-11-20 (poppler 26.08.0; the smallest address space each
/// run still gives its whole output in, design §14 "Poppler limit from
/// measured peaks"): the conversion corpus and the performance inputs
/// 40–60 MiB, a book of 20 000 pages 100 MiB; the largest was a page
/// drawing one path of a million lines, 400 MiB (poppler keeps a path's
/// pieces until it is drawn, even for its text), a shape no ordinary file
/// has (a page of a million words was still growing at 699 MiB when
/// `PDF_TIMEOUT` stopped it). 512 MiB is five times the book.
/// `pdftohtml -i` and `pdfimages -list` do decode a JPEG 2000 image whole
/// (870 MiB of `VmPeak` for an A3 scan at 600 dpi), but neither needs its
/// pixels: below that their output is the same and nothing is said. A file
/// sent through a pipe is read whole into memory first:
/// [`POPPLER_PIPE_READ_PER_BYTE`].
pub const POPPLER_TEXT_MEMORY_LIMIT_BYTES: u64 = 512 << 20;

/// The address space (`RLIMIT_AS`) of a poppler run that decodes images:
/// `pdftoppm` (a page's render) and `pdftohtml -dataurls` (the picture
/// runs). 2 GiB.
///
/// Measured 2026-11-20 as [`POPPLER_TEXT_MEMORY_LIMIT_BYTES`] (here the
/// runs' `VmPeak`): the corpus at most 70 MiB; what grows is a JPEG 2000
/// image, which poppler decodes whole at about 12 bytes a pixel (an A4 page
/// scanned at 600 dpi in colour 455–471 MiB, A3 at 600 dpi 870–900 MiB,
/// 10 000 × 14 000 pixels 1.67–1.73 GiB), a progressive JPEG (10 000 ×
/// 14 000 pixels 444–472 MiB) and a render's own raster (the 150 dpi render
/// of a page of 14 400 × 9 900 pt, 30 000 × 20 625 pixels, 2.35 GiB: a
/// render takes 4 bytes a pixel, the raster and its alpha, and poppler
/// itself refuses one raster past 2 GiB). 2 GiB is 2.2 times the A3 scan; a
/// render of more than about 500 million pixels (a page of about 15 m² at
/// 150 dpi), or a JPEG 2000 image of more than about 170 million pixels, is
/// run once more under [`POPPLER_WIDE_MEMORY_LIMIT_BYTES`]. Such an image
/// does not stop the run: poppler draws it blank and exits 0, which lmgw
/// reads from its stderr ([`super::wide::JPX_UNDECODED`]).
pub const POPPLER_IMAGE_MEMORY_LIMIT_BYTES: u64 = 2 << 30;

/// The address space a poppler run is given once more when its own limit
/// ([`POPPLER_TEXT_MEMORY_LIMIT_BYTES`], [`POPPLER_IMAGE_MEMORY_LIMIT_BYTES`])
/// was not enough: 4 GiB, the one limit every poppler run had before
/// 2026-11-20, so nothing that read under it is refused now (the owner's
/// ruling, 2026-11-11: what would run is not refused). A run that needs more than this is refused
/// by its name; a JPEG 2000 image poppler cannot decode within it (more
/// than about 340 million pixels) is blank in what poppler drew, and said
/// so by name (`image_not_decoded`).
pub const POPPLER_WIDE_MEMORY_LIMIT_BYTES: u64 = 4 << 30;

/// What a poppler run's limit grows by, per byte of a file sent to it
/// through a pipe: 3. poppler reads a piped file whole into memory before
/// it reads any of it (`convert::ocr::spool`), in a buffer that doubles as
/// it grows, so the old buffer and the new one (up to three times the file)
/// can be held at once. Measured 2026-11-20 with the smallest address space
/// each run still succeeds in: a 215 MiB PDF piped to `pdftotext` needed
/// 384 MiB (1.8 times it) more than the same run reading it by position, a
/// 16.5 MiB one 36 MiB (2.2 times). A conversion spools every PDF, so only
/// Chat and knowledge-base reads (and a conversion whose file could not be
/// spooled, or a page of a one-page file) pipe one.
pub const POPPLER_PIPE_READ_PER_BYTE: u64 = 3;

/// The tool name a picture run is said by: `pdftohtml` decoding every
/// image ([`super::geometry::image_runs_at`]), under the image limit.
pub const PDFTOHTML_IMAGES: &str = "pdftohtml -dataurls";

/// Which of poppler's limits a run is under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// [`POPPLER_TEXT_MEMORY_LIMIT_BYTES`].
    Text,
    /// [`POPPLER_IMAGE_MEMORY_LIMIT_BYTES`].
    Image,
    /// [`POPPLER_WIDE_MEMORY_LIMIT_BYTES`].
    Wide,
    /// `OCR_RENDER_MEMORY_LIMIT_BYTES`: text recognition's render of a
    /// page (`convert::ocr::pages`), widened like an image run.
    OcrRender,
}

impl Limit {
    /// The constant's name, as errors and notes say it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "POPPLER_TEXT_MEMORY_LIMIT_BYTES",
            Self::Image => "POPPLER_IMAGE_MEMORY_LIMIT_BYTES",
            Self::Wide => "POPPLER_WIDE_MEMORY_LIMIT_BYTES",
            Self::OcrRender => "OCR_RENDER_MEMORY_LIMIT_BYTES",
        }
    }

    /// Its value (a test's own, inside [`with_limits`]; the OCR render's
    /// is the image limit's there).
    pub fn bytes(self) -> u64 {
        #[cfg(test)]
        if let Ok(l) = TEST_LIMITS.try_with(|l| *l) {
            return l[match self {
                Self::Text => 0,
                Self::Image | Self::OcrRender => 1,
                Self::Wide => 2,
            }];
        }
        match self {
            Self::Text => POPPLER_TEXT_MEMORY_LIMIT_BYTES,
            Self::Image => POPPLER_IMAGE_MEMORY_LIMIT_BYTES,
            Self::Wide => POPPLER_WIDE_MEMORY_LIMIT_BYTES,
            Self::OcrRender => crate::extract::convert::ocr::OCR_RENDER_MEMORY_LIMIT_BYTES,
        }
    }

    /// "NAME (4.0 GiB)".
    pub fn said(self) -> String {
        format!("{} ({})", self.name(), crate::hf::fmt_bytes(self.bytes()))
    }
}

#[cfg(test)]
tokio::task_local! {
    static TEST_LIMITS: [u64; 3];
}

/// Run `fut` with poppler's three limits (text, image, wide) at the given
/// bytes instead of the named constants: a test of a run that would need
/// gigabytes to pass them.
#[cfg(test)]
pub async fn with_limits<F: std::future::Future>(limits: [u64; 3], fut: F) -> F::Output {
    TEST_LIMITS.scope(limits, fut).await
}

/// The poppler runs lmgw starts, by what they need of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopplerTool {
    /// `pdftotext`: the text, the layout's words, which pages have words.
    Text,
    /// `pdftohtml -i`: the layout's fonts.
    Fonts,
    /// `pdfinfo`: the picture gate's page sizes.
    Info,
    /// `pdfimages -list`: the picture gate's images.
    ImageList,
    /// `pdftoppm`: a page's render.
    Render,
    /// `pdftohtml -dataurls`: a picture run, every image decoded.
    Pictures,
    /// `pdftoppm -gray`: text recognition's render of a page.
    OcrRender,
}

impl PopplerTool {
    /// The name its errors say.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => super::PDFTOTEXT,
            Self::Fonts => super::geometry::PDFTOHTML,
            Self::Info => "pdfinfo",
            Self::ImageList => "pdfimages",
            Self::Render | Self::OcrRender => super::PDFTOPPM,
            Self::Pictures => PDFTOHTML_IMAGES,
        }
    }

    /// The limit it runs under first.
    pub const fn limit(self) -> Limit {
        match self {
            Self::Text | Self::Fonts | Self::Info | Self::ImageList => Limit::Text,
            Self::Render | Self::Pictures => Limit::Image,
            Self::OcrRender => Limit::OcrRender,
        }
    }

    /// Whether it draws images, so a JPEG 2000 image it could not decode
    /// (drawn blank, exit 0) is read from its stderr
    /// ([`super::wide::JPX_UNDECODED`]). The text runs decode such an image
    /// too but never use its pixels.
    pub const fn draws(self) -> bool {
        match self {
            Self::Render | Self::Pictures | Self::OcrRender => true,
            Self::Text | Self::Fonts | Self::Info | Self::ImageList => false,
        }
    }
}

/// A poppler run as its errors say it: the tool, the limit it ran under
/// and whether its file came through a pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub tool: PopplerTool,
    pub limit: Limit,
    pub piped: bool,
}

impl At {
    /// `tool` under its own limit, reading `input`.
    pub fn of(tool: PopplerTool, input: &run::Input) -> Self {
        Self {
            tool,
            limit: tool.limit(),
            piped: matches!(input, run::Input::Bytes(_)),
        }
    }
}

/// One run's bounds under `limit`: `timeout`, `output` bytes of stdout and
/// the limit's address space (grown for a piped file where it is run,
/// [`reading`]).
pub fn bounds(limit: Limit, timeout: Duration, output: u64) -> Bounds {
    Bounds {
        timeout,
        output,
        memory: limit.bytes(),
    }
}

/// [`bounds`] with [`MAX_UNCOMPRESSED_BYTES`] of output.
pub fn poppler(limit: Limit, timeout: Duration) -> Bounds {
    bounds(limit, timeout, MAX_UNCOMPRESSED_BYTES)
}

/// What a poppler run reading `input` needs beside its limit: the piped
/// file's bytes times [`POPPLER_PIPE_READ_PER_BYTE`]; nothing for a spooled
/// file, which poppler reads by position.
pub fn piped_read(input: &run::Input) -> u64 {
    match input {
        run::Input::Bytes(b) => (b.len() as u64).saturating_mul(POPPLER_PIPE_READ_PER_BYTE),
        run::Input::File(_) => 0,
    }
}

/// `bounds` for a poppler run reading `input`: its memory grown by
/// [`piped_read`].
pub(crate) fn reading(bounds: Bounds, input: &run::Input) -> Bounds {
    Bounds {
        memory: bounds.memory.saturating_add(piped_read(input)),
        ..bounds
    }
}
