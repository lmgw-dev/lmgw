use lmgw_api_types::documents::convert::ConvertOptions;
use lmgw_api_types::documents::InstallPlatform;

use super::*;
use crate::extract::convert::options;
use crate::extract::convert::tools::{DocTools, Fact};
use crate::extract::test_files;

/// [`super::convert`] with a request budget of its own.
async fn convert(input: Input, plan: &Plan, facts: &ToolFacts) -> Done {
    super::convert(input, plan, facts, &Arc::new(OutputBudget::request())).await
}

fn plan_of(o: ConvertOptions) -> Plan {
    options::plan(&o, InstallPlatform::Fedora).unwrap().0
}

fn input(bytes: Vec<u8>) -> Input {
    Input {
        index: 0,
        name: "example.bin".into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

/// Nothing installed but lmgw's own reader executable, which cargo builds
/// beside the test binaries: what the formats it parses need. Its absence
/// is a broken build, never a skipped test.
#[cfg(unix)]
async fn reader_only() -> std::sync::Arc<ToolFacts> {
    let facts = DocTools::on_path_with_own(std::ffi::OsString::new())
        .current()
        .await;
    assert!(
        matches!(facts.fact(ToolId::Reader), Some(Fact::Found(_))),
        "lmgw-doc-reader was not found beside this test binary ({:?}); `cargo test \
         --workspace` and `cargo nextest run --workspace` build it",
        facts.fact(ToolId::Reader)
    );
    facts
}

/// What this machine has, as a conversion sees it; `None` without poppler
/// (`ci/check.sh` says up front which document tools the suite found).
async fn with_poppler() -> Option<std::sync::Arc<ToolFacts>> {
    let facts = DocTools::on_path_with_own(std::env::var_os("PATH").unwrap_or_default())
        .current()
        .await;
    if matches!(facts.fact(ToolId::Poppler), Some(Fact::Found(_))) {
        Some(facts)
    } else {
        eprintln!("skipped: poppler is not on PATH");
        None
    }
}

#[test]
fn every_handler_of_the_table_is_wired_to_a_reader() {
    for h in caps::HANDLERS {
        assert!(
            [
                "text.native",
                "office.native",
                "pdf.poppler",
                "ocr.tesseract",
                "image.tesseract",
                "xls.calamine",
                "html.native",
                "epub.native",
                "email.native",
                "legacy.libreoffice",
                "docx_math.pandoc",
                "docx_math.native",
                "rtf.pandoc",
                "rtf.libreoffice",
                "markup.pandoc",
                "markup.text",
                "audio.asr",
                "video.ffmpeg"
            ]
            .contains(&h.id),
            "{} has no reader in file.rs",
            h.id
        );
    }
}

#[tokio::test]
async fn a_text_file_comes_back_as_it_is() {
    let none = ToolFacts::all_missing();
    let d = convert(
        input(b"# Notes\n\nplain text".to_vec()),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted);
    assert_eq!(d.format.as_deref(), Some("text"));
    assert_eq!(d.content.as_deref(), Some("# Notes\n\nplain text"));
    assert_eq!(d.stats.parts_by_source.native, 1);
    assert_eq!(d.stats.headings[0], 1);
}

#[tokio::test]
async fn a_workbook_is_markdown_tables_or_plain_lines() {
    let none = ToolFacts::all_missing();
    let book = test_files::xlsx(&[("Budget", &[&["Item", "Cost"], &["Paper", "n:3"]])]);
    let d = convert(input(book.clone()), &plan_of(Default::default()), &none)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.format.as_deref(), Some("xlsx"));
    let md = d.content.unwrap();
    assert!(md.contains("## Sheet: Budget"), "{md}");
    assert!(md.contains("| --- |"), "{md}");
    assert_eq!((d.stats.tables, d.stats.headings[1]), (1, 1));
    assert!(d.stats.table_cells > 0);

    let text = plan_of(ConvertOptions {
        output: Output::Text,
        ..Default::default()
    });
    let t = convert(input(book.clone()), &text, &none)
        .await
        .doc
        .content
        .unwrap();
    assert!(t.starts_with("Sheet: Budget"), "{t}");
    assert!(!t.contains('|') && t.contains("Paper\t3"), "{t}");

    let compact = plan_of(ConvertOptions {
        compact_tables: true,
        ..Default::default()
    });
    let c = convert(input(book), &compact, &none)
        .await
        .doc
        .content
        .unwrap();
    assert!(c.contains("|---|"), "{c}");
}

#[tokio::test]
async fn pages_on_a_file_without_pages_reads_it_whole_and_says_so() {
    let none = ToolFacts::all_missing();
    let p = plan_of(ConvertOptions {
        pages: Some("2".into()),
        ..Default::default()
    });
    let d = convert(input(test_files::docx(&["Hello"])), &p, &none)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Converted);
    assert_eq!(d.content.as_deref(), Some("Hello"));
    assert_eq!(d.warnings[0].code, WarningCode::PagesNotApplicable);
}

#[tokio::test]
async fn a_pdf_without_poppler_fails_naming_the_package_and_its_command() {
    let none = ToolFacts::all_missing();
    let pdf = test_files::pdf(&[Some("Hello")]);
    let d = convert(input(pdf), &plan_of(Default::default()), &none)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Failed);
    let e = d.error.unwrap();
    assert_eq!(e.code, ErrorCode::ToolMissing);
    assert_eq!(e.tool.as_deref(), Some("poppler"));
    assert!(
        e.message.contains("install poppler-utils")
            && e.message.contains("sudo dnf install poppler-utils"),
        "{}",
        e.message
    );
    assert_eq!(e.code.status(), 415);
}

#[tokio::test]
async fn files_without_a_reader_or_a_format_fail_alone_with_their_code() {
    let none = ToolFacts::all_missing();
    let plan = plan_of(Default::default());
    let png = convert(input(b"\x89PNG\r\n\x1a\nrest".to_vec()), &plan, &none)
        .await
        .doc;
    let e = png.error.unwrap();
    assert_eq!(
        (e.code, png.format.as_deref()),
        (ErrorCode::ToolMissing, Some("image"))
    );
    assert_eq!(e.tool.as_deref(), Some("tesseract"));
    assert!(
        e.message.contains("sudo dnf install tesseract"),
        "{}",
        e.message
    );
    let wav = convert(input(b"RIFF\0\0\0\0WAVE".to_vec()), &plan, &none)
        .await
        .doc;
    // Audio's reader is the speech-to-text model (WP7); none is chosen.
    assert_eq!(wav.error.unwrap().code, ErrorCode::ModelNotChosen);
    let junk = convert(input(vec![0, 159, 146, 150, 0, 1]), &plan, &none)
        .await
        .doc;
    assert_eq!(junk.error.unwrap().code, ErrorCode::Unsupported);
    let bad = Input {
        bytes: Err("data_base64 is not base64".into()),
        ..input(Vec::new())
    };
    let d = convert(bad, &plan, &none).await.doc;
    assert_eq!(d.error.unwrap().code, ErrorCode::InvalidBase64);
    // A docx whose document is not XML.
    let broken = test_files::docx_xml("<w:p><w:r><w:t>unclosed", None);
    let broken = convert(input(broken), &plan, &none).await.doc;
    assert_eq!(broken.status, DocStatus::Failed);
    assert_eq!(broken.error.unwrap().code, ErrorCode::Unreadable);
}

#[tokio::test]
async fn a_pdf_s_pages_without_text_make_it_partial_and_are_listed() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let pdf = test_files::pdf(&[Some("First page"), None, Some("Third page")]);
    let off = plan_of(ConvertOptions {
        ocr: lmgw_api_types::documents::convert::Ocr::Off,
        ..Default::default()
    });
    let d = convert(input(pdf.clone()), &off, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Partial, "{d:?}");
    assert_eq!(d.parts, 3);
    assert_eq!(d.stats.parts_by_source.text_layer, 2);
    let w = &d.warnings[0];
    assert_eq!((w.code, w.parts.clone()), (WarningCode::NoReader, vec![2]));
    let md = d.content.unwrap();
    assert!(md.starts_with("First page\n\nThird page"), "{md}");

    let p = plan_of(ConvertOptions {
        pages: Some("3,1".into()),
        page_break: Some("<!-- page -->".into()),
        ..Default::default()
    });
    let d = convert(input(pdf.clone()), &p, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(
        d.content.as_deref(),
        Some("First page\n\n<!-- page -->\n\nThird page")
    );

    let p = plan_of(ConvertOptions {
        pages: Some("3-9".into()),
        ..Default::default()
    });
    let d = convert(input(pdf.clone()), &p, &facts).await.doc;
    assert_eq!(d.parts, 1);
    assert_eq!(d.warnings[0].code, WarningCode::PagesOutOfRange);
    assert!(d.warnings[0].message.contains("4-9"), "{:?}", d.warnings);

    let p = plan_of(ConvertOptions {
        pages: Some("7".into()),
        ..Default::default()
    });
    let d = convert(input(pdf), &p, &facts).await.doc;
    assert_eq!(d.error.unwrap().code, ErrorCode::PagesOutOfRange);
}

#[tokio::test]
async fn a_pdf_with_no_text_at_all_fails_rather_than_answering_nothing() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let scan = test_files::pdf(&[None, None]);
    let off = plan_of(ConvertOptions {
        ocr: lmgw_api_types::documents::convert::Ocr::Off,
        ..Default::default()
    });
    let d = convert(input(scan), &off, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Failed);
    let e = d.error.unwrap();
    assert_eq!(e.code, ErrorCode::NoReader);
    assert!(
        e.message.contains("pages 1-2 have no text"),
        "{}",
        e.message
    );
    let broken = convert(
        input(b"%PDF-1.4 this is not a pdf".to_vec()),
        &plan_of(Default::default()),
        &facts,
    )
    .await
    .doc;
    assert_eq!(broken.error.unwrap().code, ErrorCode::Unreadable);
    assert_eq!(broken.format.as_deref(), Some("pdf"));
}

#[tokio::test]
async fn options_that_change_nothing_for_a_file_say_so() {
    let none = ToolFacts::all_missing();
    let p = plan_of(ConvertOptions {
        page_break: Some("<!-- page -->".into()),
        ..Default::default()
    });
    let d = convert(input(test_files::docx(&["Hello"])), &p, &none)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Converted);
    let w = &d.warnings[0];
    assert_eq!(w.code, WarningCode::OptionNotApplicable);
    assert_eq!(w.option.as_deref(), Some("page_break"));
    // And with no page_break there is nothing to say.
    let d = convert(input(b"hi".to_vec()), &plan_of(Default::default()), &none)
        .await
        .doc;
    assert!(d.warnings.is_empty());
}

/// A reader that panics is that file's `unreadable`; the slot comes back and
/// the next step still runs.
#[tokio::test]
async fn a_reader_that_panics_fails_its_file_and_nothing_else() {
    let f = on_slot(Some(Format::Docx), || -> u32 { panic!("a reader bug") })
        .await
        .expect_err("the panic is the file's failure");
    assert_eq!(f.error.code, ErrorCode::Unreadable);
    assert!(
        f.error.message.contains("stopped unexpectedly")
            && !f.error.message.contains("a reader bug"),
        "{}",
        f.error.message
    );
    assert_eq!(f.format, Some(Format::Docx));
    assert_eq!(on_slot(None, || 7).await.ok(), Some(7));
}

/// A PDF of 100 000 pages, nearly all without text: the layout and the
/// assembly are linear and the warning names ranges, not every page.
#[test]
fn many_pages_without_text_are_assembled_in_linear_time_and_named_as_ranges() {
    let n = 100_000u32;
    let has_text = |p: u32| p == 1 || p == 901;
    let mut xml = String::from("<html><body><doc>");
    for p in 1..=n {
        xml.push_str(r#"<page width="300" height="200">"#);
        if has_text(p) {
            xml.push_str(&format!(
                r#"<flow><block xMin="20" yMin="90" xMax="120" yMax="104"><line xMin="20" yMin="90" xMax="120" yMax="104"><word xMin="20" yMin="90" xMax="60" yMax="104">page</word><word xMin="64" yMin="90" xMax="120" yMax="104">{p}</word></line></block></flow>"#
            ));
        }
        xml.push_str("</page>");
    }
    xml.push_str("</doc></body></html>");
    let plan = plan_of(ConvertOptions {
        pages: Some("1-100000,1-100000".into()),
        ..Default::default()
    });
    let started = std::time::Instant::now();
    let (a, note) =
        pdf::assemble(xml.as_bytes(), pdf::Fonts::Missing, 1, Some((1, n)), &plan).unwrap();
    assert!(!note.is_empty(), "the missing pdftohtml is said");
    let r = pdf::words(a, &plan, &ToolFacts::all_missing(), Default::default())
        .ok()
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(r.partial);
    assert_eq!(r.by_source.text_layer, 2);
    assert_eq!(r.parts, 100_000);
    let w = &r.warnings[0];
    assert!(
        w.message
            .starts_with("pages 2-900, 902-100000 have no text layer"),
        "{}",
        &w.message[..w.message.len().min(120)]
    );
    assert!(w.message.len() < 400, "{}", w.message.len());
}

#[tokio::test]
async fn json_gives_office_files_and_text_as_parts_of_blocks() {
    let none = ToolFacts::all_missing();
    let json = plan_of(ConvertOptions {
        output: Output::Json,
        ..Default::default()
    });
    let book = test_files::xlsx(&[("Budget", &[&["Item", "Cost"], &["Paper", "n:3"]])]);
    let d = convert(input(book), &json, &none).await.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert!(d.content.is_none());
    let parts = d.structure.unwrap();
    let v = serde_json::to_value(&parts).unwrap();
    assert_eq!(v[0]["kind"], "sheet");
    assert_eq!(v[0]["name"], "Budget");
    assert_eq!(v[0]["blocks"][0]["kind"], "table");
    let cells = v[0]["blocks"][0]["table"]["cells"].as_array().unwrap();
    assert_eq!(cells.last().unwrap()["text"], "3", "{cells:?}");
    assert!(cells.iter().any(|c| c["text"] == "Paper"), "{cells:?}");
    assert_eq!(v[0]["blocks"][0]["bbox"], serde_json::Value::Null);
    assert_eq!(d.stats.tables, 1);
    // The sheet's own heading counts as in markdown.
    assert_eq!(d.stats.headings[1], 1);

    let deck = test_files::pptx(&[&["Hello"], &["World"]]);
    let d = convert(input(deck), &json, &none).await.doc;
    let v = serde_json::to_value(d.structure.unwrap()).unwrap();
    assert_eq!(
        (v[0]["kind"].as_str(), v[1]["number"].as_u64()),
        (Some("slide"), Some(2))
    );

    let mut md = input(b"# Notes\n\nplain text".to_vec());
    md.name = "notes.md".into();
    let d = convert(md, &json, &none).await.doc;
    let v = serde_json::to_value(d.structure.unwrap()).unwrap();
    assert_eq!(v[0]["kind"], "document");
    assert_eq!(v[0]["blocks"][0]["kind"], "heading");
    assert_eq!(v[0]["blocks"][0]["level"], 1);
    assert_eq!(v[0]["blocks"][1]["text"], "plain text");
    assert_eq!(v[0]["blocks"][1]["source"], "native");
    assert_eq!(d.stats.headings[0], 1);
}

#[tokio::test]
async fn a_pdf_as_json_has_its_pages_with_boxes_and_its_unread_page_marked() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let json = plan_of(ConvertOptions {
        output: Output::Json,
        ocr: lmgw_api_types::documents::convert::Ocr::Off,
        ..Default::default()
    });
    let pdf = test_files::pdf(&[Some("First page"), None, Some("Third page")]);
    let d = convert(input(pdf), &json, &facts).await.doc;
    assert_eq!(d.status, DocStatus::Partial, "{d:?}");
    let v = serde_json::to_value(d.structure.unwrap()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!(v[0]["kind"], "page");
    assert_eq!(
        (v[0]["width_pt"].as_f64(), v[0]["height_pt"].as_f64()),
        (Some(300.0), Some(200.0))
    );
    assert_eq!(v[0]["blocks"][0]["text"], "First page");
    assert_eq!(v[0]["blocks"][0]["source"], "text_layer");
    assert_eq!(v[0]["blocks"][0]["bbox"].as_array().unwrap().len(), 4);
    assert_eq!(v[1]["source"], serde_json::Value::Null);
    assert_eq!(v[1]["blocks"], serde_json::json!([]));
    assert_eq!(v[2]["number"], 3);
}

mod built;
// They run the real reader executable and Unix tools.
#[cfg(unix)]
mod hostile;
mod json;
// Real poppler runs under limits of the test's own, on Unix.
#[cfg(unix)]
mod memory;
// They run the reader executable, which the runner starts on Unix only yet.
#[cfg(unix)]
mod more;
mod ocr;
// Text recognition beside picture export, and the reader's formats' pictures
// and budget (WP4 on WP10): stand-in tools and the reader, on Unix.
#[cfg(unix)]
mod ocr_formats;
#[cfg(unix)]
mod ocr_pictures;
mod pdf_notes;
mod pictures;
mod pictures_linked;
// The side-by-side paths through a pool whose memory admits fewer PDF steps
// than its slots (performance round review B1): poppler, tesseract and the
// reader, on Unix.
#[cfg(unix)]
mod small_pool;
// The route's own order on a small pool and two answer shares (security
// round A review S7): poppler and the reader, on Unix.
#[cfg(unix)]
mod route_order;
// LibreOffice, pandoc and ffmpeg through the conversion (WP7): stand-ins and
// the real tools where found, on Unix.
#[cfg(unix)]
mod via;
