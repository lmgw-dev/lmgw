//! The formats of the capability table ([`super`]): what the sniffers
//! tell apart, named as the wire and the page name them.

use super::super::super::sniff::{Kind, Sniffed};

/// A format as the table knows it: what [`super::super::sniff`] tells
/// apart, plus the PDF page that has no text layer (a page of a PDF, read
/// differently from the rest of it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Format {
    Text,
    Pdf,
    PdfPageWithoutText,
    Docx,
    Xlsx,
    Pptx,
    Odt,
    Ods,
    Odp,
    /// A legacy Excel workbook (BIFF, `.xls`).
    Xls,
    Html,
    Epub,
    /// An e-mail message (`.eml`).
    Email,
    Image,
    Audio,
    /// A legacy Word document (`.doc`: an OLE2 file with a `WordDocument`
    /// stream).
    Doc,
    /// A legacy PowerPoint deck (`.ppt`: `PowerPoint Document`).
    Ppt,
    /// Rich Text Format (`{\rtf`).
    Rtf,
    /// A Word document with OMML equations (`m:oMath` in its body, design
    /// §4.2's "docx with OMML equations", D11).
    DocxMath,
    /// LaTeX, by its name (`.tex`, `.latex`).
    Latex,
    /// reStructuredText, by its name (`.rst`).
    Rst,
    /// Org, by its name (`.org`).
    Org,
    /// MediaWiki markup, by its name (`.wiki`, `.mediawiki`).
    MediaWiki,
    /// DocBook XML, by its root element or name (`.dbk`).
    DocBook,
    /// FictionBook 2 (`<FictionBook`).
    Fb2,
    /// A video container (MP4/QuickTime, Matroska): its sound.
    Video,
}

/// The text markups pandoc reads that are plain text without it.
pub const MARKUPS: &[Format] = &[
    Format::Latex,
    Format::Rst,
    Format::Org,
    Format::MediaWiki,
    Format::DocBook,
    Format::Fb2,
];

impl Format {
    /// Every format, in the page's order.
    pub const ALL: &'static [Format] = &[
        Self::Text,
        Self::Docx,
        Self::DocxMath,
        Self::Xlsx,
        Self::Pptx,
        Self::Odt,
        Self::Ods,
        Self::Odp,
        Self::Doc,
        Self::Ppt,
        Self::Xls,
        Self::Rtf,
        Self::Latex,
        Self::Rst,
        Self::Org,
        Self::MediaWiki,
        Self::DocBook,
        Self::Fb2,
        Self::Html,
        Self::Epub,
        Self::Email,
        Self::Pdf,
        Self::PdfPageWithoutText,
        Self::Image,
        Self::Audio,
        Self::Video,
    ];

    /// The format of a sniffed file (a PDF as a whole; its pages without
    /// text are [`Format::PdfPageWithoutText`] once its text is read).
    pub fn of(s: &Sniffed) -> Self {
        match s.kind {
            Kind::Text => match s.sub {
                "html" => Self::Html,
                "eml" => Self::Email,
                "rtf" => Self::Rtf,
                "fb2" => Self::Fb2,
                "docbook" => Self::DocBook,
                _ => Self::Text,
            },
            Kind::Pdf => Self::Pdf,
            Kind::Image => Self::Image,
            // `sniff_convert` names a video container `video`.
            Kind::Audio if s.sub == "video" => Self::Video,
            Kind::Audio => Self::Audio,
            Kind::Office => match s.sub {
                "docx" => Self::Docx,
                "xlsx" => Self::Xlsx,
                "pptx" => Self::Pptx,
                "odt" => Self::Odt,
                "ods" => Self::Ods,
                "xls" => Self::Xls,
                "epub" => Self::Epub,
                "doc" => Self::Doc,
                "ppt" => Self::Ppt,
                "docx+math" => Self::DocxMath,
                // `sniff` names no other office format.
                _ => Self::Odp,
            },
        }
    }

    /// The wire spelling: `sniff`'s for a whole file, `pdf_page_without_text`
    /// for the page.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Pdf => "pdf",
            Self::PdfPageWithoutText => "pdf_page_without_text",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Xls => "xls",
            Self::Html => "html",
            Self::Epub => "epub",
            Self::Email => "eml",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Doc => "doc",
            Self::Ppt => "ppt",
            Self::Rtf => "rtf",
            Self::DocxMath => "docx_math",
            Self::Latex => "latex",
            Self::Rst => "rst",
            Self::Org => "org",
            Self::MediaWiki => "mediawiki",
            Self::DocBook => "docbook",
            Self::Fb2 => "fb2",
            Self::Video => "video",
        }
    }

    /// The text markup a text file's name says it is (`.tex`, `.rst`,
    /// `.org`, `.wiki`, `.dbk`, `.fb2`), as a web page's name says it is one
    /// (design §4.2: these are plain text to the sniffer).
    pub fn markup_by_name(name: &str) -> Option<Self> {
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())?
            .to_ascii_lowercase();
        Some(match ext.as_str() {
            "tex" | "latex" | "ltx" => Self::Latex,
            "rst" | "rest" => Self::Rst,
            "org" => Self::Org,
            "wiki" | "mediawiki" => Self::MediaWiki,
            "dbk" | "docbook" => Self::DocBook,
            "fb2" => Self::Fb2,
            _ => return None,
        })
    }
}
