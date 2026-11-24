//! The docling-serve-compatible convert routes over HTTP (document
//! conversion design §7.4, WP5), driven with requests as docling-serve's
//! clients send them: multipart `files` with options as form fields, JSON
//! `sources` with `kind: file` base64, the older `file_sources` and
//! `http_sources`. The answers are checked against docling's shapes.

use base64::Engine as _;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::docx;
use crate::common::{serve, Gw};

pub(crate) async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn part(name: &str, bytes: Vec<u8>) -> reqwest::multipart::Part {
    reqwest::multipart::Part::bytes(bytes).file_name(name.to_string())
}

fn zip_names(bytes: &[u8]) -> Vec<String> {
    let z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    z.file_names().map(String::from).collect::<Vec<_>>()
}

fn zip_text(bytes: &[u8], name: &str) -> String {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut z.by_name(name).unwrap(), &mut s).unwrap();
    s
}

#[tokio::test]
async fn one_file_as_a_docling_client_sends_it_answers_convert_document_response() {
    let (_state, gw) = gateway().await;
    let form = reqwest::multipart::Form::new()
        .part(
            "files",
            part("example-report.txt", b"# Report\n\nSome text.".to_vec()),
        )
        .text("to_formats", "md")
        .text("to_formats", "text")
        .text("do_ocr", "true")
        .text("ocr_engine", "easyocr")
        .text("ocr_lang", "en")
        .text("table_mode", "fast")
        .text("abort_on_error", "false")
        .text("do_picture_classification", "true");
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        notes.contains("ocr_engine=mapped(easyocr->lmgw-ocr)"),
        "{notes}"
    );
    assert!(
        notes.contains("do_picture_classification=not_yet"),
        "{notes}"
    );
    assert!(
        !notes.contains("do_ocr"),
        "honoured options are not listed: {notes}"
    );
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=1, partial=0, failed=0, skipped=0"
    );
    let v: Value = r.json().await.unwrap();
    // docling's ConvertDocumentResponse, every field present.
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "confidence",
            "document",
            "errors",
            "processing_time",
            "status",
            "timings"
        ]
    );
    assert_eq!(v["status"], "success");
    assert_eq!(v["document"]["filename"], "example-report.txt");
    assert_eq!(v["document"]["md_content"], "# Report\n\nSome text.");
    // A text file's text form is the file itself, as native `output: text`.
    assert_eq!(v["document"]["text_content"], "# Report\n\nSome text.");
    for k in [
        "json_content",
        "html_content",
        "doctags_content",
        "doclang_content",
    ] {
        assert!(v["document"][k].is_null(), "{k}");
    }
    assert!(v["processing_time"].is_number());
}

#[tokio::test]
async fn several_files_answer_converted_docs_zip_flat_as_docling_main_writes_it() {
    let (_state, gw) = gateway().await;
    let form = reqwest::multipart::Form::new()
        .part("files", part("example-a.txt", b"alpha".to_vec()))
        .part("files", part("example-b.docx", docx("Hello from Word")))
        .part(
            "files",
            part("example-c.png", b"\x89PNG\r\n\x1a\nrest".to_vec()),
        )
        .part("files", part("example-a.txt", b"again".to_vec()));
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "application/zip");
    assert_eq!(
        r.headers()["content-disposition"],
        "attachment; filename=\"converted_docs.zip\""
    );
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=3, partial=0, failed=1, skipped=0, entry_names_renamed=1"
    );
    let bytes = r.bytes().await.unwrap();
    // The picture failed (its bytes are no picture) and is absent; the repeated stem
    // keeps both files.
    assert_eq!(
        zip_names(&bytes),
        ["example-a.md", "example-b.md", "example-a_3.md"]
    );
    assert_eq!(zip_text(&bytes, "example-b.md"), "Hello from Word");
    assert_eq!(zip_text(&bytes, "example-a_3.md"), "again");
}

#[tokio::test]
async fn json_sources_in_both_spellings_and_the_zip_target() {
    let (_state, gw) = gateway().await;
    let body = json!({
        "options": {"to_formats": ["md", "json"], "from_formats": ["docx"]},
        "sources": [{"kind": "file", "base64_string": b64(&docx("Hello")), "filename": "example.docx"}],
        "file_sources": [{"base64_string": b64(b"skipped text"), "filename": "example.txt"}],
    });
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=1, partial=0, failed=0, skipped=1"
    );
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(notes.contains("to_formats=not_yet(json)"), "{notes}");
    let names = zip_names(&r.bytes().await.unwrap());
    assert_eq!(names, ["example.md"], "the skipped file is absent");

    // One file, inbody: an output lmgw does not write is partial_success.
    let body = json!({
        "options": {"to_formats": ["md", "json"]},
        "sources": [{"kind": "file", "base64_string": b64(b"x"), "filename": "a.txt"}],
    });
    let v: Value = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "partial_success");
    assert!(v["errors"][0]["error_message"]
        .as_str()
        .unwrap()
        .contains("to_formats json"));
    assert_eq!(v["errors"][0]["component_type"], "user_input");
}

#[tokio::test]
async fn http_sources_are_a_named_400_and_their_host_is_recorded_without_the_address() {
    let (state, gw) = gateway().await;
    for body in [
        json!({"sources": [{"kind": "http", "url": "https://docs.example/a.pdf?token=secret", "headers": {"Authorization": "secret"}}]}),
        json!({"http_sources": [{"url": "https://docs.example/b.pdf"}]}),
    ] {
        let r = gw
            .client()
            .post(format!("{gw}/v1/convert/source"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["code"], "http_source_refused");
        let d = v["detail"].as_str().unwrap();
        assert!(
            d.contains("base64_string") && d.contains("Let callers send web addresses"),
            "{d}"
        );
    }
    let rows = store::convert_calls(&state.db, "/v1/convert/")
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let all = format!("{rows:?}");
    assert!(
        all.contains("docs.example") && all.contains("refused"),
        "{all}"
    );
    assert!(!all.contains("secret") && !all.contains("a.pdf"), "{all}");
    // And the summary has the host under the caller.
    let u: Value = gw
        .client()
        .post(format!("{gw}/api/op/doc_adapter_usage"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(u["keys"][0]["requests"], 2, "{u}");
    assert_eq!(u["keys"][0]["hosts"][0]["host"], "docs.example");
    assert_eq!(u["keys"][0]["hosts"][0]["count"], 2);
}

#[tokio::test]
async fn storage_targets_and_sources_are_refused_and_wrong_types_are_doclings_422() {
    let (_state, gw) = gateway().await;
    let file = json!([{"kind": "file", "base64_string": b64(b"x"), "filename": "a.txt"}]);
    for (body, status, code) in [
        (
            json!({"sources": file, "target": {"kind": "s3", "bucket": "b"}}),
            400,
            "target_refused",
        ),
        (
            json!({"sources": [{"kind": "s3", "bucket": "b"}]}),
            400,
            "source_kind_refused",
        ),
        (
            json!({"sources": file, "options": {"do_ocr": "maybe"}}),
            422,
            "invalid_option",
        ),
        (json!({"sources": []}), 422, "no_files"),
    ] {
        let r = gw
            .client()
            .post(format!("{gw}/v1/convert/source"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), status, "{body}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["code"], code, "{v}");
    }
    let v: Value = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&json!({"sources": file, "options": {"do_ocr": "maybe"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["detail"][0]["loc"], json!(["body", "options", "do_ocr"]));
    // Unknown options are a note, never a 422.
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&json!({"sources": file, "options": {"brand_new_option": 1}, "callbacks": [{"url": "https://hook.example/done"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"].to_str().unwrap();
    assert!(
        notes.contains("brand_new_option=not_yet(unknown option)"),
        "{notes}"
    );
    assert!(notes.contains("callbacks=mapped(not called"), "{notes}");
}

#[tokio::test]
async fn the_health_version_and_not_yet_routes() {
    let (_state, gw) = gateway().await;
    for path in ["/health", "/ready"] {
        let r = gw.anon().get(format!("{gw}{path}")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200, "{path}");
        assert_eq!(r.json::<Value>().await.unwrap(), json!({"status": "ok"}));
    }
    let v: Value = gw
        .anon()
        .get(format!("{gw}/version"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["docling-serve-compat"], "1.38");
    // lmgw's version to anyone, as `GET /api/version` (public) names it too
    // (security round B review: hardening 1 reviewed, not changed).
    assert_eq!(v["lmgw"], env!("CARGO_PKG_VERSION"), "{v}");
    let r = gw
        .client()
        .post(format!("{gw}/v1/chunk/hybrid/source"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 501);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "not_yet");
    assert!(v["detail"]
        .as_str()
        .unwrap()
        .contains("/v1/chunk/hybrid/source"));
}
