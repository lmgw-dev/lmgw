//! A field named in `clear` is unset on **create** too, whatever value came
//! with it — for every class with per-model container overrides.
//!
//! The bug this pins (found on a real box, 2026-10-01): the dashboard's row
//! editors send a blank "extra run args" field as its empty value *and* its
//! name in `clear` ("inherit the class"). The update paths honoured the
//! clear; the create paths stored the empty list, which means "run with no
//! extra args" — so every audio row created from the dashboard dropped the
//! class's `--device nvidia.com/gpu=all --security-opt label=disable`, and
//! its container failed to read its own `/config/server.json` under
//! SELinux.

use std::sync::Arc;

use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::{ops, store};
use serde_json::{json, Map, Value};

use crate::common::serve;

const LLAMA_HELP: &str = include_str!("../fixtures/llama/llama-server-help-4b1a27fa0.txt");
const SD_HELP: &str = include_str!("../fixtures/sdcpp/sd-server-help-c678dfe.txt");

/// Answers the `--help` runs a save validates flags against; nothing starts.
struct HelpOnly;

#[async_trait::async_trait]
impl CommandRunner for HelpOnly {
    async fn run(&self, _program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let help = args.iter().any(|a| a == "--help");
        let sd = args.iter().any(|a| a == "/sd-server");
        Ok(CmdOutput {
            status: 0,
            stdout: match (help, sd) {
                (true, true) => SD_HELP.into(),
                (true, false) => LLAMA_HELP.into(),
                _ => String::new(),
            },
            stderr: String::new(),
        })
    }
}

async fn state_with_dirs() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(HelpOnly),
        reqwest::Client::new(),
    )));
    let dir = tempfile::tempdir().unwrap();
    for f in ["chat.gguf", "embed.gguf", "diffusion.gguf"] {
        std::fs::write(dir.path().join(f), b"x").unwrap();
    }
    std::fs::create_dir_all(dir.path().join("tts")).unwrap();
    let mut s = state.snapshot().settings.clone();
    let d = dir.path().display().to_string();
    s.router.models_dir = d.clone();
    s.aux_router.models_dir = d.clone();
    s.image.models_dir = d.clone();
    s.audio.models_dir = d;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

fn args(v: Value) -> Option<Map<String, Value>> {
    v.as_object().cloned()
}

/// Chat, aux and image: the ops verbs the dashboard and the tool plane share.
#[tokio::test]
async fn a_cleared_extra_run_args_inherits_on_create_for_chat_aux_and_image() {
    let (state, _dir) = state_with_dirs().await;

    ops::local_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "chat-cleared", "gguf_path": "chat.gguf",
            "extra_run_args": "--cpus 2", "clear": "extra_run_args",
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    ops::local_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "chat-kept", "gguf_path": "chat.gguf",
            "extra_run_args": "--cpus 2",
        })))
        .unwrap(),
    )
    .await
    .unwrap();

    // The aux editor's exact shape: an empty array *and* the name.
    ops::aux_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "embed-cleared", "gguf_path": "embed.gguf",
            "kind": "embed", "extra_run_args": [], "clear": "extra_run_args",
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    ops::aux_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "embed-kept", "gguf_path": "embed.gguf",
            "kind": "embed", "extra_run_args": ["--cpus", "2"],
        })))
        .unwrap(),
    )
    .await
    .unwrap();

    // The image editor's: an empty text *and* the name.
    for (id, era, clear) in [
        ("img-cleared", "", "extra_run_args"),
        ("img-kept", "--cpus 2", ""),
    ] {
        ops::image_model_set(
            &state,
            ops::patch_from_args(args(json!({
                "action": "create", "model_id": id,
                "files": {"diffusion_model": "diffusion.gguf"},
                "extra_run_args": era, "clear": clear,
            })))
            .unwrap(),
        )
        .await
        .unwrap();
    }

    let snap = state.snapshot();
    let chat = |id: &str| {
        snap.local_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .extra_run_args
            .clone()
    };
    let aux = |id: &str| {
        snap.aux_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .extra_run_args
            .clone()
    };
    let img = |id: &str| {
        snap.image_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .extra_run_args
            .clone()
    };
    let two = Some(vec!["--cpus".to_string(), "2".to_string()]);
    assert_eq!(chat("chat-cleared"), None, "the class's args, inherited");
    assert_eq!(chat("chat-kept"), two);
    assert_eq!(aux("embed-cleared"), None);
    assert_eq!(aux("embed-kept"), two);
    assert_eq!(img("img-cleared"), None);
    assert_eq!(img("img-kept"), two);
}

/// Audio, through the dashboard's op exactly as its editor posts it — and
/// `lazy` and `busy_timeout_ms`, the other two fields that editor clears by
/// name, the same way.
#[tokio::test]
async fn the_audio_editor_s_blank_fields_inherit_on_create() {
    let (state, _dir) = state_with_dirs().await;
    let base = serve(state.clone()).await;
    let post = |body: Value| {
        let base = base.clone();
        async move {
            let resp = base
                .client()
                .post(format!("{base}/api/op/audio_model_set"))
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let v: Value = resp.json().await.unwrap();
            assert_eq!(status, 200, "{v}");
        }
    };
    post(json!({
        "action": "create", "model_id": "tts-cleared", "family": "pocket_tts",
        "path": "tts", "task": "tts", "extra_run_args": [], "lazy": true,
        "busy_timeout_ms": 5, "clear": "extra_run_args lazy busy_timeout_ms",
    }))
    .await;
    post(json!({
        "action": "create", "model_id": "tts-kept", "family": "pocket_tts",
        "path": "tts", "task": "tts", "extra_run_args": ["--cpus", "2"],
        "lazy": false, "busy_timeout_ms": 5,
    }))
    .await;

    let snap = state.snapshot();
    let row = |id: &str| {
        snap.audio_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .clone()
    };
    let cleared = row("tts-cleared");
    assert_eq!(cleared.extra_run_args, None, "the class's args, inherited");
    assert_eq!(cleared.lazy, None);
    assert_eq!(cleared.busy_timeout_ms, None);
    let kept = row("tts-kept");
    assert_eq!(
        kept.extra_run_args,
        Some(vec!["--cpus".to_string(), "2".to_string()])
    );
    assert_eq!(kept.lazy, Some(false));
    assert_eq!(kept.busy_timeout_ms, Some(5));
}

/// An override that holds no args is never stored, with or without a
/// `clear` beside it, on create and on update, for every class: an empty
/// list is the class's run args (ASR eval 2, 2026-10-02 — twelve audio rows
/// held `[]` and their containers had neither the GPU nor `label=disable`).
/// Blank text is a flat tool's "not supplied", and leaves an override as it
/// is. The store keeps the rule for any other writer.
#[tokio::test]
async fn an_empty_run_args_override_is_stored_as_inherit_for_every_class() {
    let (state, _dir) = state_with_dirs().await;
    let two = Some(vec!["--cpus".to_string(), "2".to_string()]);

    // Create with an empty list and no clear.
    ops::local_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "chat", "gguf_path": "chat.gguf",
            "extra_run_args": "\n  \n",
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    ops::aux_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "embed", "gguf_path": "embed.gguf",
            "kind": "embed", "extra_run_args": [],
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    ops::image_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "img",
            "files": {"diffusion_model": "diffusion.gguf"}, "extra_run_args": [],
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    ops::audio_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "tts", "family": "pocket_tts",
            "path": "tts", "task": "tts", "extra_run_args": [],
        })))
        .unwrap(),
    )
    .await
    .unwrap();
    let stored = |table: &'static str, id: &'static str| {
        let db = state.db.clone();
        async move {
            let sql = match table {
                "local_models" => "SELECT extra_run_args FROM local_models WHERE model_id = ?1",
                "aux_models" => "SELECT extra_run_args FROM aux_models WHERE model_id = ?1",
                "audio_models" => "SELECT extra_run_args FROM audio_models WHERE model_id = ?1",
                "image_models" => "SELECT extra_run_args FROM image_models WHERE model_id = ?1",
                other => panic!("no run args column in {other}"),
            };
            sqlx::query_scalar::<_, Option<String>>(sql)
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    for (table, id) in [
        ("local_models", "chat"),
        ("aux_models", "embed"),
        ("image_models", "img"),
        ("audio_models", "tts"),
    ] {
        assert_eq!(
            stored(table, id).await,
            None,
            "{table} create: NULL, not []"
        );
    }

    // Each gets an override, then an empty one on update: back to inherit.
    let snap = state.snapshot();
    let chat_id = snap
        .local_models
        .iter()
        .find(|m| m.model_id == "chat")
        .unwrap()
        .id;
    let aux_id = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == "embed")
        .unwrap()
        .id;
    let img_id = snap
        .image_models
        .iter()
        .find(|m| m.model_id == "img")
        .unwrap()
        .id;
    let tts_id = snap
        .audio_models
        .iter()
        .find(|m| m.model_id == "tts")
        .unwrap()
        .id;
    let set_all = |chat: Value, list: Value| {
        let state = state.clone();
        async move {
            ops::local_model_set(
                &state,
                ops::patch_from_args(args(json!({
                    "action": "update", "id": chat_id, "extra_run_args": chat,
                })))
                .unwrap(),
            )
            .await
            .unwrap();
            ops::aux_model_set(
                &state,
                ops::patch_from_args(args(json!({
                    "action": "update", "id": aux_id, "extra_run_args": list,
                })))
                .unwrap(),
            )
            .await
            .unwrap();
            ops::image_model_set(
                &state,
                ops::patch_from_args(args(json!({
                    "action": "update", "id": img_id, "extra_run_args": list,
                })))
                .unwrap(),
            )
            .await
            .unwrap();
            ops::audio_model_set(
                &state,
                ops::patch_from_args(args(json!({
                    "action": "update", "id": tts_id, "extra_run_args": list,
                })))
                .unwrap(),
            )
            .await
            .unwrap();
        }
    };
    let overrides = || {
        let snap = state.snapshot();
        [
            snap.local_models
                .iter()
                .find(|m| m.id == chat_id)
                .unwrap()
                .extra_run_args
                .clone(),
            snap.aux_models
                .iter()
                .find(|m| m.id == aux_id)
                .unwrap()
                .extra_run_args
                .clone(),
            snap.image_models
                .iter()
                .find(|m| m.id == img_id)
                .unwrap()
                .extra_run_args
                .clone(),
            snap.audio_models
                .iter()
                .find(|m| m.id == tts_id)
                .unwrap()
                .extra_run_args
                .clone(),
        ]
    };
    set_all(json!("--cpus 2"), json!(["--cpus", "2"])).await;
    assert_eq!(
        overrides(),
        [two.clone(), two.clone(), two.clone(), two.clone()]
    );
    // Blank text on aux and image: not supplied, the override stays.
    for (id, set) in [(aux_id, "aux_model_set"), (img_id, "image_model_set")] {
        let patch = args(json!({"action": "update", "id": id, "extra_run_args": ""}));
        match set {
            "aux_model_set" => ops::aux_model_set(&state, ops::patch_from_args(patch).unwrap())
                .await
                .unwrap(),
            _ => ops::image_model_set(&state, ops::patch_from_args(patch).unwrap())
                .await
                .unwrap(),
        };
    }
    assert_eq!(overrides()[1], two, "aux: blank text leaves it");
    assert_eq!(overrides()[2], two, "image: blank text leaves it");
    // An empty list on update: the class's args again.
    set_all(json!("\n"), json!([])).await;
    let [_, aux, img, tts] = overrides();
    assert_eq!((aux, img, tts), (None, None, None));
    for (table, id) in [
        ("aux_models", "embed"),
        ("image_models", "img"),
        ("audio_models", "tts"),
    ] {
        assert_eq!(
            stored(table, id).await,
            None,
            "{table} update: NULL, not []"
        );
    }
    // Chat: blank text is "not supplied" there (the field is text), so the
    // override stays until a clear.
    assert_eq!(overrides()[0], two);

    // Any other writer: the store keeps the rule itself.
    let row = store::get_audio_model(&state.db, tts_id)
        .await
        .unwrap()
        .unwrap();
    let save = store::NewAudioModel {
        model_id: row.model_id.clone(),
        family: row.family.clone(),
        path: row.path.clone(),
        task: row.task.clone(),
        mode: row.mode.clone(),
        lazy: row.lazy,
        busy_timeout_ms: row.busy_timeout_ms,
        backend: None,
        threads: None,
        load_options: row.load_options.clone(),
        session_options: row.session_options.clone(),
        default_request_options: row.default_request_options.clone(),
        model_spec_override: row.model_spec_override.clone(),
        config_id: row.config_id.clone(),
        weight_id: row.weight_id.clone(),
        voice_presets: row.voice_presets.clone(),
        default_voice_preset: row.default_voice_preset.clone(),
        enabled: row.enabled,
        image: row.image.clone(),
        extra_run_args: Some(Vec::new()),
        warm_start: row.warm_start,
        hold_fallback_mode: row.hold_fallback_mode,
        hold_fallback: row.hold_fallback.clone(),
    };
    store::update_audio_model(&state.db, tts_id, &save)
        .await
        .unwrap();
    assert_eq!(stored("audio_models", "tts").await, None);
}

/// [`HelpOnly`], keeping every argv it was asked to run.
#[derive(Default)]
struct HelpRecorder(std::sync::Mutex<Vec<Vec<String>>>);

#[async_trait::async_trait]
impl CommandRunner for HelpRecorder {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.0.lock().unwrap().push(args.to_vec());
        HelpOnly.run(program, args).await
    }
}

/// The ops level folds an empty override into "inherit" itself, not only
/// the store: an aux create's flag check runs the image's `--help` with the
/// run args the row will have, and with `[]` taken as an override it would
/// run without the class's — so the check would speak of a container that
/// never starts. The store's own fold cannot show this; the probe's argv
/// does.
#[tokio::test]
async fn a_create_s_flag_check_runs_with_the_class_args_an_empty_override_inherits() {
    let (state, _dir) = state_with_dirs().await;
    let runner = Arc::new(HelpRecorder::default());
    state.set_runtime_for_tests(Arc::new(Registry::new(
        runner.clone(),
        reqwest::Client::new(),
    )));
    let mut s = state.snapshot().settings.clone();
    s.aux_router.extra_run_args = vec!["--aux-class-flag".into()];
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    ops::aux_model_set(
        &state,
        ops::patch_from_args(args(json!({
            "action": "create", "model_id": "embed", "gguf_path": "embed.gguf",
            "kind": "embed", "extra_run_args": [],
        })))
        .unwrap(),
    )
    .await
    .unwrap();

    let helps: Vec<Vec<String>> = runner
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.iter().any(|x| x == "--help"))
        .cloned()
        .collect();
    assert!(
        helps
            .iter()
            .any(|a| a.iter().any(|x| x == "--aux-class-flag")),
        "no --help run carried the class's flag: {helps:?}"
    );
}
