//! `gguf_files target=aux` says which rows use a file — a projector too: the
//! one an aux row loads through `mmproj_path`, and the one a `--mmproj` or
//! `-mm` still in its args names (decision-models design §7 item i).

use lmgw_core::config::{AuxKind, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAuxModel};
use serde_json::{json, Value};

fn row(model_id: &str, mmproj_path: Option<&str>, args: &[&str]) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: "emb.gguf".into(),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: mmproj_path.map(String::from),
        backend: None,
        threads: None,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

#[tokio::test]
async fn an_aux_projector_is_used_by_the_rows_that_load_it() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    for name in [
        "emb.gguf",
        "mm-field.gguf",
        "mm-flag.gguf",
        "mm-short.gguf",
        "mm-eq.gguf",
        "idle.gguf",
    ] {
        synth::embedding("bert", 1, 512).write_to(&dir.path().join(name));
    }
    for r in [
        row("by-field", Some("mm-field.gguf"), &[]),
        row("by-flag", None, &["--mmproj", "/models/mm-flag.gguf"]),
        row("by-short", None, &["-mm", "mm-short.gguf"]),
        row("by-eq", None, &["--mmproj=/models/mm-eq.gguf"]),
    ] {
        store::insert_aux_model(&state.db, &r).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();

    let files: Value = modelinfo::gguf_files(&state, None, Some("aux"))
        .await
        .unwrap();
    let used = |path: &str| -> Value {
        files["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path} not listed: {files}"))["used_by"]
            .clone()
    };
    assert_eq!(used("mm-field.gguf"), json!(["by-field"]));
    assert_eq!(used("mm-flag.gguf"), json!(["by-flag"]));
    assert_eq!(used("mm-short.gguf"), json!(["by-short"]));
    assert_eq!(used("mm-eq.gguf"), json!(["by-eq"]));
    assert_eq!(
        used("emb.gguf"),
        json!(["by-eq", "by-field", "by-flag", "by-short"])
    );
    assert_eq!(used("idle.gguf"), json!([]));
}
