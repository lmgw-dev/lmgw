//! The layout model without its file (document-conversion design §14,
//! WP8): what Settings › Documents reads about it, that it cannot be
//! switched on while the file is not there, and that a conversion with it
//! switched on over a file that is not the model converts as without it
//! and says so. The model itself runs in `layout_model`'s ignored test.

use lmgw_core::extract::convert::layout_model::files::HERON;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::pdf_bytes;
use crate::common::{serve, Gw};

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let gw = serve(state.clone()).await;
    (state, gw)
}

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// The aux models directory set to `dir`.
async fn models_dir(state: &SharedState, dir: &std::path::Path) {
    let mut settings = state.snapshot().settings.clone();
    settings.aux_router.models_dir = dir.display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

fn layout_row(caps: &Value) -> &Value {
    caps["models"]
        .as_array()
        .and_then(|m| m.iter().find(|r| r["id"] == "layout"))
        .unwrap_or_else(|| panic!("no layout row: {caps}"))
}

#[tokio::test]
async fn the_page_lists_the_model_and_it_stays_off_until_its_file_is_here() {
    let (state, gw) = gateway().await;
    let dir = tempfile::tempdir().unwrap();
    models_dir(&state, dir.path()).await;
    let (status, caps) = op(&gw, "doc_tools", json!({})).await;
    assert_eq!(status, 200, "{caps}");
    let row = layout_row(&caps);
    assert_eq!(row["state"], "missing", "{row}");
    assert_eq!(
        (&row["chosen"], &row["in_use"]),
        (&json!(false), &json!(false))
    );
    assert_eq!(row["size_bytes"], 171_220_471u64);
    assert_eq!(row["licence"], "Apache-2.0");
    assert_eq!(row["repo"], "docling-project/docling-layout-heron-onnx");
    assert_eq!(row["revision"].as_str().map(str::len), Some(40));
    assert!(row["path"]
        .as_str()
        .unwrap()
        .ends_with("docling-project/docling-layout-heron-onnx/model.onnx"));
    assert_eq!(row["loaded"], Value::Null);
    assert!(!state.snapshot().settings.documents.onnx_layout);
    // Switching it on is refused, by what to do first; nothing is stored.
    let (status, e) = op(&gw, "settings_set", json!({"documents_onnx_layout": true})).await;
    assert_ne!(status, 200, "{e}");
    assert!(e.to_string().contains("doc_model_add id=layout"), "{e}");
    assert!(!state.snapshot().settings.documents.onnx_layout);
    // Switching it off is always possible.
    let (status, e) = op(&gw, "settings_set", json!({"documents_onnx_layout": false})).await;
    assert_eq!(status, 200, "{e}");
    // A model lmgw does not know is named as such.
    let (status, e) = op(&gw, "doc_model_add", json!({"id": "example"})).await;
    assert_ne!(status, 200, "{e}");
    assert!(e.to_string().contains("unknown model 'example'"), "{e}");
}

#[tokio::test]
async fn without_a_folder_for_models_the_row_says_where_to_set_one() {
    let (state, gw) = gateway().await;
    let mut settings = state.snapshot().settings.clone();
    settings.aux_router.models_dir = String::new();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let (_, caps) = op(&gw, "doc_tools", json!({})).await;
    let row = layout_row(&caps);
    assert_eq!(row["state"], "no_folder", "{row}");
    assert_eq!(row["path"], Value::Null);
    assert!(row["message"].as_str().is_some_and(|m| !m.is_empty()));
    // Nothing is asked of the hub when there is nowhere to download into.
    let (status, e) = op(&gw, "doc_model_add", json!({"id": "layout"})).await;
    assert_ne!(status, 200, "{e}");
    assert!(e.to_string().contains("models directory is not set"), "{e}");
}

/// A file of the model's size that is not the model: found by its size,
/// refused by its hash before anything is loaded.
#[tokio::test]
async fn a_file_that_is_not_the_model_is_never_loaded_and_the_pdf_converts_as_before() {
    if std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!("skipped: poppler is not on PATH");
        return;
    }
    let (state, gw) = gateway().await;
    let pdf = pdf_bytes(&[Some("An example page for the layout model")]);
    let convert = || async {
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(pdf.clone()).file_name("example.pdf"),
        );
        let r = gw
            .client()
            .post(format!("{gw}/v1/documents/convert"))
            .multipart(form)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap();
        assert_eq!(status, 200, "{v}");
        v["documents"][0].clone()
    };
    let before = convert().await;
    assert_eq!(before["warnings"], json!([]), "{before}");

    let dir = tempfile::tempdir().unwrap();
    let place = dir.path().join(HERON.dest_rel_path());
    std::fs::create_dir_all(place.parent().unwrap()).unwrap();
    // As large as the model, all zeros (sparse on disk).
    std::fs::File::create(&place)
        .unwrap()
        .set_len(HERON.size_bytes)
        .unwrap();
    models_dir(&state, dir.path()).await;
    let (_, caps) = op(&gw, "doc_tools", json!({})).await;
    assert_eq!(layout_row(&caps)["state"], "found");
    let (status, e) = op(&gw, "settings_set", json!({"documents_onnx_layout": true})).await;
    assert_eq!(status, 200, "{e}");
    assert!(state.snapshot().settings.documents.onnx_layout);

    let after = convert().await;
    assert_eq!(after["status"], "converted", "{after}");
    assert_eq!(after["content"], before["content"]);
    let warnings = after["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{after}");
    assert_eq!(warnings[0]["code"], "tool_missing");
    assert_eq!(warnings[0]["tool"], "layout_model");
    let said = warnings[0]["message"].as_str().unwrap();
    assert!(
        said.contains("is not the model") && said.contains("SHA-256"),
        "{said}"
    );
    // The page now says what is wrong with the file, and that it is not used.
    let (_, caps) = op(&gw, "doc_tools", json!({})).await;
    let row = layout_row(&caps);
    assert_eq!(row["state"], "wrong_file", "{row}");
    assert_eq!(
        (&row["chosen"], &row["in_use"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(row["loaded"], Value::Null);
}
