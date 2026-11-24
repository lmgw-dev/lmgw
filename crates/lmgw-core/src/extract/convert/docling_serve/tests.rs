use bytes::Bytes;
use serde_json::json;

use super::*;
use crate::extract::convert::model_help::Reader;
use crate::extract::test_files;

fn input(index: u32, name: &str, bytes: Vec<u8>) -> Input {
    Input {
        index,
        name: name.into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

fn parsed(inputs: Vec<Input>, options: serde_json::Value) -> Parsed {
    Parsed {
        route: "/v1/convert/file",
        inputs,
        options: options.as_object().unwrap().clone(),
        target: Target::Inbody,
        shape_notes: Vec::new(),
        web: Vec::new(),
        decided: Vec::new(),
    }
}

#[tokio::test]
async fn from_formats_skips_the_files_whose_format_is_not_listed() {
    let p = parsed(
        vec![
            input(0, "example-a.txt", b"plain".to_vec()),
            input(1, "example-b.docx", test_files::docx(&["Hello"])),
        ],
        json!({"from_formats": ["docx"]}),
    );
    let Ok(p) = prepare(p, InstallPlatform::Fedora, &["body"], &Reader::none()).await else {
        panic!("refused")
    };
    assert_eq!(p.inputs.len(), 1);
    assert_eq!(p.inputs[0].index, 1);
    assert_eq!(p.skipped[0].index, 0);
    assert_eq!(p.skipped[0].status, DocStatus::Skipped);
    assert_eq!(p.record.files, 2);
}

#[tokio::test]
async fn a_wrong_option_type_is_refused_with_its_record() {
    let p = parsed(
        vec![input(0, "a.txt", b"x".to_vec())],
        json!({"do_ocr": "maybe"}),
    );
    let Err((r, rec)) = prepare(p, InstallPlatform::Fedora, &["body"], &Reader::none()).await
    else {
        panic!("served")
    };
    assert_eq!(r.status, 422);
    assert_eq!(r.detail[0]["loc"], json!(["body", "do_ocr"]));
    assert_eq!(rec.route, "/v1/convert/file");
}

#[tokio::test]
async fn the_notes_header_is_built_from_the_settled_outcomes() {
    let p = parsed(
        vec![input(0, "a.txt", b"x".to_vec())],
        json!({"ocr_engine": "easyocr", "do_ocr": true}),
    );
    let Ok(p) = prepare(p, InstallPlatform::Fedora, &["body"], &Reader::none()).await else {
        panic!("refused")
    };
    assert_eq!(
        p.spec.notes.as_deref(),
        Some("ocr_engine=mapped(easyocr->lmgw-ocr)")
    );
}
