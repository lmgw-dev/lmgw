//! The formats lmgw's reader executable parses, through the conversion:
//! HTML, EPUB, e-mail with attachments read through the table, legacy
//! `.xls`. Each test runs the real reader as a child.

use std::io::Write as _;

use super::*;

fn named(name: &str, bytes: Vec<u8>) -> Input {
    Input {
        index: 0,
        name: name.into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

#[tokio::test]
async fn a_web_page_is_markdown_and_its_json_has_figures() {
    let none = reader_only().await;
    let page = br#"<!DOCTYPE html><html><head><script>x()</script></head><body>
        <h1>Example page</h1><p>Some <b>bold</b> words.</p>
        <img src="https://images.example/a.png"><ul><li>one</li><li>two</li></ul></body></html>"#;
    let d = convert(
        named("page.bin", page.to_vec()),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.format.as_deref(), Some("html"), "{d:?}");
    assert_eq!(d.status, DocStatus::Converted);
    let md = d.content.unwrap();
    assert!(md.starts_with("# Example page"), "{md}");
    assert!(
        md.contains("<!-- image -->") && !md.contains("images.example"),
        "{md}"
    );
    assert_eq!(d.stats.headings[0], 1);

    let p = plan_of(ConvertOptions {
        output: Output::Json,
        ..Default::default()
    });
    let d = convert(named("page.bin", page.to_vec()), &p, &none)
        .await
        .doc;
    let s = serde_json::to_value(d.structure.unwrap()).unwrap();
    let kinds: Vec<&str> = s[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["heading", "paragraph", "figure", "list_item", "list_item"]
    );

    // A fragment starting with a heading is a page by its name.
    let d = convert(
        named("part.html", b"<h1>Frag</h1><p>x</p>".to_vec()),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.format.as_deref(), Some("html"));
    assert!(d.content.unwrap().starts_with("# Frag"));
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let o = zip::write::SimpleFileOptions::default();
    for (n, c) in entries {
        z.start_file(*n, o).unwrap();
        z.write_all(c).unwrap();
    }
    z.finish().unwrap();
    buf.into_inner()
}

#[tokio::test]
async fn an_epub_reads_its_chapters_in_order() {
    let none = reader_only().await;
    let book = zip(&[
        ("mimetype", b"application/epub+zip"),
        (
            "META-INF/container.xml",
            br#"<container><rootfiles><rootfile full-path="book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        (
            "book.opf",
            br#"<package><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c2"/><itemref idref="c1"/></spine></package>"#,
        ),
        ("c1.xhtml", b"<html><body><h2>Later</h2></body></html>"),
        ("c2.xhtml", b"<html><body><h1>First</h1><p>Opening.</p></body></html>"),
    ]);
    let d = convert(
        named("book.epub", book),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(
        (d.format.as_deref(), d.status),
        (Some("epub"), DocStatus::Converted),
        "{d:?}"
    );
    assert_eq!(
        d.content.as_deref(),
        Some("# First\n\nOpening.\n\n## Later")
    );
}

#[tokio::test]
async fn an_email_shows_its_fields_and_reads_its_attachments_through_the_table() {
    let none = reader_only().await;
    let docx = crate::extract::test_files::docx(&["Attached Word text"]);
    let b64 = |b: &[u8]| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(b)
    };
    let inner =
        "From: c@example.com\r\nTo: d@example.com\r\nSubject: Inner\r\n\r\nForwarded body\r\n";
    let mail = format!(
        "From: Ann <ann@example.com>\r\nTo: bob@example.com\r\nSubject: Example\r\n\
         Date: Tue, 17 Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain\r\n\r\nSee the files.\r\n\
         --b\r\nContent-Type: application/vnd.openxmlformats-officedocument.wordprocessingml.document\r\n\
         Content-Disposition: attachment; filename=\"notes.docx\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n\
         --b\r\nContent-Type: application/octet-stream\r\n\
         Content-Disposition: attachment; filename=\"blob.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n\
         --b\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"fwd.eml\"\r\n\r\n{inner}\r\n\
         --b--\r\n",
        b64(&docx),
        b64(&[0, 159, 146, 150, 0, 1]),
    );
    let d = convert(
        named("mail.eml", mail.into_bytes()),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.format.as_deref(), Some("eml"), "{d:?}");
    // The binary attachment is named, the rest read.
    assert_eq!(d.status, DocStatus::Partial, "{d:?}");
    let md = d.content.unwrap();
    assert!(
        md.starts_with("# Example\n\nFrom: Ann <ann@example.com>"),
        "{md}"
    );
    assert!(md.contains("See the files."), "{md}");
    assert!(
        md.contains("## Attachment: notes.docx") && md.contains("Attached Word text"),
        "{md}"
    );
    assert!(md.contains("Forwarded body"), "{md}");
    let w = d
        .warnings
        .iter()
        .find(|w| w.message.contains("blob.bin"))
        .expect("the unreadable attachment is named");
    assert_eq!(w.code, WarningCode::PartFailed);
    assert_eq!(
        d.parts, 3,
        "the message, the docx and the forwarded message"
    );
}

/// Without the reader these formats are a named `tool_missing`, never read
/// in the gateway.
#[tokio::test]
async fn without_the_reader_its_formats_name_it_as_missing() {
    let none = ToolFacts::all_missing();
    for (name, bytes) in [
        (
            "page.html",
            b"<!DOCTYPE html><html><body><p>x</p></body></html>".to_vec(),
        ),
        ("book.xls", xls()),
        (
            "mail.eml",
            b"From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\n\r\nBody\r\n".to_vec(),
        ),
    ] {
        let d = convert(named(name, bytes), &plan_of(Default::default()), &none)
            .await
            .doc;
        assert_eq!(d.status, DocStatus::Failed, "{name}");
        let e = d.error.unwrap();
        if cfg!(unix) {
            assert_eq!(e.code, ErrorCode::ToolMissing, "{name}: {e:?}");
            assert_eq!(e.tool.as_deref(), Some("reader"), "{name}");
            assert!(
                e.message.contains("lmgw's document reader")
                    && e.message.contains("install lmgw again"),
                "{}",
                e.message
            );
        }
    }
}

/// A BIFF8 workbook of one sheet with three numbers, written here.
fn xls() -> Vec<u8> {
    fn rec(typ: u16, data: &[u8]) -> Vec<u8> {
        let mut v = typ.to_le_bytes().to_vec();
        v.extend_from_slice(&(data.len() as u16).to_le_bytes());
        v.extend_from_slice(data);
        v
    }
    let bof = |dt: u16| {
        let mut d = 0x0600u16.to_le_bytes().to_vec();
        d.extend_from_slice(&dt.to_le_bytes());
        d.extend_from_slice(&[0; 12]);
        rec(0x0809, &d)
    };
    let number = |row: u16, col: u16, v: f64| {
        let mut d = row.to_le_bytes().to_vec();
        d.extend_from_slice(&col.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
        rec(0x0203, &d)
    };
    let name = b"Costs";
    let sheet_rec_len = 4 + 6 + 2 + name.len();
    let globals_len = (4 + 16) + sheet_rec_len + 4;
    let mut bs = (globals_len as u32).to_le_bytes().to_vec();
    bs.extend_from_slice(&[0, 0, name.len() as u8, 0]);
    bs.extend_from_slice(name);
    let mut s = bof(0x0005);
    s.extend(rec(0x0085, &bs));
    s.extend(rec(0x000A, &[]));
    assert_eq!(s.len(), globals_len);
    s.extend(bof(0x0010));
    s.extend(number(0, 0, 4.0));
    s.extend(number(0, 1, 2.5));
    s.extend(number(1, 0, 7.0));
    s.extend(rec(0x000A, &[]));
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    file.create_stream("/Workbook")
        .unwrap()
        .write_all(&s)
        .unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

#[tokio::test]
async fn a_legacy_workbook_reads_like_the_xlsx_reader_writes_one() {
    let none = reader_only().await;
    let d = convert(
        named("book.xls", xls()),
        &plan_of(Default::default()),
        &none,
    )
    .await
    .doc;
    assert_eq!(
        (d.format.as_deref(), d.status),
        (Some("xls"), DocStatus::Converted),
        "{d:?}"
    );
    assert_eq!(
        d.content.as_deref(),
        Some("## Sheet: Costs\n\n| row | A | B |\n| --- | --- | --- |\n| 1 | 4 | 2.5 |\n| 2 | 7 |  |")
    );
    let p = plan_of(ConvertOptions {
        output: Output::Json,
        ..Default::default()
    });
    let d = convert(named("book.xls", xls()), &p, &none).await.doc;
    let s = serde_json::to_value(d.structure.unwrap()).unwrap();
    assert_eq!(
        (s[0]["kind"].as_str(), s[0]["name"].as_str()),
        (Some("sheet"), Some("Costs"))
    );
}
