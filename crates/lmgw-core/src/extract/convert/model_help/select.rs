//! Which pages go to the page-reading model (design §3 tier 1, §7.1): a
//! function over what the readers before it made of a page. The order of
//! readers for a page is text layer → text recognition → model: the model
//! is asked last, about what the others left or read poorly.
//!
//! | the page | `missing` | `missing_or_tables` | `every_page` |
//! |---|---|---|---|
//! | nothing read it (no text layer; recognition off, missing, failed, or found no word) | picture | picture | picture |
//! | recognition read it poorly ([`OCR_LOW_CONFIDENCE`]) | – | picture | picture |
//! | recognition read it well, a table on it | – | picture | picture |
//! | recognition read it well, no table | – | – | picture |
//! | its text layer, a table on it | – | picture and text | picture and text |
//! | its text layer, no table | – | – | picture and text |
//!
//! "Picture" asks with the page's image alone; "picture and text" adds the
//! text layer's words for the model to copy from. Recognised text is never
//! given to copy from: it may be wrong, which is why the model reads the
//! page. "Poorly" is a page of recognition's `ocr_low_confidence` warning:
//! its words' mean confidence below [`OCR_LOW_CONFIDENCE`] (60 of 100), or
//! lines that read as vertical with no turn reading better. A page turned
//! upright before it was read (`ocr_page_turned`) and read with confidence
//! is read well.

use crate::extract::convert::doc::{BlockKind, Part, Source};
pub use crate::extract::convert::ocr::OCR_LOW_CONFIDENCE;

/// What the request's `model_help` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pages {
    /// Only pages nothing read.
    Missing,
    /// Pages nothing read or recognition read poorly, and pages that hold a
    /// table.
    MissingOrTables,
    Every,
}

/// Who read a page before the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBy {
    /// Nothing did, or recognition found no word on it.
    Nothing,
    /// Its text layer (laid out by the tools, with or without the layout
    /// model).
    TextLayer,
    /// Text recognition; `poor` as the module doc says.
    Recognition { poor: bool },
}

/// What the tools made of a page: the facts the choice is made on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFacts {
    pub read: ReadBy,
    /// Its blocks hold a table: the layout pass found aligned cells
    /// (`pdf_layout`'s table rule, the page rule of `vision_prompts` on
    /// geometry, the layout model's table regions).
    pub table: bool,
}

/// What text recognition said of a file's pages (`ocr::pages::PdfOcr`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Recognised<'a> {
    pub poor: Option<&'a std::collections::BTreeSet<u32>>,
    pub empty: Option<&'a std::collections::BTreeSet<u32>>,
}

impl PageFacts {
    pub fn of(part: &Part, ocr: Recognised<'_>) -> Self {
        let has = |set: Option<&std::collections::BTreeSet<u32>>| {
            set.is_some_and(|s| s.contains(&part.number))
        };
        let read = match part.source {
            None => ReadBy::Nothing,
            Some(Source::Ocr) if has(ocr.empty) => ReadBy::Nothing,
            Some(Source::Ocr) => ReadBy::Recognition {
                poor: has(ocr.poor),
            },
            Some(_) => ReadBy::TextLayer,
        };
        Self {
            read,
            table: part
                .blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table(_))),
        }
    }
}

/// How the model is asked about a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The page's image alone: it has no text layer.
    Image,
    /// The page has text: its image and the text the tools read, which the
    /// model copies from.
    ImageAndText,
}

/// Whether a page goes to the model, and how it is asked (module doc).
pub fn pick(facts: PageFacts, pages: Pages) -> Option<Ask> {
    use Pages::{Every, Missing, MissingOrTables};
    match (facts.read, facts.table, pages) {
        (ReadBy::Nothing, _, _) => Some(Ask::Image),
        (_, _, Missing) => None,
        (ReadBy::Recognition { poor: true }, _, MissingOrTables)
        | (ReadBy::Recognition { .. }, true, MissingOrTables)
        | (ReadBy::Recognition { .. }, _, Every) => Some(Ask::Image),
        (ReadBy::Recognition { .. }, false, MissingOrTables) => None,
        (ReadBy::TextLayer, false, MissingOrTables) => None,
        (ReadBy::TextLayer, true, MissingOrTables) | (ReadBy::TextLayer, _, Every) => {
            Some(Ask::ImageAndText)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_pages_always_go_table_pages_too_and_every_page_takes_the_rest() {
        let f = |read, table| PageFacts { read, table };
        let text = ReadBy::TextLayer;
        for p in [Pages::MissingOrTables, Pages::Every] {
            assert_eq!(pick(f(ReadBy::Nothing, false), p), Some(Ask::Image));
            assert_eq!(pick(f(text, true), p), Some(Ask::ImageAndText));
        }
        // `missing`: a page something read never goes, table or not.
        for table in [false, true] {
            assert_eq!(
                pick(f(ReadBy::Nothing, table), Pages::Missing),
                Some(Ask::Image)
            );
            assert_eq!(pick(f(text, table), Pages::Missing), None);
        }
        assert_eq!(pick(f(text, false), Pages::MissingOrTables), None);
        assert_eq!(pick(f(text, false), Pages::Every), Some(Ask::ImageAndText));
    }

    /// The module doc's table, row by row, for recognised pages.
    #[test]
    fn recognised_pages_go_when_read_poorly_or_with_a_table_and_always_as_their_picture() {
        let f = |poor, table| PageFacts {
            read: ReadBy::Recognition { poor },
            table,
        };
        // Poorly read: not for `missing` (something read it), else yes.
        assert_eq!(pick(f(true, false), Pages::Missing), None);
        assert_eq!(
            pick(f(true, false), Pages::MissingOrTables),
            Some(Ask::Image)
        );
        assert_eq!(pick(f(true, false), Pages::Every), Some(Ask::Image));
        // Read well: only a table page, or every page.
        assert_eq!(pick(f(false, false), Pages::MissingOrTables), None);
        assert_eq!(
            pick(f(false, true), Pages::MissingOrTables),
            Some(Ask::Image)
        );
        assert_eq!(pick(f(false, true), Pages::Missing), None);
        assert_eq!(pick(f(false, false), Pages::Every), Some(Ask::Image));
    }

    #[test]
    fn a_page_s_facts_come_from_its_source_and_what_recognition_said_of_it() {
        let part = |n, source| Part {
            kind: crate::extract::convert::doc::PartKind::Page,
            number: n,
            name: None,
            size: None,
            source,
            blocks: Vec::new(),
        };
        let poor = std::collections::BTreeSet::from([2]);
        let empty = std::collections::BTreeSet::from([3]);
        let ocr = Recognised {
            poor: Some(&poor),
            empty: Some(&empty),
        };
        let read = |p: &Part| PageFacts::of(p, ocr).read;
        assert_eq!(read(&part(1, None)), ReadBy::Nothing);
        assert_eq!(read(&part(1, Some(Source::TextLayer))), ReadBy::TextLayer);
        assert_eq!(read(&part(1, Some(Source::Onnx))), ReadBy::TextLayer);
        assert_eq!(
            read(&part(1, Some(Source::Ocr))),
            ReadBy::Recognition { poor: false }
        );
        assert_eq!(
            read(&part(2, Some(Source::Ocr))),
            ReadBy::Recognition { poor: true }
        );
        // Recognition found no word: nothing was read there.
        assert_eq!(read(&part(3, Some(Source::Ocr))), ReadBy::Nothing);
        // Without recognition's facts, a recognised page is read well.
        assert_eq!(
            PageFacts::of(&part(2, Some(Source::Ocr)), Recognised::default()).read,
            ReadBy::Recognition { poor: false }
        );
    }
}
