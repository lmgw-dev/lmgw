//! The page-reading model after text recognition (document-conversion
//! design §3, §7.1; WP6 on WP4, WP8 and WP10), against
//! `documents_model_help.rs`' mock model: the order of readers for a page
//! (text layer, then tesseract, then the model) and which pages reach the
//! model under each `model_help`, and the file's clock, which counts a call
//! in flight but not the wait for a model's turn.

use std::io::Write as _;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::documents_model_help::{convert, gateway, has_poppler, sent, settings, Model};

/// Whether tesseract with its English pack is installed (the order tests
/// need it; without it they skip, as the WP4 tests do).
fn has_tesseract() -> bool {
    std::process::Command::new("tesseract")
        .arg("--list-langs")
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l == "eng")
        })
}

/// A one-page PDF with `lines` of large text, rendered by `pdftoppm` as an
/// A4 page of grey pixels at 150 dpi: (width, height, pixels).
fn rendered(lines: &[&str]) -> (u32, u32, Vec<u8>) {
    let mut content = String::from("BT /F1 22 Tf 60 760 Td 30 TL");
    for l in lines {
        content.push_str(&format!(" ({l}) Tj T*"));
    }
    content.push_str(" ET");
    let pdf = pdf_of(&[Page::Text(content)]);
    let out = std::process::Command::new("pdftoppm")
        .args(["-gray", "-r", "150", "-singlefile", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().unwrap().write_all(&pdf)?;
            c.wait_with_output()
        })
        .unwrap();
    pgm(&out.stdout)
}

/// A binary PGM's width, height and pixels.
fn pgm(data: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while data[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while !data[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..at]).to_string());
    }
    let (w, h): (u32, u32) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    (w, h, data[at + 1..].to_vec())
}

enum Page {
    /// A page with a text layer: its content stream.
    Text(String),
    /// A scanned page: grey pixels drawn over the whole A4 page.
    Scan(u32, u32, Vec<u8>),
}

/// An A4 PDF of `pages`.
fn pdf_of(pages: &[Page]) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = Vec::new();
    for p in pages {
        let id = objs.len() + 1;
        kids.push(format!("{id} 0 R"));
        let (resources, content, image) = match p {
            Page::Text(c) => ("/Font << /F1 3 0 R >>".to_string(), c.clone(), None),
            Page::Scan(w, h, px) => (
                format!("/XObject << /Im0 {} 0 R >>", id + 2),
                "q 595 0 0 842 0 0 cm /Im0 Do Q".to_string(),
                Some((w, h, px)),
            ),
        };
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << {resources} >> /Contents {} 0 R >>",
                id + 1
            )
            .into_bytes(),
        );
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        objs.push(stream);
        if let Some((w, h, px)) = image {
            let mut img = format!(
                "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceGray \
                 /BitsPerComponent 8 /Length {} >>\nstream\n",
                px.len()
            )
            .into_bytes();
            img.extend_from_slice(px);
            img.extend_from_slice(b"\nendstream");
            objs.push(img);
        }
    }
    objs[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        kids.len()
    )
    .into_bytes();
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", i + 1).unwrap();
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).unwrap();
    for off in offsets {
        writeln!(out, "{off:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    )
    .unwrap();
    out
}

fn body(pdf: &[u8], options: Value) -> Value {
    let mut b = json!({"files": [{
        "name": "example-scan.pdf",
        "data_base64": base64::engine::general_purpose::STANDARD.encode(pdf),
    }]});
    for (k, v) in options.as_object().unwrap() {
        b[k] = v.clone();
    }
    b
}

/// The layer page's own words, which a prompt for it carries.
const LAYER: &str = "Example layer page with its own text";

/// The order of readers: a page with a text layer, a scan tesseract reads
/// well and a blank scan (tesseract finds no word on it). The model gets
/// what the decision table (`model_help::select`) says, and never the
/// recognised text to copy from.
#[tokio::test]
async fn the_model_reads_what_the_text_layer_and_recognition_left() {
    if !has_poppler() || !has_tesseract() {
        eprintln!("skipped: poppler-utils or tesseract (eng) is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    let scan = rendered(&["Example invoice for the year", "Paper and ink were bought"]);
    let blank = (scan.0, scan.1, vec![255u8; scan.2.len()]);
    let pdf = pdf_of(&[
        Page::Text(format!("BT /F1 16 Tf 60 760 Td ({LAYER}) Tj ET")),
        Page::Scan(scan.0, scan.1, scan.2.clone()),
        Page::Scan(blank.0, blank.1, blank.2.clone()),
    ]);
    let calls = |a: &Value| a["documents"][0]["stats"]["model_calls"].as_u64().unwrap();
    let by = |a: &Value, s: &str| a["documents"][0]["stats"]["parts_by_source"][s].clone();

    for help in ["missing", "missing_or_tables"] {
        let before = m.bodies().await.len();
        let (status, a) = convert(&gw, &body(&pdf, json!({"model_help": help}))).await;
        assert_eq!(status, 200, "{a}");
        // Only the blank page: nothing read it.
        assert_eq!(calls(&a), 1, "{help}: {a}");
        assert_eq!(m.bodies().await.len() - before, 1, "{help}");
        assert_eq!(
            (by(&a, "text_layer"), by(&a, "ocr"), by(&a, "vision")),
            (json!(1), json!(1), json!(1)),
            "{help}: {a}"
        );
        let content = a["documents"][0]["content"].as_str().unwrap();
        assert!(content.contains(LAYER), "{content}");
        assert!(
            content.contains("Example invoice"),
            "tesseract's page: {content}"
        );
        assert!(content.contains("Read by the model"), "{content}");
    }

    let before = m.bodies().await.len();
    let (status, a) = convert(&gw, &body(&pdf, json!({"model_help": "every_page"}))).await;
    assert_eq!(status, 200, "{a}");
    assert_eq!(calls(&a), 3, "{a}");
    assert_eq!(by(&a, "vision"), json!(3));
    assert_eq!(by(&a, "ocr"), json!(0), "the model's pages are its own");
    let bodies = m.bodies().await;
    let prompts: Vec<String> = bodies[before..].iter().map(|b| sent(b).0).collect();
    // The layer page is asked with its text, the recognised one without
    // the words tesseract read.
    assert_eq!(prompts.iter().filter(|p| p.contains(LAYER)).count(), 1);
    assert!(
        prompts.iter().all(|p| !p.contains("Example invoice")),
        "{prompts:?}"
    );
    // No warning is left about what tesseract made of pages the model read.
    let warnings = a["documents"][0]["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .all(|w| !w["code"].as_str().unwrap().starts_with("ocr_")),
        "{warnings:?}"
    );

    // Recognition off: both scans are pages nothing read.
    let before = m.bodies().await.len();
    let (_, a) = convert(
        &gw,
        &body(&pdf, json!({"model_help": "missing", "ocr": "off"})),
    )
    .await;
    assert_eq!(calls(&a), 2, "{a}");
    assert_eq!(m.bodies().await.len() - before, 2);
    assert_eq!(by(&a, "ocr"), json!(0));
}

/// The file's clock counts a call in flight, not the wait for a turn: two
/// files' scans behind one call at a time, each call a second long, a
/// `document_timeout` of 1.6 s. The second file waits a second for its
/// turn and still has its time; by the wall clock it would be late.
#[tokio::test]
async fn waiting_for_a_model_turn_is_not_the_file_s_time() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::from_millis(1000)).await;
    let (state, gw) = gateway(&m).await;
    // One call in flight: a cloud route's bound is the tool pool's size.
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let scan = crate::chat_attach_kinds::pdf_bytes(&[None]);
    let file = |name: &str| json!({"name": name, "data_base64": base64::engine::general_purpose::STANDARD.encode(&scan)});
    let req = json!({
        "files": [file("example-a.pdf"), file("example-b.pdf")],
        "ocr": "off",
        "document_timeout": 1.6,
    });
    let started = std::time::Instant::now();
    let (status, a) = convert(&gw, &req).await;
    assert_eq!(status, 200, "{a}");
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "the calls took turns"
    );
    for d in a["documents"].as_array().unwrap() {
        assert_eq!(d["status"], "converted", "{d}");
        assert_eq!(d["stats"]["parts_by_source"]["vision"], 1, "{d}");
    }
    assert_eq!(m.bodies().await.len(), 2);
}
