//! The layout model (document-conversion design §3 tier 1 "ONNX models on
//! the CPU", §10 WP8): docling's heron model (RT-DETRv2) on ONNX Runtime
//! finds a PDF page's regions (title, section header, text, list item,
//! table, picture, caption, footnote, formula, code, page header and
//! footer), and the layout pass builds the page's blocks from them instead
//! of from poppler's geometry alone ([`super::pdf_layout`]'s `regions`).
//!
//! **Off until present and chosen.** The model file is not shipped and
//! nothing downloads it by itself ([`files`]): Settings › Documents shows
//! it as found or missing ([`status`]) and "Use the layout model"
//! (`documents.onnx_layout`) is off by default. [`configure`] takes both
//! from every published settings snapshot, so the page and a conversion
//! read one choice.
//!
//! **The model stage of a PDF** ([`stage`]) runs before the file's geometry
//! runs and holds no slot of the tool pool while it waits:
//! 1. one plain `pdftotext` run over the request's page span says which
//!    of its pages there are and which have text (a page without words
//!    has nothing to assign to regions, so it is not rendered);
//! 2. the picked pages with text are rendered by `pdftoppm` straight to the
//!    model's input size ([`prep::SIDE`] squared, the page stretched as
//!    docling's preprocessor stretches it, a turned page as displayed), a
//!    run of up to [`RENDER_BATCH_PAGES`] neighbouring pages at a time in
//!    one slot, into a private folder (in the system's temporary folder,
//!    which may be memory: at most [`RENDER_BATCH_BYTES`] at once), the
//!    document's own images never decoded in the gateway;
//! 3. each page's picture is read back and deleted, and goes through the
//!    one shared session ([`session`]): one inference at a time
//!    gateway-wide, on a blocking thread that holds as many pool slots as
//!    the session has threads.
//!
//! So a 1 000-page file holds one picture in memory and one batch on disk,
//! and its regions (a few KiB a page); the file is piped to poppler once
//! per batch, not once per page. A caller that goes away stops the stage
//! between pages. Everything is CPU work: the GPU hold changes nothing.
//!
//! **Bounds.** The renders of one file get [`pdf::PDF_TIMEOUT`] together,
//! what one geometry run of the whole file gets: the run that reaches it
//! ends the stage. With the caller's `document_timeout` (docling's) the
//! stage also ends once the file's clock (`pool::clock`) has used
//! [`MODEL_DEADLINE_SHARE`] of it, so the geometry has the rest. Either
//! way the pages not read yet are laid out by poppler's geometry, and the
//! file says so.
//!
//! A page whose render or inference fails, and a page whose regions are
//! implausible, is laid out by poppler's geometry as without the model,
//! and the file says so ([`notes`]).

mod decode;
pub mod files;
mod prep;
mod session;
mod stage;
pub mod status;

#[cfg(test)]
mod tests;

#[cfg(test)]
tokio::task_local! {
    /// A test's recorded answer, which [`stage`] gives in place of the
    /// model's while it is set (a whole conversion with regions, no model).
    pub static RECORDED: Regions;
}

use std::collections::BTreeMap;
use std::sync::RwLock;

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};

use super::doc::Rect;
use super::pages::page_ranges;

pub use decode::{decode, Label, Region};
pub use prep::SIDE;
pub use session::{LAYOUT_IDLE_UNLOAD, LAYOUT_MEMORY_BYTES, LAYOUT_THREADS};
pub use stage::{stage, MODEL_DEADLINE_SHARE, RENDER_BATCH_BYTES, RENDER_BATCH_PAGES};

/// The tool name the warnings carry.
pub const TOOL: &str = "layout_model";

/// What Settings › Documents chose, as the published settings have it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    /// "Use the layout model" (`documents.onnx_layout`).
    pub on: bool,
    /// The aux models directory the file lives under (design §3).
    pub models_dir: String,
}

static CHOICE: RwLock<Choice> = RwLock::new(Choice {
    on: false,
    models_dir: String::new(),
});

/// The choice in force, applied by every snapshot publish.
pub fn configure(on: bool, models_dir: &str) {
    let mut g = CHOICE.write().unwrap_or_else(|e| e.into_inner());
    if g.on != on || g.models_dir != models_dir {
        *g = Choice {
            on,
            models_dir: models_dir.to_string(),
        };
    }
}

/// The choice in force.
pub fn choice() -> Choice {
    CHOICE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// What the model made of one page.
#[derive(Debug, Clone, PartialEq)]
pub enum PageRegions {
    /// Its regions, boxes as shares of the page's width and height.
    Found(Vec<Region>),
    /// It was not read: its render or its inference failed, or the stage
    /// ended before it. Why.
    Failed(String),
}

/// What the model stage made of a file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Regions {
    /// The pages the model was asked about, by their 1-based number.
    pub pages: BTreeMap<u32, PageRegions>,
    /// The model could not be used at all (its file is gone or is another
    /// file, the session did not load): why.
    pub unusable: Option<String>,
}

impl Regions {
    /// Page `number`'s regions, when the model read it.
    pub fn page(&self, number: u32) -> Option<&[Region]> {
        match self.pages.get(&number) {
            Some(PageRegions::Found(r)) => Some(r),
            _ => None,
        }
    }

    pub(crate) fn unusable(why: String) -> Self {
        Self {
            pages: BTreeMap::new(),
            unusable: Some(why),
        }
    }
}

/// How the layout pass used one page's regions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageUse {
    /// The lines the model marked as page header or footer and the pass
    /// left out of the text: `true` for a header, and the line.
    pub furniture: Vec<(bool, String)>,
    /// Lines inside pictures (a chart's labels), kept in the figure block
    /// and left out of the text.
    pub picture_lines: u32,
}

/// Why a page's regions were not used ([`super::pdf_layout`]'s `regions`
/// says when).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Implausible {
    /// More regions than the model has queries.
    TooMany,
    /// The page is turned and its displayed size is not known.
    UnknownSize,
    /// None left after the thresholds.
    NoRegions,
    /// Most of the page's lines in none.
    Orphans,
    /// Most of the page's lines inside pictures.
    PictureText,
}

impl Implausible {
    /// Why, in words for the warning.
    fn why(self) -> String {
        match self {
            Self::TooMany => format!(
                "it answered more regions than it has queries (MAX_REGIONS, {})",
                crate::extract::convert::pdf_layout::MAX_REGIONS
            ),
            Self::UnknownSize => "the page is turned and pdftohtml gave no size for it, so the \
                                  regions could not be placed"
                .into(),
            Self::NoRegions => "it found no regions on them".into(),
            Self::Orphans => format!(
                "more than ORPHAN_MAX_SHARE ({:.0} %) of their lines lay in no region",
                crate::extract::convert::pdf_layout::ORPHAN_MAX_SHARE * 100.0
            ),
            Self::PictureText => format!(
                "more than PICTURE_TEXT_MAX_SHARE ({:.0} %) of their lines lay inside what it \
                 took for pictures, so their text would have been lost to the markdown",
                crate::extract::convert::pdf_layout::PICTURE_TEXT_MAX_SHARE * 100.0
            ),
        }
    }
}

/// How the layout pass used the regions, page by page (the pages it has).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Used {
    /// The pages laid out from regions.
    pub pages: BTreeMap<u32, PageUse>,
    /// The pages whose regions were implausible, and why, laid out by
    /// poppler's geometry.
    pub implausible: Vec<(u32, Implausible)>,
}

fn warn(code: WarningCode, message: String, parts: Vec<u32>) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: Some(TOOL.to_string()),
        parts,
        option: None,
    }
}

/// A removed line as the warning names it: its digits as `#`, so a page
/// number that counts with the page is one text.
fn numbered(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// The file's notes about the model: why it could not be used, the pages
/// it failed on or gave implausible regions for (each laid out by
/// poppler's geometry), the page headers and footers it marked, and the
/// text inside pictures. Only `picked` pages are named.
pub fn notes(regions: &Regions, used: &Used, picked: &[u32]) -> Vec<ConvertWarning> {
    const BUILT_IN: &str = "laid out by the built-in rules instead";
    let is_picked = |n: &u32| picked.binary_search(n).is_ok();
    let mut out = Vec::new();
    if let Some(why) = &regions.unusable {
        out.push(warn(
            WarningCode::ToolMissing,
            format!(
                "the layout model is switched on under Settings → Documents but could not be \
                 used: {why}; the pages it did not read were {BUILT_IN}"
            ),
            Vec::new(),
        ));
    }
    // One note per reason, each with its pages.
    let mut failed: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for (n, p) in regions.pages.iter().filter(|(n, _)| is_picked(n)) {
        if let PageRegions::Failed(why) = p {
            failed.entry(why.as_str()).or_default().push(*n);
        }
    }
    let mut failed: Vec<(&str, Vec<u32>)> = failed.into_iter().collect();
    failed.sort_by_key(|(_, pages)| pages[0]);
    for (why, pages) in failed {
        out.push(warn(
            WarningCode::PartFailed,
            format!(
                "the layout model did not read page(s) {} ({why}); they were {BUILT_IN}",
                page_ranges(&pages)
            ),
            pages,
        ));
    }
    let mut implausible: BTreeMap<Implausible, Vec<u32>> = BTreeMap::new();
    for (n, why) in used.implausible.iter().filter(|(n, _)| is_picked(n)) {
        implausible.entry(*why).or_default().push(*n);
    }
    for (why, pages) in implausible {
        out.push(warn(
            WarningCode::PartFailed,
            format!(
                "the layout model's regions were not used on page(s) {} ({}); they were \
                 {BUILT_IN}",
                page_ranges(&pages),
                why.why()
            ),
            pages,
        ));
    }
    // Each removed text once, with how many picked pages it left.
    let mut removed: BTreeMap<(bool, String), u32> = BTreeMap::new();
    let (mut picture_pages, mut picture_lines) = (Vec::new(), 0u64);
    for (n, page) in used.pages.iter().filter(|(n, _)| is_picked(n)) {
        let mut on_page: Vec<(bool, String)> = page
            .furniture
            .iter()
            .map(|(top, t)| (*top, numbered(t)))
            .collect();
        on_page.sort();
        on_page.dedup();
        for key in on_page {
            *removed.entry(key).or_default() += 1;
        }
        if page.picture_lines > 0 {
            picture_pages.push(*n);
            picture_lines += u64::from(page.picture_lines);
        }
    }
    if !removed.is_empty() {
        let lines: Vec<String> = removed
            .iter()
            .map(|((top, text), pages)| {
                let at = if *top { "top" } else { "bottom" };
                format!("\"{text}\" ({at}, {pages} pages)")
            })
            .collect();
        out.push(warn(
            WarningCode::HeaderRemoved,
            format!(
                "{} line(s) the layout model marked as page headers and footers were left out \
                 of the text (json keeps them as page_header and page_footer blocks; a number \
                 is written #): {}",
                lines.len(),
                lines.join("; ")
            ),
            Vec::new(),
        ));
    }
    if picture_lines > 0 {
        out.push(warn(
            WarningCode::PictureText,
            format!(
                "{picture_lines} line(s) of text inside pictures (the labels of a figure or a \
                 chart) on page(s) {} were left out of the text; json keeps them in the figure \
                 block's text",
                page_ranges(&picture_pages)
            ),
            picture_pages,
        ));
    }
    out
}

/// A region's box in points on a page of `width` × `height`.
pub fn in_points(r: &Rect, width: f32, height: f32) -> Rect {
    Rect {
        x0: r.x0 * width,
        y0: r.y0 * height,
        x1: r.x1 * width,
        y1: r.y1 * height,
    }
}
