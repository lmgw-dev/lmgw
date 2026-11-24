//! docling's `from_formats` (document-conversion design §7.4.2): a file
//! whose format is not in the list is `skipped`, as docling skips it. The
//! format is lmgw's sniffed one, named as docling names it; a text file is
//! told apart by its name's extension as docling does (`.csv`, `.html`,
//! `.adoc`, `.vtt`, `.tex`; anything else is `md`). A file lmgw cannot
//! sniff is converted and fails by its own error.

use lmgw_api_types::documents::convert::{ConvertStats, ConvertedDocument, DocStatus};

use crate::extract::convert::caps::Format;
use crate::extract::convert::file::Input;
use crate::extract::sniff;

/// docling's `InputFormat` for a sniffed `format` with file name `name`.
pub fn docling_format(format: Format, name: &str) -> &'static str {
    match format {
        Format::Text => {
            let ext = std::path::Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            match ext.as_str() {
                "csv" => "csv",
                "html" | "htm" | "xhtml" => "html",
                "adoc" | "asciidoc" | "asc" => "asciidoc",
                "vtt" => "vtt",
                "tex" | "latex" => "latex",
                _ => "md",
            }
        }
        Format::Pdf | Format::PdfPageWithoutText => "pdf",
        Format::Docx => "docx",
        Format::Xlsx => "xlsx",
        Format::Pptx => "pptx",
        Format::Odt => "odt",
        Format::Ods => "ods",
        Format::Odp => "odp",
        Format::Xls => "xls",
        Format::Html => "html",
        Format::Epub => "epub",
        Format::Email => "email",
        Format::Image => "image",
        // docling's audio format takes videos too (their sound).
        Format::Audio | Format::Video => "audio",
        Format::DocxMath => "docx",
        Format::Latex => "latex",
        // Formats docling has no reader for, by their own names: a
        // `from_formats` list skips them.
        Format::Doc => "doc",
        Format::Ppt => "ppt",
        Format::Rtf => "rtf",
        Format::Rst => "rst",
        Format::Org => "org",
        Format::MediaWiki => "mediawiki",
        Format::DocBook => "docbook",
        Format::Fb2 => "fb2",
    }
}

/// `inputs` split into the files to convert and the skipped ones' entries.
pub async fn split(inputs: Vec<Input>, allowed: &[String]) -> (Vec<Input>, Vec<ConvertedDocument>) {
    let mut keep = Vec::new();
    let mut skipped = Vec::new();
    for i in inputs {
        let format = match &i.bytes {
            Ok(b) => sniff::sniff_convert_async(b.clone())
                .await
                .ok()
                .map(|s| Format::of(&s)),
            Err(_) => None,
        };
        match format.map(|f| (f, docling_format(f, &i.name))) {
            Some((f, d)) if !allowed.iter().any(|a| a == d) => skipped.push(ConvertedDocument {
                index: i.index,
                name: i.name,
                status: DocStatus::Skipped,
                format: Some(f.as_str().to_string()),
                parts: 0,
                content: None,
                structure: None,
                pictures: Vec::new(),
                text: None,
                warnings: Vec::new(),
                stats: ConvertStats::default(),
                error: None,
            }),
            _ => keep.push(i),
        }
    }
    (keep, skipped)
}
