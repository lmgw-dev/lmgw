//! Where an office file's pictures are (document-conversion design §5.3,
//! WP10): the readers of [`super::office`] call a [`Collector`] for each
//! element they walk, and it notes each picture the document references,
//! in the document's own order, with the size the document gives it:
//!
//! - docx: `w:drawing` (`wp:extent`, `a:blip r:embed`) and VML `w:pict`
//!   (`v:imagedata r:id`); the page from `w:pgSz` (twips);
//! - pptx: `p:pic` (`a:blip r:embed`, its `a:ext`); the slide size from
//!   `p:sldSz`;
//! - odt/odp: `draw:frame` (`svg:width`/`svg:height`) holding `draw:image`
//!   (`xlink:href`); the page from `styles.xml`'s page layout.
//!
//! A picture becomes a marker paragraph in the part's markdown
//! ([`marker`]): the reader writes it where the picture was (after the
//! paragraph that holds it, after the table whose cell holds it), and the
//! conversion turns each into the request's picture line or `figure` block
//! (`convert::pictures::office`). The marker is `U+FFFC` (the object
//! replacement character) around the picture's number; the readers drop
//! `U+FFFC` from the document's own text while collecting, so no text can
//! forge one. Readers that do not collect (Chat attachments, knowledge
//! bases) write no marker and keep their text as before.
//!
//! Only references are noted: the bytes are read later, from the archive,
//! when the request needs them.

use super::office::{Elem, Tok};

/// The object replacement character a marker is made of.
pub const MARK: char = '\u{FFFC}';

/// The marker paragraph of picture `id`.
pub fn marker(id: u32) -> String {
    format!("{MARK}{id}{MARK}")
}

/// The picture a marker paragraph stands for.
pub fn read_marker(text: &str) -> Option<u32> {
    text.trim()
        .strip_prefix(MARK)?
        .strip_suffix(MARK)?
        .parse()
        .ok()
}

/// One picture the document references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// The part it is in (0-based: the document, a slide).
    pub part: usize,
    /// docx/pptx: the relationship id of its part.
    pub rel: Option<String>,
    /// odt/odp: its path in the package (or a URL: a linked picture).
    pub href: Option<String>,
    /// The size the document gives it, in EMU (914 400 an inch).
    pub extent: Option<(u64, u64)>,
}

/// What a reader collected.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficeMedia {
    pub media: Vec<Media>,
    /// The page (docx, odt) or slide (pptx, odp) size, in EMU.
    pub page: Option<(u64, u64)>,
    /// docx/pptx: each part's XML path, whose relationships resolve `rel`.
    pub part_paths: Vec<String>,
}

/// The picture being read.
#[derive(Debug)]
struct Open {
    /// The element that closes it.
    root: String,
    rel: Option<String>,
    href: Option<String>,
    extent: Option<(u64, u64)>,
}

/// Notes the pictures a reader walks past.
#[derive(Debug, Default)]
pub(super) struct Collector {
    pub(super) out: OfficeMedia,
    /// The part being read.
    pub(super) part: usize,
    open: Option<Open>,
}

fn emu(v: Option<&str>) -> Option<u64> {
    v?.trim().parse().ok()
}

/// An ODF length (`21cm`, `8.5in`, `210mm`, `595pt`, `1pc`, `96px`) in EMU.
pub fn length_emu(v: &str) -> Option<u64> {
    let v = v.trim();
    let at = v.find(|c: char| c.is_ascii_alphabetic())?;
    let n: f64 = v[..at].trim().parse().ok()?;
    let per = match &v[at..] {
        "cm" => 360_000.0,
        "mm" => 36_000.0,
        "in" => 914_400.0,
        "pt" => 12_700.0,
        "pc" => 152_400.0,
        "px" => 9_525.0,
        _ => return None,
    };
    let e = n * per;
    (e.is_finite() && e >= 0.0).then_some(e as u64)
}

impl Collector {
    /// A reader's token: the picture that closes with it, if any.
    pub(super) fn look(&mut self, t: &Tok) -> Option<u32> {
        match t {
            Tok::Start(e) => {
                self.start(e);
                None
            }
            Tok::End(n) => self.end(n),
            Tok::Text(_) => None,
        }
    }

    /// An element opens.
    pub(super) fn start(&mut self, e: &Elem) {
        let name = e.name.as_str();
        match name {
            "drawing" | "pict" | "pic" | "frame" if self.open.is_none() => {
                let extent = match name {
                    "frame" => e
                        .attr("svg:width")
                        .and_then(length_emu)
                        .zip(e.attr("svg:height").and_then(length_emu)),
                    _ => None,
                };
                self.open = Some(Open {
                    root: name.to_string(),
                    rel: None,
                    href: None,
                    extent,
                });
            }
            "pgSz" => {
                // Twips: 635 EMU each.
                let w = emu(e.attr("w:w")).map(|t| t.saturating_mul(635));
                let h = emu(e.attr("w:h")).map(|t| t.saturating_mul(635));
                if let (Some(w), Some(h)) = (w, h) {
                    self.out.page = Some((w, h));
                }
            }
            _ => {}
        }
        let Some(o) = self.open.as_mut() else {
            return;
        };
        match name {
            "extent" | "ext" if o.extent.is_none() => {
                o.extent = emu(e.attr("cx")).zip(emu(e.attr("cy")));
            }
            "blip" => {
                o.rel = e
                    .attr("r:embed")
                    .or_else(|| e.attr("r:link"))
                    .map(str::to_string);
            }
            "imagedata" => o.rel = e.attr("r:id").map(str::to_string),
            "image" => o.href = e.attr("xlink:href").map(str::to_string),
            _ => {}
        }
    }

    /// An element closes: the picture's number when it closed one.
    pub(super) fn end(&mut self, name: &str) -> Option<u32> {
        if self.open.as_ref().is_none_or(|o| o.root != name) {
            return None;
        }
        let o = self.open.take()?;
        if o.rel.is_none() && o.href.is_none() {
            return None;
        }
        let id = u32::try_from(self.out.media.len()).ok()?;
        self.out.media.push(Media {
            part: self.part,
            rel: o.rel,
            href: o.href,
            extent: o.extent,
        });
        Some(id)
    }

    /// The slide size of a presentation part (`p:sldSz`) or an ODF page
    /// layout (`style:page-layout-properties`), whichever `e` is.
    pub(super) fn page_size(&mut self, e: &Elem) {
        let size = match e.name.as_str() {
            "sldSz" => emu(e.attr("cx")).zip(emu(e.attr("cy"))),
            "page-layout-properties" => e
                .attr("fo:page-width")
                .and_then(length_emu)
                .zip(e.attr("fo:page-height").and_then(length_emu)),
            _ => None,
        };
        if self.out.page.is_none() {
            self.out.page = size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip_and_lengths_read_in_emu() {
        assert_eq!(read_marker(&marker(7)), Some(7));
        assert_eq!(read_marker(" \u{FFFC}3\u{FFFC}\n"), Some(3));
        assert_eq!(read_marker("\u{FFFC}x\u{FFFC}"), None);
        assert_eq!(read_marker("3"), None);
        assert_eq!(length_emu("21cm"), Some(7_560_000));
        assert_eq!(length_emu("1in"), Some(914_400));
        assert_eq!(length_emu("12pt"), Some(152_400));
        assert_eq!(length_emu("12furlong"), None);
        assert_eq!(length_emu("-1cm"), None);
    }
}
