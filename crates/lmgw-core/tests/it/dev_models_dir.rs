//! A dev instance never writes into a models directory outside its own data
//! dir (the owner's ruling of 2026-10-04 on chat-voice WP11 review m6).
//!
//! A dev copy keeps production's absolute models dirs, so every write lmgw
//! makes there on its own (a download and its retry, the delete of a tracked
//! download, a voice-library clip or its transcript, the image class's
//! LoRA/upscaler dirs) would land in the installed app's tree. Each test
//! stands production's models dir in with a temp dir of its own, outside the
//! test state's data dir, and checks the tree byte for byte afterwards. The
//! hub is a local mock: a refused download never reaches it.
//!
//! The image class's start, which goes ahead without the dirs, is in
//! `vram_admission::dev_image_start` (it needs that suite's fake podman).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lmgw_core::config::DEV_SHARED_MODELS_DIR;
use lmgw_core::jobs::hf_download;
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::acquire_spec;
use lmgw_core::runtime::Class;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// A gateway whose four models dirs are all `models`, else `models` inside
/// its own data dir; a dev instance or not. Returns the models dir.
async fn gateway(dev: bool, models: Option<&Path>) -> (SharedState, Gw, PathBuf) {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_dev_for_tests(dev);
    let models = models.map_or_else(|| state.data_dir.join("models"), Path::to_path_buf);
    let dir = models.display().to_string();
    let mut s = state.snapshot().settings.clone();
    s.router.models_dir = dir.clone();
    s.aux_router.models_dir = dir.clone();
    s.audio.models_dir = dir.clone();
    s.image.models_dir = dir;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw, models)
}

/// A finished download on disk (with a stray `.part` beside it) and its
/// tracked row, one clip with its transcript in the voice library, and an
/// image row. Returns the download's row id.
async fn seed(state: &SharedState, models: &Path) -> i64 {
    let file = models.join("o/r/m.gguf");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"GGUF weights").unwrap();
    std::fs::write(models.join("o/r/m.gguf.part"), b"GGUF half").unwrap();
    let voices = models.join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("me.wav"), b"RIFF synthetic clip").unwrap();
    std::fs::write(voices.join("prompt_text"), "me|hello there\n").unwrap();

    let id = store::upsert_hf_model(&state.db, "o/r", "m.gguf", "o/r/m.gguf", "chat")
        .await
        .unwrap();
    store::set_hf_status(&state.db, id, "done", None)
        .await
        .unwrap();
    let mut files = serde_json::Map::new();
    files.insert("diffusion_model".into(), json!("z.gguf"));
    store::insert_image_model(
        &state.db,
        &store::NewImageModel {
            model_id: "z".into(),
            files,
            enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

/// Every file and directory under `root` with its bytes: what "nothing
/// changed" is compared against.
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            if p.is_dir() {
                out.insert(rel, None);
                todo.push(p);
            } else {
                out.insert(rel, Some(std::fs::read(&p).unwrap()));
            }
        }
    }
    out
}

async fn post(gw: &Gw, route: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    post(gw, &format!("/api/op/{name}"), body).await
}

/// One clip `new.wav`, with a typed transcript, into the voice library.
async fn upload(gw: &Gw) -> (u16, Value) {
    let part = reqwest::multipart::Part::bytes(b"RIFF another synthetic clip".to_vec())
        .file_name("new.wav")
        .mime_str("audio/wav")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("transcript", "a typed line");
    let resp = gw
        .client()
        .post(format!("{gw}/audio-lab/api/refs"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The refusal: a 400 with the stable code, the rule and the way out, in
/// the admin plane's `{code, message}` or the Audio lab's `{error, code}`.
#[track_caller]
fn assert_refused((status, body): (u16, Value), what: &str) {
    assert_eq!(status, 400, "{what}: {body}");
    assert_eq!(body["code"], DEV_SHARED_MODELS_DIR, "{what}: {body}");
    let message = body["message"]
        .as_str()
        .or(body["error"].as_str())
        .unwrap_or_default();
    assert!(message.contains("outside its data dir"), "{what}: {body}");
    assert!(message.contains("folder of its own"), "{what}: {body}");
}

/// The image row's start spec: may it create its dirs in the models dir?
fn image_start_may_write(state: &SharedState) -> bool {
    let snap = state.snapshot();
    let rt = model_runtime(&snap, Class::Image, "z").expect("the seeded image row");
    acquire_spec(state, &snap, &rt).may_write_models_dir
}

/// A hub that serves one file, `o/r` `m.gguf` and `n.gguf` — or, with
/// `expect_none`, a hub that must not be asked at all.
async fn hub(expect_none: bool) -> MockServer {
    let hub = MockServer::start().await;
    let tree = json!([
        {"type": "file", "path": "m.gguf", "size": 12},
        {"type": "file", "path": "n.gguf", "size": 12},
    ]);
    let mut listing = Mock::given(method("GET"))
        .and(path("/api/models/o/r/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tree));
    let mut file = Mock::given(method("GET"))
        .and(path("/o/r/resolve/main/n.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GGUF new".to_vec()));
    if expect_none {
        listing = listing.expect(0);
        file = file.expect(0);
    }
    listing.mount(&hub).await;
    file.mount(&hub).await;
    hub
}

/// Wait for a tracked row to reach `done` or `failed`, and return it.
async fn settled(state: &SharedState, id: i64) -> store::HfModelRow {
    for _ in 0..250 {
        let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
        if (row.status == "done" || row.status == "failed") && state.jobs.live().is_empty() {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("download {id} never settled");
}

#[tokio::test]
async fn a_dev_instance_refuses_every_write_into_a_models_dir_outside_its_data_dir() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(true).await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    // Production's models dir, as a dev copy keeps it.
    let shared = tempfile::tempdir().unwrap();
    let (state, gw, _) = gateway(true, Some(shared.path())).await;
    let id = seed(&state, shared.path()).await;
    let before = tree(shared.path());

    // Downloads: a new one in each class, a retry or update of a tracked one,
    // and a recipe. All refused before the hub is asked or a row is written.
    for target in ["chat", "aux", "audio", "image"] {
        let args = json!({"repo": "o/r", "file": "n.gguf", "target": target});
        assert_refused(op(&gw, "hf_add", args).await, target);
    }
    let rows = store::list_hf_models(&state.db).await.unwrap();
    assert_eq!(rows.len(), 1, "a refused download writes no row: {rows:?}");
    let redownload = json!({"action": "redownload", "id": id});
    assert_refused(op(&gw, "hf_set", redownload).await, "redownload");
    let key = lmgw_core::image_recipes::keys()[0];
    assert_refused(
        op(&gw, "image_recipe_add", json!({"key": key})).await,
        "image_recipe_add",
    );

    // Deleting a tracked download keeps the file and the entry.
    let delete = json!({"action": "delete", "id": id});
    assert_refused(op(&gw, "hf_set", delete).await, "delete");
    assert!(store::get_hf_model(&state.db, id).await.unwrap().is_some());

    // The voice library: upload, transcript, transcription, delete.
    assert_refused(upload(&gw).await, "clip upload");
    let text = json!({"transcript": "changed"});
    let route = "/audio-lab/api/refs/me.wav/text";
    assert_refused(post(&gw, route, text).await, "transcript");
    let route = "/audio-lab/api/refs/me.wav/transcribe";
    assert_refused(
        post(&gw, route, json!({"alias": "asr"})).await,
        "transcribe",
    );
    let all = json!({"alias": "asr"});
    assert_refused(op(&gw, "voice_transcribe", all).await, "voice_transcribe");
    let route = "/audio-lab/api/refs/me.wav/delete";
    assert_refused(post(&gw, route, json!({})).await, "clip delete");

    // A row a previous run left queued: boot hands it straight to a
    // transfer, which refuses too, and says why on the row.
    let queued = store::upsert_hf_model(&state.db, "o/r", "n.gguf", "o/r/n.gguf", "chat")
        .await
        .unwrap();
    let row = store::get_hf_model(&state.db, queued)
        .await
        .unwrap()
        .unwrap();
    hf_download::start(&state, &row).await.unwrap();
    let row = settled(&state, queued).await;
    assert_eq!(row.status, "failed");
    let error = row.error.unwrap_or_default();
    assert!(error.contains(DEV_SHARED_MODELS_DIR), "{error}");

    // The image class: saving it creates no dirs and says so; a start would
    // not create them either.
    let image = json!({"image": {"public_prefix": "pictures"}});
    let (status, body) = op(&gw, "settings_set_full", image).await;
    assert_eq!(status, 200, "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("were not created"), "{body}");
    assert!(!image_start_may_write(&state));

    assert_eq!(
        tree(shared.path()),
        before,
        "production's models dir changed"
    );
}

#[tokio::test]
async fn inside_its_own_data_dir_a_dev_instance_writes_as_before() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(false).await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    // The way out the refusal names: a models dir inside the data dir.
    let (state, gw, models) = gateway(true, None).await;
    let id = seed(&state, &models).await;

    let args = json!({"repo": "o/r", "file": "n.gguf", "target": "chat"});
    let (status, body) = op(&gw, "hf_add", args).await;
    assert_eq!(status, 200, "{body}");
    let new_id = body["downloads"][0]["id"].as_i64().expect("the queued row");
    assert_eq!(settled(&state, new_id).await.status, "done");
    assert_eq!(
        std::fs::read(models.join("o/r/n.gguf")).unwrap(),
        b"GGUF new"
    );

    let (status, body) = upload(&gw).await;
    assert_eq!(status, 200, "{body}");
    assert!(models.join("voices/new.wav").is_file());
    let text = json!({"transcript": "changed"});
    let (status, body) = post(&gw, "/audio-lab/api/refs/me.wav/text", text).await;
    assert_eq!(status, 200, "{body}");
    let prompt = std::fs::read_to_string(models.join("voices/prompt_text")).unwrap();
    assert!(prompt.contains("me|changed"), "{prompt}");
    let (status, body) = post(&gw, "/audio-lab/api/refs/me.wav/delete", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert!(!models.join("voices/me.wav").exists());

    let delete = json!({"action": "delete", "id": id});
    let (status, body) = op(&gw, "hf_set", delete).await;
    assert_eq!(status, 200, "{body}");
    assert!(!models.join("o/r/m.gguf").exists());
    assert!(!models.join("o/r/m.gguf.part").exists());

    let image = json!({"image": {"public_prefix": "pictures"}});
    let (status, body) = op(&gw, "settings_set_full", image).await;
    assert_eq!(status, 200, "{body}");
    assert!(models.join("loras").is_dir() && models.join("upscalers").is_dir());
    assert!(image_start_may_write(&state));
}

/// Production writes into its models dir wherever it is: the rule is a dev
/// instance's alone. No download here, so no hub.
#[tokio::test]
async fn production_is_unaffected() {
    let models = tempfile::tempdir().unwrap();
    let (state, gw, _) = gateway(false, Some(models.path())).await;
    let id = seed(&state, models.path()).await;

    let (status, body) = upload(&gw).await;
    assert_eq!(status, 200, "{body}");
    assert!(models.path().join("voices/new.wav").is_file());
    let text = json!({"transcript": "changed"});
    let (status, body) = post(&gw, "/audio-lab/api/refs/me.wav/text", text).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&gw, "/audio-lab/api/refs/me.wav/delete", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let delete = json!({"action": "delete", "id": id});
    let (status, body) = op(&gw, "hf_set", delete).await;
    assert_eq!(status, 200, "{body}");
    assert!(!models.path().join("o/r/m.gguf").exists());
    let image = json!({"image": {"public_prefix": "pictures"}});
    let (status, body) = op(&gw, "settings_set_full", image).await;
    assert_eq!(status, 200, "{body}");
    assert!(models.path().join("loras").is_dir());
    assert!(image_start_may_write(&state));
}
