//! WP7's readers beside WP11's picture descriptions on
//! `POST /v1/documents/convert`: an old Word file and an old PowerPoint
//! file reach lmgw's reader through LibreOffice and have their pictures
//! described like a docx's or a pptx's (`describe_min_area` included;
//! skipped by name without LibreOffice or without a reader that confines
//! it); a file read through pandoc and an audio file name why nothing of
//! them is described; an e-mail with an audio and a pictured attachment
//! adds both models' calls up, and keeps one clock: an attachment started
//! after a picture stage took the message's end over ends at its deadline,
//! named. Against `documents_model_help.rs`' mock model and a mock
//! speech-to-text upstream.

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_attach_kinds::{transcriptions, wav};
use crate::common::serve;
use crate::documents_media::{confines, path_with_reader};
use crate::documents_model_help::{convert, settings, Model, ALIAS};
use crate::documents_pictures::{describing, docx, Pic};
use crate::documents_pictures_clock::mail;

const ASR: &str = "my-asr";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A real PNG of `w` × `h` pixels (LibreOffice keeps only pictures it can
/// decode).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A picture that is a fifth of an A4 page.
fn pic(w: u32, h: u32) -> Pic {
    Pic {
        bytes: png(w, h),
        ext: "png",
        cx: 5_000_000,
        cy: 3_000_000,
    }
}

/// A flat OpenDocument text (what LibreOffice makes a `.doc` from): per
/// picture a paragraph before it, the picture (13 × 8 cm, a sixth of an A4
/// page), and its caption, as `documents_pictures::docx` writes them.
fn fodt(pictures: &[Vec<u8>]) -> Vec<u8> {
    let mut body = String::new();
    for (i, p) in pictures.iter().enumerate() {
        let n = i + 1;
        body.push_str(&format!(
            "<text:p>Text before picture {n}.</text:p><text:p><draw:frame \
             text:anchor-type=\"as-char\" svg:width=\"13cm\" svg:height=\"8cm\"><draw:image>\
             <office:binary-data>{}</office:binary-data></draw:image></draw:frame></text:p>\
             <text:p>Figure {n}: Example chart {n}</text:p>",
            b64(p)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document {ODF} office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.text">
<office:body><office:text>{body}</office:text></office:body></office:document>"#
    )
    .into_bytes()
}

const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""#;

/// A mock speech-to-text upstream answering `text` after `delay`.
async fn stt(text: &str, delay: Duration) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "text": text }))
                .set_delay(delay),
        )
        .mount(&mock)
        .await;
    mock
}

/// `state` with the speech-to-text alias [`ASR`] on `stt`, chosen.
async fn hearing(state: &SharedState, stt: &MockServer) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "stt-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::AudioCpp,
            base_url: format!("{}/v1", stt.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: ASR.into(),
            upstream_id: up,
            upstream_model_id: "asr-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
            } })),
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    settings(state, |s| s.documents.asr_alias = ASR.into()).await;
}

/// `source` (named `name`) made into `to` by an unconfined LibreOffice of
/// the test's own, in `dir`.
fn made_by_libreoffice(dir: &Path, name: &str, source: &[u8], to: &str) -> Vec<u8> {
    let src = dir.join(name);
    std::fs::write(&src, source).unwrap();
    let made = std::process::Command::new("soffice")
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            dir.join("maker").display()
        ))
        .args(["--convert-to", to, "--outdir"])
        .arg(dir)
        .arg(&src)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let stem = Path::new(name).file_stem().unwrap().to_str().unwrap();
    std::fs::read(dir.join(format!("{stem}.{to}"))).unwrap()
}

/// A flat OpenDocument presentation of one slide with a title and one
/// picture (what LibreOffice makes a `.ppt` from).
fn fodp(picture: &[u8]) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document {ODF} office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.presentation">
<office:body><office:presentation><draw:page draw:name="p1">
<draw:frame svg:x="2cm" svg:y="1cm" svg:width="20cm" svg:height="2cm"><draw:text-box><text:p>Example slide title</text:p></draw:text-box></draw:frame>
<draw:frame svg:x="4cm" svg:y="4cm" svg:width="16cm" svg:height="10cm"><draw:image><office:binary-data>{}</office:binary-data></draw:image></draw:frame>
</draw:page></office:presentation></office:body></office:document>"#,
        b64(picture)
    )
    .into_bytes()
}

fn request(name: &str, file: &[u8], options: Value) -> Value {
    let mut body = json!({
        "files": [{"name": name, "data_base64": b64(file)}],
        "describe_pictures": true,
    });
    for (k, v) in options.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

/// The descriptions in a file's `pictures` list.
fn descriptions(d: &Value) -> Vec<String> {
    d["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["description"].as_str().map(str::to_string))
        .collect()
}

/// An old Word file and an old PowerPoint file, made here by LibreOffice:
/// lmgw reads each through the docx or pptx LibreOffice makes of it, so
/// their pictures are exported and described as a docx's and a pptx's are,
/// and `describe_min_area` leaves a small share of the page undescribed.
#[tokio::test]
async fn old_word_and_powerpoint_files_have_their_pictures_described() {
    const TEST: &str = "old_word_and_powerpoint_files_have_their_pictures_described";
    if std::process::Command::new("soffice")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("{TEST}: skipped, LibreOffice (soffice) is not on PATH");
        return;
    }
    if !confines() {
        eprintln!("{TEST}: skipped, LibreOffice runs only confined, and nothing confines here");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let doc = made_by_libreoffice(
        t.path(),
        "memo.fodt",
        &fodt(&[png(400, 300), png(300, 200)]),
        "doc",
    );
    let ppt = made_by_libreoffice(t.path(), "deck.fodp", &fodp(&png(400, 300)), "ppt");
    let m = Model::start(Duration::ZERO).await;
    for _ in 0..3 {
        m.will_answer("An example chart.", "stop");
    }
    let state = describing(&m).await;
    state.doc_tools.set_search_path(path_with_reader());
    let gw = serve(state.clone()).await;

    let (status, a) = convert(&gw, &request("example.doc", &doc, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(
        (&d["format"], &d["status"]),
        (&json!("doc"), &json!("converted")),
        "{d}"
    );
    assert_eq!(
        descriptions(d),
        ["An example chart.", "An example chart."],
        "{d}"
    );
    let content = d["content"].as_str().unwrap();
    assert!(
        content.contains("<!-- image -->\n\nFigure 1: Example chart 1\n\nAn example chart."),
        "{content}"
    );
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (&by["alias"], &by["pictures_described"]),
        (&json!(ALIAS), &json!(2)),
        "{d}"
    );
    assert_eq!(d["stats"]["model_calls"], 2, "{d}");
    // The tools: LibreOffice made the docx, lmgw's reader read it.
    let tools: Vec<&str> = a["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert!(tools.contains(&"libreoffice"), "{tools:?}");

    // Markdown: a slide's shapes in the pptx LibreOffice writes are one line
    // apart; in json the picture's marker is a block of its own and becomes
    // a figure block (security round B; a pptx of the same deck reads alike).
    let (status, a) = convert(&gw, &request("example.ppt", &ppt, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(
        (&d["format"], &d["status"]),
        (&json!("ppt"), &json!("converted")),
        "{d}"
    );
    assert_eq!(descriptions(d), ["An example chart."], "{d}");
    let content = d["content"].as_str().unwrap();
    assert!(
        content.contains("<!-- image -->\n\nAn example chart."),
        "{content}"
    );
    assert_eq!(m.bodies().await.len(), 3);

    // A share of the page larger than either picture: exported, no call.
    let (status, a) = convert(
        &gw,
        &request("example.doc", &doc, json!({"describe_min_area": 0.9})),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert!(descriptions(d).is_empty(), "{d}");
    assert_eq!(d["pictures"].as_array().unwrap().len(), 2, "{d}");
    assert_eq!(d["stats"]["pictures_below_describe_min_area"], 2, "{d}");
    assert_eq!(m.bodies().await.len(), 3, "no call below describe_min_area");
}

/// What a request asks of a file with no picture lmgw can describe is
/// answered by name: an audio file has none, and a file read through
/// pandoc keeps its pictures as placeholders (skipped without pandoc).
#[tokio::test]
async fn audio_and_pandoc_files_say_why_nothing_of_them_is_described() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let heard = stt("an example recording", Duration::ZERO).await;
    hearing(&state, &heard).await;
    let gw = serve(state.clone()).await;
    let (status, a) = convert(&gw, &request("example.wav", &wav(), json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(
        (&d["format"], &d["status"]),
        (&json!("audio"), &json!("converted")),
        "{d}"
    );
    let note = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["option"] == "describe_pictures")
        .unwrap_or_else(|| panic!("describe_pictures is answered: {d}"));
    assert_eq!(note["code"], "option_not_applicable", "{note}");
    assert!(
        note["message"].as_str().unwrap().contains("no pictures"),
        "{note}"
    );
    assert_eq!(transcriptions(&heard).await, 1);

    if std::process::Command::new("pandoc")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("audio_and_pandoc_files_say_why_nothing_of_them_is_described: pandoc part skipped, pandoc is not on PATH");
        return;
    }
    state.doc_tools.set_search_path(path_with_reader());
    let tex = b"\\documentclass{article}\\usepackage{graphicx}\\begin{document}\n\
                \\section{Example}\nSome text.\n\n\\includegraphics{example-chart.png}\n\
                \\end{document}\n";
    for output in ["markdown", "text"] {
        let (status, a) =
            convert(&gw, &request("example.tex", tex, json!({"output": output}))).await;
        assert_eq!(status, 200, "{a}");
        let d = &a["documents"][0];
        assert_eq!(d["format"], "latex", "{d}");
        let note = d["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["option"] == "describe_pictures")
            .unwrap_or_else(|| panic!("describe_pictures is answered in {output}: {d}"));
        assert_eq!(note["code"], "not_yet", "{note}");
        assert!(
            note["message"]
                .as_str()
                .unwrap()
                .contains("read through pandoc"),
            "{note}"
        );
    }
    assert!(
        m.bodies().await.is_empty(),
        "nothing was sent to the describer"
    );
}

/// The `documents-asr` rows, once there are `at_least`.
async fn asr_rows(state: &SharedState, at_least: usize) -> Vec<store::RequestLogRow> {
    let mut rows = Vec::new();
    for _ in 0..300 {
        rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some(ASR.into()),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        rows.retain(|r| r.ingress_proto == "documents-asr");
        if rows.len() >= at_least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rows
}

/// A state that describes with `m`, hears with `heard` and reads e-mail
/// with lmgw's reader beside the tests.
async fn mail_state(m: &Model, heard: &MockServer) -> SharedState {
    let state = describing(m).await;
    hearing(&state, heard).await;
    state.doc_tools.set_search_path(path_with_reader());
    state
}

/// An e-mail with a pictured Word file and a recording: one entry per
/// answering model in the message's stats, each with its own count, the
/// calls summed, the transcript counted by its source.
#[tokio::test]
async fn an_email_s_pictures_and_recording_add_up_in_its_stats() {
    let m = Model::start(Duration::ZERO).await;
    let heard = stt("an example recording", Duration::ZERO).await;
    let state = mail_state(&m, &heard).await;
    let gw = serve(state.clone()).await;
    let message = mail(&[
        (
            "pictures.docx",
            "application/octet-stream",
            docx(&[pic(400, 300), pic(300, 200)]),
        ),
        ("recording.wav", "audio/wav", wav()),
    ]);
    let (status, a) = convert(&gw, &request("example.eml", &message, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert!(
        d["content"]
            .as_str()
            .unwrap()
            .contains("an example recording"),
        "{d}"
    );
    let stats = &d["stats"];
    assert_eq!(stats["model_calls"], 3, "{stats}");
    assert_eq!(stats["parts_by_source"]["asr"], 1, "{stats}");
    let of = |alias: &str| {
        stats["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["alias"] == alias)
            .cloned()
            .unwrap_or_else(|| panic!("{alias} in {stats}"))
    };
    let (seen, heard_by) = (of(ALIAS), of(ASR));
    assert_eq!(
        (
            &seen["calls"],
            &seen["pictures_described"],
            &seen["files_transcribed"]
        ),
        (&json!(2), &json!(2), &json!(0)),
        "{seen}"
    );
    assert_eq!(
        (
            &heard_by["calls"],
            &heard_by["parts_read"],
            &heard_by["files_transcribed"]
        ),
        (&json!(1), &json!(0), &json!(1)),
        "{heard_by}"
    );
    assert_eq!(heard_by["bound_from"], "one_call", "{heard_by}");
    assert_eq!(transcriptions(&heard).await, 1);
}

/// One clock for the message: the pictured attachment's stage takes its
/// end over, so the recording after it, still being heard at the
/// deadline, ends there; it is named, its call leaves a `499` row, and
/// the attachment after it is not started.
#[tokio::test]
async fn an_email_s_recording_after_a_picture_stage_ends_at_the_deadline() {
    let m = Model::start(Duration::ZERO).await;
    let heard = stt("never heard", Duration::from_millis(4000)).await;
    let state = mail_state(&m, &heard).await;
    let gw = serve(state.clone()).await;
    let message = mail(&[
        (
            "pictures.docx",
            "application/octet-stream",
            docx(&[pic(400, 300)]),
        ),
        ("recording.wav", "audio/wav", wav()),
        (
            "later.txt",
            "text/plain",
            b"Example later attachment.".to_vec(),
        ),
    ]);
    let started = std::time::Instant::now();
    let (status, a) = convert(
        &gw,
        &request("example.eml", &message, json!({"document_timeout": 1.5})),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    assert!(
        started.elapsed() < Duration::from_millis(3500),
        "the message ended at its deadline: {:?}",
        started.elapsed()
    );
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert_eq!(descriptions(d).len(), 1, "{d}");
    let content = d["content"].as_str().unwrap();
    assert!(!content.contains("Example later attachment"), "{content}");
    let late = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["message"].as_str())
        .find(|w| w.contains("document_timeout of 1.5 s"))
        .unwrap_or_else(|| panic!("the attachments left are named: {d}"));
    assert!(
        late.contains("recording.wav") && late.contains("later.txt"),
        "{late}"
    );
    let rows = asr_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, 499, "{rows:?}");
    assert_eq!(transcriptions(&heard).await, 1);
}
