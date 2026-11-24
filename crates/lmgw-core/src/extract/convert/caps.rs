//! The capability table (document-conversion design §4.1, §4.2): which
//! handler reads which format, what each needs, and the optional tools with
//! their packages. The one source both Settings › Documents
//! ([`super::report`]) and a conversion read through [`resolve`], so the two
//! cannot disagree.
//!
//! The handlers are the readers lmgw has today: text as it is, the office
//! formats through `office.rs`, PDFs through poppler, and, through lmgw's
//! own reader executable ([`ToolId::Reader`], `reader.rs`: a tool that is
//! installed with lmgw), legacy .xls, HTML, EPUB and e-mail; scanned pages
//! through tesseract, pictures through tesseract and the reader (WP4).
//! LibreOffice reads old Word and PowerPoint files (and RTF without pandoc)
//! by turning them into the files lmgw reads natively, pandoc reads RTF, the
//! text markups of §4.2 and Word files with equations, and audio and video
//! are transcribed by the chosen speech-to-text model, a video's sound taken
//! out by ffmpeg first (WP7, `libreoffice.rs`, `pandoc.rs`, `media.rs`). A
//! format with several rows is read by the first whose tools are all found
//! (D4); a later row that is a lesser reading (text as it is, a Word file
//! without its equations) says what the earlier one's tools would add
//! ([`Resolution::passed_over`]). A format lmgw sniffs but has no reader for
//! yet has a named [`NO_READER`] row.
//!
//! Every row with a foreign tool needs the reader too: it starts every tool
//! (`run/start.rs`, §14 performance round), so a missing reader is one named
//! state on every row it stops.

use lmgw_api_types::documents::Tier;

use super::tools::{Fact, ToolFacts};

mod format;
mod table;

/// An optional tool lmgw can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolId {
    /// lmgw's own reader executable (`lmgw-doc-reader`), installed with it.
    Reader,
    Poppler,
    Tesseract,
    Pandoc,
    Ffmpeg,
    LibreOffice,
}

impl ToolId {
    /// The wire spelling ([`lmgw_api_types::documents::ToolRow::id`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Poppler => "poppler",
            Self::Tesseract => "tesseract",
            Self::Pandoc => "pandoc",
            Self::Ffmpeg => "ffmpeg",
            Self::LibreOffice => "libreoffice",
        }
    }
}

pub use format::{Format, MARKUPS};
pub use table::*;

/// One way to read formats (design §4.1).
#[derive(Debug)]
pub struct Handler {
    /// `text.native`, `office.native`, `pdf.poppler`.
    pub id: &'static str,
    /// The formats it reads.
    pub formats: &'static [Format],
    /// Every tool it needs; empty: always available.
    pub needs: &'static [ToolId],
    pub tier: Tier,
    /// What it gives, in plain words.
    pub gives: &'static str,
}

/// The handlers, in preference order per format: the first whose tools are
/// all found reads it (D4: one handler per format at any moment).
pub const HANDLERS: &[Handler] = &[
    Handler {
        id: "text.native",
        formats: &[Format::Text],
        needs: &[],
        tier: Tier::Zero,
        gives: "always, as they are",
    },
    Handler {
        id: "office.native",
        formats: &[
            Format::Docx,
            Format::Xlsx,
            Format::Pptx,
            Format::Odt,
            Format::Ods,
            Format::Odp,
        ],
        needs: &[],
        tier: Tier::Zero,
        gives: "always, with headings, lists and tables",
    },
    Handler {
        id: "pdf.poppler",
        formats: &[Format::Pdf],
        needs: &[ToolId::Poppler, ToolId::Reader],
        tier: Tier::Zero,
        gives: "text, headings, columns and tables",
    },
    Handler {
        id: "xls.calamine",
        formats: &[Format::Xls],
        needs: &[ToolId::Reader],
        tier: Tier::Zero,
        gives: "each sheet as a table",
    },
    Handler {
        id: "html.native",
        formats: &[Format::Html],
        needs: &[ToolId::Reader],
        tier: Tier::Zero,
        gives: "headings, lists, links and tables; scripts and remote content left out",
    },
    Handler {
        id: "epub.native",
        formats: &[Format::Epub],
        needs: &[ToolId::Reader],
        tier: Tier::Zero,
        gives: "chapter by chapter in reading order",
    },
    Handler {
        id: "email.native",
        formats: &[Format::Email],
        needs: &[ToolId::Reader],
        tier: Tier::Zero,
        gives: "sender, recipients, date, subject and the message, with its attachments read \
                like files of their own",
    },
    Handler {
        id: "ocr.tesseract",
        formats: &[Format::PdfPageWithoutText],
        needs: &[ToolId::Tesseract, ToolId::Reader],
        tier: Tier::Zero,
        gives: "text recognition, with columns and tables from the words' positions",
    },
    // A picture's size is read, and a TIFF's pages are decoded, by lmgw's
    // reader before tesseract sees them.
    Handler {
        id: "image.tesseract",
        formats: &[Format::Image],
        needs: &[ToolId::Tesseract, ToolId::Reader],
        tier: Tier::Zero,
        gives: "text recognition, with columns and tables from the words' positions",
    },
    // WP7. LibreOffice turns the file into a docx or pptx, which lmgw's own
    // reader reads (`libreoffice.rs`); Writer reads `.doc`, Impress `.ppt`
    // ([`part_needed`]).
    Handler {
        id: "legacy.libreoffice",
        formats: &[Format::Doc, Format::Ppt],
        needs: &[ToolId::LibreOffice, ToolId::Reader],
        tier: Tier::Zero,
        gives: "through LibreOffice, with headings, lists, tables and pictures",
    },
    // pandoc writes HTML, which lmgw's reader writes as markdown
    // (`pandoc.rs`): its tables, lists and code as a web page's.
    Handler {
        id: "docx_math.pandoc",
        formats: &[Format::DocxMath],
        needs: &[ToolId::Pandoc, ToolId::Reader],
        tier: Tier::Zero,
        gives: "through pandoc, equations as LaTeX, with headings, lists and tables",
    },
    Handler {
        id: "docx_math.native",
        formats: &[Format::DocxMath],
        needs: &[],
        tier: Tier::Zero,
        gives: "with headings, lists, tables and pictures; equations only as their characters \
                (pandoc reads them as LaTeX)",
    },
    Handler {
        id: "rtf.pandoc",
        formats: &[Format::Rtf],
        needs: &[ToolId::Pandoc, ToolId::Reader],
        tier: Tier::Zero,
        gives: "through pandoc, with headings, lists and tables",
    },
    Handler {
        id: "rtf.libreoffice",
        formats: &[Format::Rtf],
        needs: &[ToolId::LibreOffice, ToolId::Reader],
        tier: Tier::Zero,
        gives: "through LibreOffice, with headings, lists, tables and pictures",
    },
    Handler {
        id: "markup.pandoc",
        formats: MARKUPS,
        needs: &[ToolId::Pandoc, ToolId::Reader],
        tier: Tier::Zero,
        gives: "through pandoc, with headings, lists, tables and code",
    },
    Handler {
        id: "markup.text",
        formats: MARKUPS,
        needs: &[],
        tier: Tier::Zero,
        gives: "as plain text, markup included (pandoc reads their headings, lists and tables)",
    },
    // The speech-to-text model chosen under Settings › Documents transcribes
    // them as the calling key (`media.rs`); none chosen, they fail by name.
    Handler {
        id: "audio.asr",
        formats: &[Format::Audio],
        needs: &[],
        tier: Tier::Model,
        gives: "transcribed by the speech-to-text model chosen below",
    },
    Handler {
        id: "video.ffmpeg",
        formats: &[Format::Video],
        needs: &[ToolId::Ffmpeg, ToolId::Reader],
        tier: Tier::Model,
        gives: "their sound, taken out by ffmpeg and transcribed by the speech-to-text model \
                chosen below",
    },
];

/// A format lmgw recognises and has no reader for yet: the table's named
/// "no reader yet" row (design §4.1), with why in plain words.
#[derive(Debug)]
pub struct NoReader {
    pub format: Format,
    pub why: &'static str,
}

pub const NO_READER: &[NoReader] = &[];

/// The page's rows: formats that read alike, under one plain name.
#[derive(Debug)]
pub struct Group {
    /// `text`, `office`, `pdf`, `scans`, `audio`.
    pub id: &'static str,
    pub label: &'static str,
    pub formats: &'static [Format],
}

pub const GROUPS: &[Group] = &[
    Group {
        id: "text",
        label: "Text, Markdown, CSV and other plain text",
        formats: &[Format::Text],
    },
    Group {
        id: "office",
        label: "Word, PowerPoint, Excel, OpenDocument",
        formats: &[
            Format::Docx,
            Format::Pptx,
            Format::Xlsx,
            Format::Odt,
            Format::Odp,
            Format::Ods,
        ],
    },
    Group {
        id: "equations",
        label: "Word files with equations",
        formats: &[Format::DocxMath],
    },
    Group {
        id: "legacy_word",
        label: "Old Word files (.doc)",
        formats: &[Format::Doc],
    },
    Group {
        id: "legacy_powerpoint",
        label: "Old PowerPoint files (.ppt)",
        formats: &[Format::Ppt],
    },
    Group {
        id: "xls",
        label: "Old Excel files (.xls)",
        formats: &[Format::Xls],
    },
    Group {
        id: "rtf",
        label: "Rich Text (RTF)",
        formats: &[Format::Rtf],
    },
    Group {
        id: "markup",
        label: "LaTeX, reStructuredText, Org, MediaWiki, DocBook, FictionBook",
        formats: MARKUPS,
    },
    Group {
        id: "html",
        label: "Web pages (HTML)",
        formats: &[Format::Html],
    },
    Group {
        id: "epub",
        label: "E-books (EPUB)",
        formats: &[Format::Epub],
    },
    Group {
        id: "email",
        label: "E-mail (.eml)",
        formats: &[Format::Email],
    },
    Group {
        id: "pdf",
        label: "PDF",
        formats: &[Format::Pdf],
    },
    Group {
        id: "scans",
        label: "Scanned PDF pages",
        formats: &[Format::PdfPageWithoutText],
    },
    Group {
        id: "pictures",
        label: "Pictures of text (PNG, JPEG, GIF, WebP, TIFF)",
        formats: &[Format::Image],
    },
    Group {
        id: "audio",
        label: "Audio",
        formats: &[Format::Audio],
    },
    Group {
        id: "video",
        label: "Videos (their sound)",
        formats: &[Format::Video],
    },
];

/// Why `tool` cannot be run on this system yet, when its install row for it
/// says there is nothing to install yet and the runner has no way to limit
/// its memory (`run::limit_address_space` refuses every run off Unix until
/// the job-object limit exists): a `pdftotext` found on a Windows `PATH`
/// would otherwise show as reading PDFs while every run fails.
fn not_run_here(tool: ToolId) -> Option<&'static str> {
    if cfg!(unix) {
        return None;
    }
    match self::tool(tool).packages.windows {
        Install::NotYet(why) => Some(why),
        _ => None,
    }
}

/// What reads `format` here: the first handler whose tools are all found,
/// or none, with what the preferred one is missing, or the format's "no
/// reader yet" reason.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub handler: Option<&'static Handler>,
    /// The tools the preferred handler needs that are not usable here
    /// (missing, or their probe failed). Empty when a handler reads it.
    pub missing: Vec<ToolId>,
    /// No handler exists for it yet: why, in plain words.
    pub no_reader: Option<&'static str>,
    /// The tools whose absence passed over an earlier row for the one
    /// that reads it (pandoc for a Word file with equations read without
    /// them): what installing them would add. Empty when the first row
    /// reads it.
    pub passed_over: Vec<ToolId>,
}

/// The part of LibreOffice a format needs (its launcher is how the probe
/// tells the parts apart, [`COMPONENTS`]): Writer for documents, Impress
/// for decks.
pub fn part_needed(format: Format) -> Option<&'static str> {
    match format {
        Format::Doc | Format::Rtf | Format::Docx | Format::Odt => Some("Writer"),
        Format::Ppt | Format::Pptx | Format::Odp => Some("Impress"),
        _ => None,
    }
}

/// Whether `tool` is usable for `format`: found, and for LibreOffice with
/// the part the format needs (a probe that read no parts, as a test's
/// fake, is taken at its word).
fn usable(tool: ToolId, format: Format, facts: &ToolFacts) -> bool {
    let Some(Fact::Found(f)) = facts.fact(tool) else {
        return false;
    };
    if tool != ToolId::LibreOffice {
        return true;
    }
    let Some(part) = part_needed(format) else {
        return true;
    };
    match f
        .extras
        .iter()
        .find(|e| e.kind == lmgw_api_types::documents::ExtraKind::Components)
    {
        Some(e) => e.present.iter().any(|p| p == part),
        None => true,
    }
}

/// Resolve `format` against what was found (design §4.1). The page renders
/// this for every format; a conversion acts on it for the format it
/// sniffed.
pub fn resolve(format: Format, facts: &ToolFacts) -> Resolution {
    let candidates: Vec<&'static Handler> = HANDLERS
        .iter()
        .filter(|h| h.formats.contains(&format))
        .collect();
    let Some(first) = candidates.first().copied() else {
        return Resolution {
            handler: None,
            missing: Vec::new(),
            no_reader: NO_READER.iter().find(|n| n.format == format).map(|n| n.why),
            passed_over: Vec::new(),
        };
    };
    let mut passed_over: Vec<ToolId> = Vec::new();
    for h in &candidates {
        // A tool the runner cannot run here yet reads nothing, found or not.
        if h.needs.iter().any(|t| not_run_here(*t).is_some()) {
            continue;
        }
        let lacking: Vec<ToolId> = h
            .needs
            .iter()
            .copied()
            .filter(|t| !usable(*t, format, facts))
            .collect();
        if lacking.is_empty() {
            return Resolution {
                handler: Some(h),
                missing: Vec::new(),
                no_reader: None,
                passed_over,
            };
        }
        for t in lacking {
            if !passed_over.contains(&t) {
                passed_over.push(t);
            }
        }
    }
    if let Some(why) = first.needs.iter().find_map(|t| not_run_here(*t)) {
        return Resolution {
            handler: None,
            missing: Vec::new(),
            no_reader: Some(why),
            passed_over: Vec::new(),
        };
    }
    Resolution {
        handler: None,
        missing: first
            .needs
            .iter()
            .copied()
            .filter(|t| !usable(*t, format, facts))
            .collect(),
        no_reader: None,
        passed_over: Vec::new(),
    }
}

#[cfg(test)]
mod tests_wp7;

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::InstallPlatform;
    use std::collections::BTreeSet;

    /// Design §4.1's walk: every `needs` names a known tool, every format
    /// has a handler or a named "no reader yet" row (never both), every
    /// format is on the page exactly once, and the formats of one page row
    /// resolve alike, so the row can speak for them.
    #[test]
    fn the_table_is_whole() {
        for id in [
            ToolId::Reader,
            ToolId::Poppler,
            ToolId::Tesseract,
            ToolId::Pandoc,
            ToolId::Ffmpeg,
            ToolId::LibreOffice,
        ] {
            assert_eq!(TOOLS.iter().filter(|t| t.id == id).count(), 1, "{id:?}");
        }
        let ids: BTreeSet<&str> = HANDLERS.iter().map(|h| h.id).collect();
        assert_eq!(ids.len(), HANDLERS.len(), "a handler id repeats");
        for h in HANDLERS {
            for t in h.needs {
                assert!(TOOLS.iter().any(|x| x.id == *t), "{} needs {t:?}", h.id);
            }
            assert!(!h.gives.is_empty(), "{}", h.id);
        }
        for f in Format::ALL {
            let handled = HANDLERS.iter().any(|h| h.formats.contains(f));
            let named = NO_READER.iter().filter(|n| n.format == *f).count();
            assert!(
                handled ^ (named == 1),
                "{f:?}: a handler or one named no-reader row, not both"
            );
            let on_page = GROUPS.iter().filter(|g| g.formats.contains(f)).count();
            assert_eq!(on_page, 1, "{f:?} is on the page {on_page} times");
        }
        for g in GROUPS {
            let firsts: BTreeSet<Option<&str>> = g
                .formats
                .iter()
                .map(|f| {
                    HANDLERS
                        .iter()
                        .find(|h| h.formats.contains(f))
                        .map(|h| h.id)
                })
                .collect();
            assert_eq!(firsts.len(), 1, "{}: its formats resolve apart", g.id);
        }
        for t in TOOLS {
            assert!(!t.probe.binaries.is_empty(), "{:?}", t.id);
            assert!(!t.packages.fedora.is_empty(), "{:?}", t.id);
        }
    }

    #[test]
    fn a_pdf_needs_poppler_and_says_so_when_it_is_missing() {
        let none = ToolFacts::all_missing();
        let r = resolve(Format::Pdf, &none);
        assert!(r.handler.is_none());
        // lmgw's reader starts poppler (`run/start.rs`).
        assert_eq!(r.missing, vec![ToolId::Poppler, ToolId::Reader]);
        assert_eq!(r.no_reader, None);
        let reader = ToolFacts::all_missing().with_found(ToolId::Reader);
        assert_eq!(resolve(Format::Pdf, &reader).missing, vec![ToolId::Poppler]);

        let with = reader.with_found(ToolId::Poppler);
        let r = resolve(Format::Pdf, &with);
        assert_eq!(r.handler.map(|h| h.id), Some("pdf.poppler"));
        assert!(r.missing.is_empty());
    }

    #[test]
    fn native_readers_need_nothing_and_unread_formats_say_why() {
        let none = ToolFacts::all_missing();
        assert_eq!(
            resolve(Format::Docx, &none).handler.map(|h| h.id),
            Some("office.native")
        );
        assert_eq!(
            resolve(Format::Text, &none).handler.map(|h| h.id),
            Some("text.native")
        );
        // The formats lmgw's reader parses need it, and say so without it.
        let with = ToolFacts::all_missing().with_found(ToolId::Reader);
        for f in [Format::Xls, Format::Html, Format::Epub, Format::Email] {
            let r = resolve(f, &none);
            assert!(r.handler.is_none(), "{f:?}");
            if cfg!(unix) {
                assert_eq!(r.missing, vec![ToolId::Reader], "{f:?}");
            }
            assert_eq!(resolve(f, &with).handler.is_some(), cfg!(unix), "{f:?}");
        }
        // Audio's reader is the speech-to-text model (WP7): the handler is
        // there, the model's choice is the file's own check (`media.rs`).
        let audio = resolve(Format::Audio, &none);
        assert_eq!(audio.handler.map(|h| h.id), Some("audio.asr"));
    }

    #[test]
    fn scans_need_tesseract_and_pictures_the_reader_too() {
        let r = resolve(Format::PdfPageWithoutText, &ToolFacts::all_missing());
        assert_eq!(
            (r.handler.is_none(), r.missing.clone()),
            (true, vec![ToolId::Tesseract, ToolId::Reader])
        );
        let with = ToolFacts::all_missing()
            .with_found(ToolId::Tesseract)
            .with_found(ToolId::Reader);
        assert_eq!(
            resolve(Format::PdfPageWithoutText, &with)
                .handler
                .map(|h| h.id),
            Some("ocr.tesseract")
        );
        if cfg!(unix) {
            let tesseract = ToolFacts::all_missing().with_found(ToolId::Tesseract);
            assert_eq!(
                resolve(Format::Image, &tesseract).missing,
                vec![ToolId::Reader]
            );
            assert_eq!(
                resolve(Format::Image, &with).handler.map(|h| h.id),
                Some("image.tesseract")
            );
        }
    }

    #[test]
    fn sniffed_files_map_onto_the_table() {
        use super::super::super::sniff::sniff;
        let doc = super::super::super::test_files::docx(&["x"]);
        assert_eq!(Format::of(&sniff(&doc).unwrap()), Format::Docx);
        assert_eq!(Format::of(&sniff(b"%PDF-1.7\n").unwrap()), Format::Pdf);
        assert_eq!(Format::of(&sniff(b"plain").unwrap()), Format::Text);
        assert_eq!(
            Format::of(&sniff(b"\x89PNG\r\n\x1a\n").unwrap()),
            Format::Image
        );
    }

    #[test]
    fn the_server_image_installs_with_dnf_too() {
        let p = tool(ToolId::Ffmpeg).packages;
        let (i, note) = install_hint(&p, InstallPlatform::ServerImage);
        assert!(matches!(i, Install::Command("ffmpeg-free")));
        assert!(note.contains("RPM Fusion"));
        let (i, _) = install_hint(&tool(ToolId::Poppler).packages, InstallPlatform::Windows);
        assert!(matches!(i, Install::NotYet(w) if w.contains("not built yet")));
    }
}
