//! Picture export (document-conversion design §5.3, WP10): a file's
//! pictures found in reading order, each a `figure` block in the file's
//! [`Doc`](super::doc::Doc) (or a line in an office reader's markdown) and
//! an entry of the file's [`Found`] list, then written as the request's
//! `pictures` asks: placeholder, embedded or referenced.
//!
//! Where they come from:
//! - **PDF** ([`pdf`]): the file's own text runs stay as they are; its
//!   pictures come from `pdftohtml -xml -dataurls` runs of their own over
//!   the pages `pdfimages -list` says may have one ([`pdf::gate`],
//!   [`pdf::runs`]), beside the text runs, with every raster image drawn
//!   and its box. The images come inline as `data:` URIs, so the run writes
//!   no file (without `-dataurls` it writes each image into its working
//!   directory, checked with poppler 26.08 on 2026-11-18); its output is
//!   bounded by `MAX_UNCOMPRESSED_BYTES` like any poppler output, and only
//!   a hash of each image is kept. The picture is cropped out of a
//!   `pdftoppm` render of its page ([`render`]), so it is what a reader
//!   sees (masks, rotation, JBIG2/JPEG 2000 decoded by poppler in its
//!   child), at `72 × images_scale` dpi. Vector drawings are not found
//!   until the layout model marks them (D22); a figure caption with no
//!   picture beside it is a `pictures_partial` note. `output: text` does
//!   no picture work at all.
//! - **docx/pptx/odt/odp** ([`office`]): the readers mark where the document
//!   references a picture (`a:blip r:embed`, `v:imagedata`, `draw:image`),
//!   so the order is the document's own; the picture's bytes are the media
//!   entry as stored in the file, never decoded.
//!
//! Which are left out ([`WarningCode::PicturesLeftOut`], each kind counted):
//! smaller than `picture_min_area` of the page, the same image in the same
//! place on at least [`HEADER_MIN_SHARE`] of the pages (a logo in the
//! running header, a watermark, a background: the rule the repeated header
//! text follows, anywhere on the page), and an image covering at least
//! [`PAGE_COVER_SHARE`] of its page (the page's background or its scan: a
//! scanned page's text is read by text recognition, `ocr`, and the scan
//! itself is the page, not a picture on it).
//!
//! **For picture description (WP11).** Every exported picture is one
//! [`Picture`] of [`Found::pictures`], in reading order and addressed by its
//! index (the figure block's [`Figure::index`](super::doc::Figure)): its part
//! and box, its caption (the caption-like text block right below or above),
//! the paragraph before it, and its bytes once [`render`] (PDF) or
//! [`office`] (media) filled them. A describer adds one model call per
//! picture and writes [`Figure::description`](super::doc::Figure); nothing
//! has to find the pictures again. In `placeholder` mode the bytes are not
//! made yet; [`needs_bytes`] is the one switch.
//!
//! **Bounds.** A picture's bytes are reserved from the request's output
//! budget when it gets them, at the size the answer writes them in
//! ([`reserve`]: base64 and its entry), and the file's output takes use that
//! reservation first (`budget::Spend`), so each byte counts once. A picture
//! that does not fit keeps its placeholder, named; the file stays
//! converted. A page's crop raster passing [`render::PAGE_RASTER_MAX_BYTES`]
//! and a page with more than [`PICTURES_PER_PAGE_MAX`] pictures are named
//! warnings.
//!
//! **Privacy.** Picture bytes are document content: they go into the answer
//! and a job's stored result only, never into a log line, a usage row or the
//! `convert_calls` record.

use base64::Engine as _;
use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertWarning, Output, Pictures, WarningCode};
use lmgw_api_types::documents::pictures::DocPicture;
use sha2::{Digest, Sha256};

use super::doc::{Rect, Source};

pub(crate) mod caption;
pub mod office;
pub mod pdf;
pub mod render;
pub(crate) mod sniff;

pub use super::pdf_layout::{HEADER_BAND, HEADER_MIN_PAGES, HEADER_MIN_SHARE};

/// An image covering at least this share of its page's area is the page's
/// background or its scan, not a picture: a scanned page is one image the
/// size of the page, whose text recognition reads (`ocr`). docling's layout
/// model does not call a whole page a picture either.
pub const PAGE_COVER_SHARE: f64 = 0.9;

/// The most pictures one page has placed in reading order and cropped. A
/// real page has a few dozen at most; a hostile one can draw an image a
/// million times. Pictures past it on a page are still written (as
/// placeholders at the end of their page), without bytes and without a
/// caption lookup, and the file's `pictures_partial` warning names this
/// constant.
pub const PICTURES_PER_PAGE_MAX: usize = 256;

/// What a request makes of pictures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub mode: Pictures,
    /// `picture_min_area`.
    pub min_area: f64,
    /// `images_scale`.
    pub scale: f64,
    /// The pictures are described (`describe_pictures` with a model
    /// chosen, `super::describe`): their bytes are made for the model's
    /// calls whatever the mode, and written only as the mode says.
    pub describe: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Pictures::Placeholder,
            min_area: lmgw_api_types::documents::convert::PICTURE_MIN_AREA_DEFAULT,
            scale: lmgw_api_types::documents::convert::IMAGES_SCALE_DEFAULT,
            describe: false,
        }
    }
}

/// Whether the pictures' bytes are made: embedded and referenced pictures
/// carry them, and described ones are sent to the model.
pub fn needs_bytes(s: &Settings) -> bool {
    writes_bytes(s) || s.describe
}

/// Whether the answer writes the pictures' bytes (embedded or referenced;
/// `output: text` is planned as placeholders). Bytes made only for the
/// picture-description model's calls are reserved at their own size
/// ([`held`]) and given back once the calls are done.
pub fn writes_bytes(s: &Settings) -> bool {
    s.mode != Pictures::Placeholder
}

/// What a picture of `bytes` bytes with a caption of `caption` bytes holds
/// of the request's budget while it has its bytes: [`reserve`] when the
/// answer writes them, else the bytes themselves (made only to be sent to
/// the picture-description model).
pub fn held(bytes: usize, caption: usize, s: &Settings) -> u64 {
    if writes_bytes(s) {
        reserve(bytes, caption)
    } else {
        bytes as u64
    }
}

/// The advice a note gives for pictures past the request's output budget:
/// placeholders help only where the answer would have written the bytes.
pub fn over_budget_advice(s: &Settings) -> &'static str {
    if writes_bytes(s) {
        "send the files in separate requests or ask for placeholders"
    } else {
        "the pictures' bytes were made only to be sent to the picture-description model; \
         send the files in separate requests"
    }
}

/// A picture's bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub bytes: Bytes,
    pub mime: &'static str,
    /// The artifact's extension.
    pub ext: &'static str,
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
}

/// One picture of a file, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// The part it is on (a page's number, a slide's position, 1 for a
    /// document).
    pub part: u32,
    /// Its box in points, where the part has geometry.
    pub bbox: Option<Rect>,
    pub caption: Option<String>,
    /// The paragraph before it: the context a describer is given (§5.3).
    pub before: Option<String>,
    pub image: Option<Image>,
    pub source: Source,
    /// Why it has no bytes although they were asked for (said once per
    /// file in the warnings; kept here for a describer).
    pub missing: Option<&'static str>,
    /// An office picture's media entry in its archive.
    pub(crate) entry: Option<String>,
    /// The picture-description model's words (`super::describe`), written
    /// after the picture and its caption, as its `figure` block's
    /// `description` and in the `pictures` list.
    pub description: Option<String>,
    /// Its share of its part's area, where both are known (what
    /// `describe_min_area` is judged on).
    pub share: Option<f64>,
}

impl Picture {
    pub fn new(part: u32, bbox: Option<Rect>, source: Source) -> Self {
        Self {
            part,
            bbox,
            caption: None,
            before: None,
            image: None,
            source,
            missing: None,
            entry: None,
            description: None,
            share: None,
        }
    }
}

/// A file's pictures and what was said about them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    pub pictures: Vec<Picture>,
    pub warnings: Vec<ConvertWarning>,
    /// Bytes reserved from the request's budget for the pictures' bytes
    /// ([`reserve`]), which the file's output takes use first.
    pub charged: u64,
    /// An office file's markers (`office_media`): each marker's picture
    /// index, `None` for one left out.
    pub(crate) markers: Vec<Option<u32>>,
}

/// What one picture takes in the answer besides its data and its caption,
/// at most: its `pictures` list entry ([`wire_size`]: 256 and the longest
/// artifact name, `artifacts/image_` + 10 digits + `_` + 64 hex + `.` + 5,
/// 97), and its line in the text or its `figure` block in json (the same
/// name again, the line's or the block's own fields).
pub const ENTRY_RESERVE: u64 = 512;

/// What a picture of `bytes` bytes with a caption of `caption` bytes is
/// reserved at when it gets its bytes: its base64 (4 · ⌈n/3⌉), its caption
/// twice (the list's entry and the json block) and [`ENTRY_RESERVE`]: the
/// most the answer writes it in, so the file's output takes that use the
/// reservation first never need more for the picture than it holds.
pub fn reserve(bytes: usize, caption: usize) -> u64 {
    let b64 = (bytes as u64).div_ceil(3).saturating_mul(4);
    b64.saturating_add(ENTRY_RESERVE)
        .saturating_add((caption as u64).saturating_mul(2))
}

/// The id a picture has in the answer: `p1` for index 0.
pub fn id(index: u32) -> String {
    format!("p{}", u64::from(index) + 1)
}

/// The relative name of picture `index` with `image` (docling's
/// `artifacts/image_<n:06>_<hash>.png`; the hash here is the SHA-256 of the
/// bytes, docling's of the decoded pixels, and the extension the format's).
pub fn artifact(index: u32, image: &Image) -> String {
    let hash = hex::encode(Sha256::digest(&image.bytes));
    format!("artifacts/image_{index:06}_{hash}.{}", image.ext)
}

/// Whether `name` is a name [`artifact`] makes: what a stored result's
/// picture file may be called, so no name from a file on disk can reach
/// outside its directory.
pub fn is_artifact(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("artifacts/image_") else {
        return false;
    };
    let Some((n, rest)) = rest.split_once('_') else {
        return false;
    };
    let Some((hash, ext)) = rest.split_once('.') else {
        return false;
    };
    n.len() >= 6
        && n.bytes().all(|b| b.is_ascii_digit())
        && hash.len() == 64
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && !ext.is_empty()
        && ext.len() <= 5
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `page 3` or `pages 1-2, 5`, as a warning names them.
pub(crate) fn pages_said(pages: &[u32]) -> String {
    let list = super::pages::page_ranges(pages);
    if pages.len() == 1 {
        format!("page {list}")
    } else {
        format!("pages {list}")
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Picture `index`'s markdown line, as docling writes it: `<!-- image -->`,
/// `![Image](data:…)` or `![Image](artifacts/…)`. A picture without bytes is
/// a placeholder whatever the mode. A described picture's description is
/// written after the picture and its caption (`super::describe::place`).
pub fn line(mode: Pictures, index: u32, p: Option<&Picture>, out: &mut String) {
    match (mode, p.and_then(|p| p.image.as_ref())) {
        (Pictures::Embedded, Some(img)) => {
            out.push_str("![Image](data:");
            out.push_str(img.mime);
            out.push_str(";base64,");
            out.push_str(&b64(&img.bytes));
            out.push(')');
        }
        (Pictures::Referenced, Some(img)) => {
            out.push_str("![Image](");
            out.push_str(&artifact(index, img));
            out.push(')');
        }
        _ => out.push_str(super::doc::render::PLACEHOLDER),
    }
}

/// The answer's `pictures` list: the bytes in it where the request asked
/// for them and the text does not hold them already (module doc of
/// [`DocPicture`]).
pub fn wire(found: &Found, s: &Settings, output: Output) -> Vec<DocPicture> {
    let with_bytes = match s.mode {
        Pictures::Placeholder => false,
        Pictures::Referenced => true,
        Pictures::Embedded => output == Output::Json,
    };
    let round = |v: f32| (f64::from(v) * 10.0).round() / 10.0;
    found
        .pictures
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            let img = p.image.as_ref();
            let carried = img.filter(|_| with_bytes);
            DocPicture {
                id: id(i),
                part: p.part,
                bbox: p
                    .bbox
                    .map(|r| [round(r.x0), round(r.y0), round(r.x1), round(r.y1)]),
                caption: p.caption.clone(),
                mime: img.map(|i| i.mime.to_string()),
                width_px: img.and_then(|i| i.width_px),
                height_px: img.and_then(|i| i.height_px),
                bytes: img.map(|i| i.bytes.len() as u64),
                file: carried.map(|img| artifact(i, img)),
                data_base64: carried.map(|img| b64(&img.bytes)),
                description: p.description.clone(),
            }
        })
        .collect()
}

/// The bytes the answer's `pictures` list takes (what the request's budget
/// counts for it).
pub fn wire_size(list: &[DocPicture]) -> usize {
    list.iter()
        .map(|p| {
            256 + p.data_base64.as_ref().map_or(0, String::len)
                + p.file.as_ref().map_or(0, String::len)
                + p.caption.as_ref().map_or(0, String::len)
                + p.description.as_ref().map_or(0, String::len)
        })
        .fold(0usize, usize::saturating_add)
}

/// A warning of this module, the tool named where one ran.
pub(crate) fn warning(
    code: WarningCode,
    message: String,
    parts: Vec<u32>,
    tool: Option<&str>,
) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: tool.map(str::to_string),
        parts,
        option: None,
    }
}

/// The note an EPUB's conversion carries until its pictures are exported:
/// WP4 builds the EPUB reader beside this WP, and its handler adds this
/// (one call) where its file has images; picture export for EPUB then uses
/// the `office_media` markers the office readers use.
pub fn epub_not_yet() -> ConvertWarning {
    let mut w = warning(
        WarningCode::NotYet,
        "pictures in EPUB files are not exported yet; the text is converted without them"
            .to_string(),
        Vec::new(),
        None,
    );
    w.option = Some("pictures".to_string());
    w
}

/// What was left out, counted by kind ([`WarningCode::PicturesLeftOut`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeftOut {
    pub small: u32,
    pub repeated: u32,
    pub cover: u32,
}

impl LeftOut {
    /// The file's warning, or none when nothing was left out.
    pub fn warning(&self, s: &Settings, pages: Vec<u32>) -> Option<ConvertWarning> {
        let mut said = Vec::new();
        if self.small > 0 {
            said.push(format!(
                "{} smaller than picture_min_area ({} of the page; send a smaller \
                 picture_min_area to keep them)",
                self.small, s.min_area
            ));
        }
        if self.repeated > 0 {
            said.push(format!(
                "{} the same picture in the same place on most pages (a running header or \
                 footer, a logo, a watermark or a background)",
                self.repeated
            ));
        }
        if self.cover > 0 {
            said.push(format!(
                "{} covering their whole page (its background or a scan, at least {} of the \
                 page's area; a scanned page's text is read by text recognition or the \
                 page-reading model)",
                self.cover, PAGE_COVER_SHARE
            ));
        }
        if said.is_empty() {
            return None;
        }
        Some(warning(
            WarningCode::PicturesLeftOut,
            format!("pictures left out of the export: {}", said.join("; ")),
            pages,
            None,
        ))
    }
}

#[cfg(test)]
mod tests;
