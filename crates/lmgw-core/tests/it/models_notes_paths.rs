//! `/v1/models` is read by every key holder, so its notes name a file as the
//! container sees it (`/models/...`) and keep the host's directory out of the
//! read error (decision-models design §7 item c).

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::ops;
use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::serve;

#[tokio::test]
async fn an_unreadable_aux_projector_note_carries_no_host_path() {
    let state = AppState::init_for_tests().await.unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    synth::embedding("bert", 1, 512).write_to(&aux_dir.path().join("emb.gguf"));
    // A directory where a projector file should be (a misconfigured mmproj_path).
    std::fs::create_dir_all(aux_dir.path().join("org/repo-GGUF")).unwrap();
    ops::aux_model_set(
        &state,
        serde_json::from_value(json!({"action": "create", "model_id": "vl", "gguf_path":
            "emb.gguf", "kind": "embed", "mmproj_path": "org/repo-GGUF"}))
        .unwrap(),
    )
    .await
    .unwrap();

    let gw = serve(state.clone()).await;
    let list: Value = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "embed/vl")
        .unwrap_or_else(|| panic!("embed/vl not listed: {list}"));
    let notes = entry["notes"].to_string();
    let host = aux_dir.path().display().to_string();
    assert!(!notes.contains(&host), "host path leaked: {notes}");
    assert!(notes.contains("/models/org/repo-GGUF"), "{notes}");
    assert!(notes.contains("not a readable GGUF"), "{notes}");
}

/// A chat row whose configured projector is a directory: the `local_derived`
/// path, whose note names the file relative to the models dir.
#[tokio::test]
async fn an_unreadable_chat_projector_note_carries_no_host_path() {
    let state = AppState::init_for_tests().await.unwrap();
    let chat_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = chat_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    synth::chat("qwen35", 4096).write_to(&chat_dir.path().join("w.gguf"));
    std::fs::create_dir_all(chat_dir.path().join("org/proj-GGUF")).unwrap();
    ops::local_model_set(
        &state,
        ops::patch_from_args(
            json!({"action": "create", "model_id": "vl", "gguf_path": "w.gguf",
                   "mmproj_path": "org/proj-GGUF"})
            .as_object()
            .cloned(),
        )
        .unwrap(),
    )
    .await
    .unwrap();

    let gw = serve(state.clone()).await;
    let list: Value = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "vl")
        .unwrap_or_else(|| panic!("vl not listed: {list}"));
    let notes = entry["notes"].to_string();
    let host = chat_dir.path().display().to_string();
    assert!(!notes.contains(&host), "host path leaked: {notes}");
    assert!(notes.contains("org/proj-GGUF"), "{notes}");
    assert!(notes.contains("not a readable GGUF"), "{notes}");
}

/// An empty or root models dir has nothing to strip: the error stays as it
/// was, not garbled by replacing the empty string.
#[test]
fn an_empty_models_dir_leaves_the_error_alone() {
    use lmgw_core::capabilities::exposed::without_host_path;
    use std::path::Path;
    let err = "/x/a.gguf: not a readable GGUF: nope";
    assert_eq!(without_host_path(err, Path::new("")), err);
    assert_eq!(without_host_path(err, Path::new("/")), err);
    assert_eq!(
        without_host_path("/m/x/a.gguf: not a readable GGUF: nope", Path::new("/m")),
        "not a readable GGUF: nope"
    );
}
