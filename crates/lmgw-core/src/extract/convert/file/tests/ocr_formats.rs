//! The reader's formats beside picture export (WP4 on WP10): an image file
//! as its one picture, what a web page and an EPUB say when they have
//! pictures that are not exported yet, and an e-mail whose attachments have pictures: exported
//! with each attachment, counted against the request's output once, and
//! the attachments past it named and left.

use std::io::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use lmgw_api_types::documents::convert::Pictures;

use super::ocr::{png, scanned, with_ocr, without_osd, PAGE_LINES};
use super::ocr_pictures::{pdf, Sheet};
use super::*;

fn named(name: &str, bytes: Vec<u8>) -> Input {
    Input {
        index: 0,
        name: name.into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

fn plan(pictures: Pictures, output: Output) -> Plan {
    plan_of(ConvertOptions {
        pictures,
        output,
        ..Default::default()
    })
}

fn not_yet(d: &ConvertedDocument) -> Vec<&ConvertWarning> {
    d.warnings
        .iter()
        .filter(|w| w.code == WarningCode::NotYet && w.option.as_deref() == Some("pictures"))
        .collect()
}

/// An image file is read by text recognition; asked for its pictures, it is
/// its one picture as well (WP11 review S7), with no not_yet note; a
/// placeholder or text answer has no picture of it.
#[tokio::test]
async fn an_image_file_asked_for_pictures_is_its_one_picture() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let (w, h, px) = scanned(PAGE_LINES).await;
    let file = png(w, h, px);
    let d = convert(
        input(file.clone()),
        &plan(Pictures::Referenced, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.pictures.len(), 1, "{:?}", d.pictures);
    assert_eq!(d.pictures[0].mime.as_deref(), Some("image/png"));
    assert!(not_yet(&d).is_empty(), "{:?}", d.warnings);
    let md = d.content.unwrap();
    assert!(md.starts_with("![Image](artifacts/image_000000_"), "{md}");
    for p in [
        plan(Pictures::Placeholder, Output::Markdown),
        plan(Pictures::Embedded, Output::Text),
    ] {
        let d = convert(input(file.clone()), &p, &facts).await.doc;
        assert!(not_yet(&d).is_empty(), "{:?}", d.warnings);
        assert!(d.pictures.is_empty(), "{:?}", d.pictures);
        assert!(!d.content.unwrap().contains("<!-- image -->"));
    }
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

fn book(chapter: &[u8]) -> Vec<u8> {
    zip(&[
        ("mimetype", b"application/epub+zip"),
        (
            "META-INF/container.xml",
            br#"<container><rootfiles><rootfile full-path="book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        (
            "book.opf",
            br#"<package><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
        ),
        ("c1.xhtml", chapter),
    ])
}

/// A web page's and an EPUB's pictures are placeholders with a `not_yet`
/// note naming them; a data-URI picture is no exception, and a page
/// without pictures has no note.
#[tokio::test]
async fn web_pages_and_books_with_pictures_say_they_are_not_exported_yet() {
    let none = reader_only().await;
    let page = br#"<!DOCTYPE html><html><body><h1>Example</h1>
        <img src="https://images.example/a.png"><p>Between.</p>
        <img src="data:image/png;base64,iVBORw0KGgo="></body></html>"#;
    for (pictures, output) in [
        (Pictures::Placeholder, Output::Markdown),
        (Pictures::Referenced, Output::Json),
    ] {
        let d = convert(
            named("page.html", page.to_vec()),
            &plan(pictures, output),
            &none,
        )
        .await
        .doc;
        assert_eq!(d.status, DocStatus::Converted, "{d:?}");
        assert!(d.pictures.is_empty());
        let n = not_yet(&d);
        assert_eq!(n.len(), 1, "{:?}", d.warnings);
        assert!(n[0].message.contains("web pages"), "{}", n[0].message);
        if output == Output::Json {
            // Figure blocks without a picture: the list's shape is WP10's.
            let s = serde_json::to_value(d.structure.unwrap()).unwrap();
            let figures: Vec<_> = s[0]["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["kind"] == "figure")
                .collect();
            assert_eq!(figures.len(), 2);
            assert!(figures.iter().all(|f| f.get("picture").is_none()));
        } else {
            let md = d.content.unwrap();
            assert_eq!(md.matches("<!-- image -->").count(), 2, "{md}");
            assert!(!md.contains("images.example") && !md.contains("base64"));
        }
    }
    let plain = convert(
        named(
            "page.html",
            b"<html><body><p>No pictures.</p></body></html>".to_vec(),
        ),
        &plan(Pictures::Referenced, Output::Markdown),
        &none,
    )
    .await
    .doc;
    assert!(not_yet(&plain).is_empty(), "{:?}", plain.warnings);

    let epub = book(b"<html><body><h1>Chapter</h1><img src=\"cover.png\"/></body></html>");
    let d = convert(
        named("book.epub", epub),
        &plan(Pictures::Placeholder, Output::Markdown),
        &none,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let n = not_yet(&d);
    assert_eq!(n.len(), 1, "{:?}", d.warnings);
    assert!(n[0].message.contains("EPUB"), "{}", n[0].message);
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// A message with `attachments` (name, content type, bytes).
fn mail(attachments: &[(&str, &str, Vec<u8>)]) -> Vec<u8> {
    let mut m = String::from(
        "From: ann@example.com\r\nTo: bob@example.com\r\nSubject: Example\r\nMIME-Version: \
         1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: \
         text/plain\r\n\r\nSee the files.\r\n",
    );
    for (name, ty, bytes) in attachments {
        m.push_str(&format!(
            "--b\r\nContent-Type: {ty}\r\nContent-Disposition: attachment; \
             filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            b64(bytes)
        ));
    }
    m.push_str("--b--\r\n");
    m.into_bytes()
}

/// A PDF attachment with a line of text and a picture.
fn pictured_pdf(text: &str) -> Vec<u8> {
    pdf(&[Sheet {
        scan: None,
        text: Some(text),
        figures: vec![[72.0, 200.0, 300.0, 200.0]],
    }])
}

/// The output `d` writes for its file (what the request's budget counts).
fn written(d: &ConvertedDocument) -> u64 {
    let content = d.content.as_ref().map_or(0, String::len);
    let list: usize = d
        .pictures
        .iter()
        .map(|p| p.data_base64.as_ref().map_or(0, String::len))
        .sum();
    (content + list) as u64
}

/// An e-mail's attachments with pictures, `embedded`: each attachment's
/// pictures are exported with it and numbered on in the message; under a
/// budget that holds one attachment and its picture, the second's picture
/// stays a placeholder or the attachment is left, named, and the message
/// stays converted inside the budget.
#[tokio::test]
async fn an_emails_attached_pictures_are_exported_inside_the_request_budget() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let one = mail(&[(
        "one.pdf",
        "application/pdf",
        pictured_pdf("The first report"),
    )]);
    let two = mail(&[
        (
            "one.pdf",
            "application/pdf",
            pictured_pdf("The first report"),
        ),
        (
            "two.pdf",
            "application/pdf",
            pictured_pdf("The second report"),
        ),
    ]);
    let p = plan(Pictures::Embedded, Output::Markdown);
    // Room for everything: both pictures, numbered on.
    let d = convert(named("mail.eml", two.clone()), &p, &facts)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let md = d.content.as_deref().unwrap();
    assert_eq!(
        md.matches("![Image](data:image/png;base64,").count(),
        2,
        "{md}"
    );
    assert!(md.find("first report") < md.find("second report"), "{md}");
    assert_eq!(d.stats.pictures, 2);
    // json numbers them p1 and p2, in the attachments' order.
    let j = convert(
        named("mail.eml", two.clone()),
        &plan(Pictures::Referenced, Output::Json),
        &facts,
    )
    .await
    .doc;
    let ids: Vec<&str> = j.pictures.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["p1", "p2"], "{:?}", j.warnings);
    // What one attachment with its picture takes.
    let big = Arc::new(OutputBudget::request());
    let d1 = super::super::convert(named("mail.eml", one), &p, &facts, &big)
        .await
        .doc;
    assert_eq!(d1.status, DocStatus::Converted, "{d1:?}");
    let one_used = big.used();
    let picture = j.pictures[1].data_base64.as_ref().map_or(0, String::len) as u64;
    assert!(picture > 64 << 10, "{picture}");
    // A budget with room for that, the charges' allowance and a second
    // attachment's text, not for a second picture.
    let limit = one_used + (16 << 10);
    let small = Arc::new(OutputBudget::with_limit(limit));
    let d = super::super::convert(named("mail.eml", two), &p, &facts, &small)
        .await
        .doc;
    assert_ne!(d.status, DocStatus::Failed, "{d:?}");
    assert!(small.used() <= limit && written(&d) <= limit, "{d:?}");
    let md = d.content.as_deref().unwrap();
    assert_eq!(md.matches("![Image](data:").count(), 1, "{md}");
    assert!(md.contains("first report"), "{md}");
    let named_gap = d.warnings.iter().any(|w| {
        (w.code == WarningCode::PicturesPartial && w.message.contains("MAX_UNCOMPRESSED_BYTES"))
            || (w.code == WarningCode::PartFailed && w.message.contains("two.pdf"))
    });
    assert!(named_gap, "{:?}", d.warnings);
}

/// Attachments past the request's output are named and left, and the
/// message keeps what fit; the budget is never passed.
#[tokio::test]
async fn attachments_past_the_budget_are_named_and_left() {
    let none = reader_only().await;
    let text = |c: char| {
        std::iter::repeat_n(c, 1000)
            .collect::<String>()
            .into_bytes()
    };
    let m = mail(&[
        ("a.txt", "text/plain", text('a')),
        ("b.txt", "text/plain", text('b')),
        ("c.txt", "text/plain", text('c')),
    ]);
    let p = plan(Pictures::Embedded, Output::Markdown);
    let limit = 3000;
    let budget = Arc::new(OutputBudget::with_limit(limit));
    let d = super::super::convert(named("mail.eml", m), &p, &none, &budget)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Partial, "{d:?}");
    assert!(budget.used() <= limit);
    let md = d.content.unwrap();
    assert!(md.contains("aaaa") && !md.contains("cccc"), "{md}");
    let w = d
        .warnings
        .iter()
        .find(|w| w.message.contains("used up"))
        .expect("the left attachments are named");
    assert!(w.message.contains("c.txt"), "{}", w.message);
}
