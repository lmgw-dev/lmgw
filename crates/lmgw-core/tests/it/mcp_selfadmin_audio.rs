//! The audio class on the self-admin tool plane (realtime fix package B7):
//! `lmgw__audio_catalog` lists, refreshes and downloads audio.cpp packages,
//! `lmgw__audio_model_set` creates, updates and deletes audio rows through the
//! same function the dashboard's op calls, and `lmgw__local_model_get
//! target=audio` reads one back. Both write tools are gone at 'read only'.
//!
//! Before this an agent could download a single audio file (`lmgw__hf_add
//! target=audio`) and then had no way to turn it into a model.

use lmgw_core::config::{SelfAdmin, Settings};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;

const ADMIN_TOKEN: &str = "audio-tools-token";

async fn gateway(mode: SelfAdmin) -> (SharedState, String, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(models.path().join("pocket/german")).unwrap();
    let mut s = Settings {
        self_admin: mode,
        ..Settings::default()
    };
    s.audio.models_dir = models.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        ADMIN_TOKEN,
        true,
    )
    .await
    .unwrap();
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, base, models)
}

async fn rpc(base: &str, sid: Option<&str>, body: Value) -> (Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&body);
    if let Some(sid) = sid {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (sid, resp.json().await.unwrap())
}

async fn session(base: &str) -> String {
    let (sid, _) = rpc(
        base,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                          "clientInfo": {"name": "audio-tools", "version": "0"}}}),
    )
    .await;
    sid.expect("a session id")
}

async fn tools(base: &str, sid: &str) -> Vec<Value> {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    body["result"]["tools"].as_array().unwrap().clone()
}

/// `(is_error, text)` of one tool call.
async fn call(base: &str, sid: &str, name: &str, args: Value) -> (bool, String) {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert!(body["error"].is_null(), "{name}: {body}");
    let r = &body["result"];
    (
        r["isError"] == json!(true),
        r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// A successful call's JSON payload.
async fn ok(base: &str, sid: &str, name: &str, args: Value) -> Value {
    let (err, text) = call(base, sid, name, args).await;
    assert!(!err, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn the_audio_tools_are_listed_at_full_and_hidden_at_read_only() {
    let (_state, base, _dir) = gateway(SelfAdmin::Full).await;
    let sid = session(&base).await;
    let listed = tools(&base, &sid).await;
    let tool = |name: &str| {
        listed
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    };
    let catalog = tool("lmgw__audio_catalog");
    assert_eq!(
        catalog["inputSchema"]["properties"]["action"]["enum"],
        json!(["list", "refresh", "download"])
    );
    let set = tool("lmgw__audio_model_set");
    for prop in [
        "action",
        "model_id",
        "family",
        "path",
        "task",
        "voice_presets",
        "clear",
    ] {
        assert!(
            set["inputSchema"]["properties"].get(prop).is_some(),
            "audio_model_set.{prop}"
        );
    }
    let desc = set["description"].as_str().unwrap();
    for teach in [
        "lmgw__audio_catalog",
        "/v1/audio/speech",
        "residency",
        "lmgw__local_model_get target=audio",
    ] {
        assert!(desc.contains(teach), "the description never says {teach}");
    }
    let get = tool("lmgw__local_model_get");
    assert!(get["inputSchema"]["properties"]["target"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("audio")));

    let (_state, ro_base, _dir) = gateway(SelfAdmin::ReadOnly).await;
    let ro_sid = session(&ro_base).await;
    let names: Vec<String> = tools(&ro_base, &ro_sid)
        .await
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for hidden in ["lmgw__audio_catalog", "lmgw__audio_model_set"] {
        assert!(!names.contains(&hidden.to_string()), "{hidden}: {names:?}");
        let (err, text) = call(&ro_base, &ro_sid, hidden, json!({"action": "list"})).await;
        assert!(
            err && text.contains("read"),
            "{hidden} at read only: {text}"
        );
    }
    assert!(names.contains(&"lmgw__local_model_get".to_string()));
}

/// Create from flat arguments, read back, update by name, reset the learned
/// residency, delete — and a field named in `clear` is unset on create too.
#[tokio::test]
async fn an_audio_row_is_created_read_updated_and_deleted_through_the_tools() {
    let (state, base, _dir) = gateway(SelfAdmin::Full).await;
    let sid = session(&base).await;

    let created = ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({
            "action": "create", "model_id": "tts-de", "family": "pocket_tts",
            "path": "pocket/german", "task": "tts",
            "load_options": "{\"language\": \"de\"}",
            "voice_presets": "{\"alba\": {\"voice_id\": \"alba\"}}",
            "default_voice_preset": "alba",
            "extra_run_args": "--cpus 2 --memory 4g",
        }),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    assert!(
        created["next_step"]
            .as_str()
            .unwrap()
            .contains("/v1/audio/speech"),
        "{created}"
    );
    let id = created["id"].as_i64().unwrap();

    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "tts-de"}),
    )
    .await;
    assert_eq!(got["class"], "audio", "{got}");
    crate::common::round_trips::<lmgw_api_types::LocalModelRead>("audio local_model_get", &got);
    assert_eq!(got["public_name"], "audio/tts-de");
    assert_eq!(got["path_present"], true);
    assert_eq!(got["load_options"]["language"], "de");
    assert_eq!(got["voice_presets"]["alba"]["voice_id"], "alba");
    assert_eq!(got["extra_run_args"], "--cpus 2\n--memory 4g");
    assert_eq!(got["server_json"]["models"][0]["id"], "tts-de", "{got}");
    assert!(got["residency_note"]
        .as_str()
        .unwrap()
        .contains("not learned"));
    assert_eq!(got["problems"], json!([]));

    // Update by model_id alone (no id), and reset what a request taught it.
    let key = lmgw_core::vram::residency::resident_key(
        &state.snapshot().audio_models[0],
        &state.snapshot().settings.audio,
    );
    store::set_audio_model_residency(&state.db, id, Some((3 << 30, &key)))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let updated = ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "model_id": "tts-de",
               "session_options": "{\"weight_type\": \"q8_0\"}",
               "clear": "residency extra_run_args"}),
    )
    .await;
    assert_eq!(updated["residency_reset"], true, "{updated}");
    let row = state.snapshot().audio_models[0].clone();
    assert_eq!(row.model_id, "tts-de", "selected, not renamed");
    assert_eq!(row.session_options["weight_type"], "q8_0");
    assert_eq!(row.load_options["language"], "de", "left as it was");
    assert_eq!(row.residency, None);
    assert_eq!(row.extra_run_args, None, "the class's args again");

    // A bad map is refused by name, before anything is written.
    let (err, text) = call(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "id": id, "load_options": "[1, 2]"}),
    )
    .await;
    assert!(err && text.contains("load_options"), "{text}");

    // Create with a cleared run-args field inherits the class's, whatever
    // value came with it — the dashboard's bug, on this plane too.
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "create", "model_id": "asr", "family": "qwen3_asr",
               "path": "pocket/german", "task": "asr",
               "extra_run_args": "--cpus 2", "clear": "extra_run_args"}),
    )
    .await;
    let asr = state
        .snapshot()
        .audio_models
        .iter()
        .find(|m| m.model_id == "asr")
        .cloned()
        .unwrap();
    assert_eq!(asr.extra_run_args, None);

    let gone = ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "delete", "model_id": "tts-de"}),
    )
    .await;
    assert_eq!(gone["ok"], true);
    assert!(!state
        .snapshot()
        .audio_models
        .iter()
        .any(|m| m.model_id == "tts-de"));
    let (err, _) = call(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "tts-de", "target": "audio"}),
    )
    .await;
    assert!(err, "deleted");
}

/// list → refresh → list (with and without a family) → download, through the
/// tool, against a mock catalog host and a mock hub.
#[tokio::test]
async fn the_catalog_is_listed_refreshed_and_downloaded_through_the_tool() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [{"path": "model_specs/pocket_tts.json"}],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "family": "pocket_tts",
            "display_name": "Pocket TTS",
            "description": "Small multilingual TTS.",
            "category": "tts",
            "tasks": ["tts"],
            "modes": ["offline"],
            "languages": ["de"],
            "options": {"load": [{"name": "language", "kind": "string"}]},
            "packages": [{
                "id": "pocket_tts_german_q8_0",
                "display_name": "German · q8_0",
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "pocket-tts/german",
                "download": {"kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF"},
                "files": ["german/model.gguf"],
            }],
        })))
        .mount(&hub)
        .await;
    let weights = b"GGUF\x00pocket tts german".to_vec();
    Mock::given(method("GET"))
        .and(path("/api/models/audio-cpp/PocketTTS-GGUF/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"type": "file", "path": "german/model.gguf", "size": weights.len()},
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/audio-cpp/PocketTTS-GGUF/resolve/main/german/model.gguf",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(weights.clone()))
        .mount(&hub)
        .await;
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let (_state, base, models) = gateway(SelfAdmin::Full).await;
    let sid = session(&base).await;

    let empty = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "list"}),
    )
    .await;
    assert_eq!(empty["families"], json!([]));
    assert!(empty["message"]
        .as_str()
        .unwrap()
        .contains("action=refresh"));

    let refreshed = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "refresh"}),
    )
    .await;
    assert_eq!(refreshed["families"], 1);

    let all = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "list"}),
    )
    .await;
    let fam = &all["families"][0];
    assert_eq!(fam["family"], "pocket_tts");
    assert!(
        fam["options"]
            .as_str()
            .unwrap()
            .contains("family=pocket_tts"),
        "the listing says where the options are: {fam}"
    );
    let pkg = &fam["packages"][0];
    assert_eq!(pkg["suggested_model_id"], "pocket-tts-german-q8-0");
    assert_eq!(pkg["suggested_path"], "audio-cpp/PocketTTS-GGUF/german");
    assert_eq!(pkg["installed"], false);

    let one = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "list", "family": "pocket_tts"}),
    )
    .await;
    assert_eq!(one["families"][0]["options"]["load"][0]["name"], "language");
    let none = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "list", "search": "no-such-thing"}),
    )
    .await;
    assert_eq!(none["families"], json!([]));

    let (err, text) = call(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "download", "family": "pocket_tts"}),
    )
    .await;
    assert!(err && text.contains("pass family and package"), "{text}");

    let queued = ok(
        &base,
        &sid,
        "lmgw__audio_catalog",
        json!({"action": "download", "family": "pocket_tts",
               "package": "pocket_tts_german_q8_0"}),
    )
    .await;
    assert_eq!(queued["files_queued"], 1, "{queued}");
    assert!(queued["next_step"]
        .as_str()
        .unwrap()
        .contains("lmgw__audio_model_set"));
    let file = models
        .path()
        .join("audio-cpp/PocketTTS-GGUF/german/model.gguf");
    for _ in 0..300 {
        if std::fs::read(&file).ok().as_deref() == Some(&weights[..]) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read(&file).unwrap(), weights);
}

/// The CPU switch through the tools: `backend: cpu` and `threads` are saved
/// and read back with where the row runs and what it renders — the class's
/// run args without the GPU passthrough, `server_json` on the CPU — a count
/// above this machine's CPUs is saved with a note, anything but `cpu` is
/// refused by name, and `clear` returns both to the class.
#[tokio::test]
async fn an_audio_row_is_switched_to_the_cpu_and_back_through_the_tools() {
    let (state, base, _dir) = gateway(SelfAdmin::Full).await;
    let sid = session(&base).await;
    let created = ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "create", "model_id": "asr", "family": "parakeet_tdt",
               "path": "pocket/german", "task": "asr", "backend": "cpu"}),
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    let host = lmgw_core::host::cpu();

    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "asr"}),
    )
    .await;
    assert_eq!(got["backend"], "cpu", "{got}");
    assert_eq!(got["runs_on"], "cpu");
    assert_eq!(got["threads_in_effect"], host.physical_cores as u64);
    assert_eq!(got["threads_source"], "cores");
    assert_eq!(got["server_json"]["backend"], "cpu");
    assert_eq!(
        got["server_json"]["threads"], host.physical_cores as u64,
        "{got}"
    );
    let args = got["effective_extra_run_args"].as_str().unwrap();
    assert!(!args.contains("nvidia.com/gpu"), "{args}");
    assert!(args.contains("label=disable"), "{args}");
    assert!(args.contains("NVIDIA_VISIBLE_DEVICES=void"), "{args}");
    assert_eq!(got["problems"], json!([]), "{got}");
    assert!(got["residency_note"]
        .as_str()
        .unwrap()
        .starts_with("runs on the CPU"));

    let many = host.logical_cpus as i64 + 8;
    let updated = ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "id": id, "threads": many}),
    )
    .await;
    assert!(
        updated["message"]
            .as_str()
            .unwrap()
            .contains(&format!("threads {many} is more than this machine's")),
        "{updated}"
    );
    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "asr"}),
    )
    .await;
    assert_eq!(
        (&got["threads"], &got["threads_source"]),
        (&json!(many), &json!("row"))
    );

    // Its own args are used as written; when they still pass the GPU, the
    // row says so.
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "id": id,
               "extra_run_args": "--device nvidia.com/gpu=all\n--security-opt label=disable"}),
    )
    .await;
    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "asr"}),
    )
    .await;
    assert!(got["effective_extra_run_args"]
        .as_str()
        .unwrap()
        .contains("nvidia.com/gpu=all"));
    let problems = got["problems"].to_string();
    assert!(problems.contains("still pass the GPU"), "{problems}");
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "id": id, "clear": "extra_run_args"}),
    )
    .await;

    for (args, word) in [
        (
            json!({"action": "update", "id": id, "backend": "vulkan"}),
            "'vulkan'",
        ),
        (
            json!({"action": "update", "id": id, "threads": 0}),
            "threads",
        ),
    ] {
        let (err, text) = call(&base, &sid, "lmgw__audio_model_set", args).await;
        assert!(err && text.contains(word), "{text}");
    }

    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "update", "id": id, "clear": "backend threads"}),
    )
    .await;
    let row = state.snapshot().audio_models[0].clone();
    assert_eq!((row.backend, row.threads), (None, None));
    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "asr"}),
    )
    .await;
    assert_eq!(
        (&got["runs_on"], &got["threads_source"]),
        (&json!("gpu"), &json!("class"))
    );
    assert!(got["effective_extra_run_args"]
        .as_str()
        .unwrap()
        .contains("nvidia.com/gpu=all"));
}

/// A row whose path is the GGUF file itself — audio.cpp loads a file path
/// as it is — is present, and that file is what lmgw takes it to load.
#[tokio::test]
async fn a_row_whose_path_is_its_gguf_file_is_present() {
    let (_state, base, dir) = gateway(SelfAdmin::Full).await;
    std::fs::write(dir.path().join("pocket/german/pocket-q8_0.gguf"), [0u8; 64]).unwrap();
    let sid = session(&base).await;
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "create", "model_id": "file-row", "family": "pocket_tts",
               "path": "pocket/german/pocket-q8_0.gguf", "task": "tts"}),
    )
    .await;
    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "file-row"}),
    )
    .await;
    assert_eq!(got["path_present"], true, "{got}");
    assert_eq!(got["problems"], json!([]), "{got}");
    assert_eq!(
        got["server_json"]["models"][0]["path"],
        "/models/pocket/german/pocket-q8_0.gguf"
    );
}

/// A row's GGUF linked out of the models dir — into a cache the container
/// does not mount — is dangling to audio.cpp. lmgw does not count it either
/// (one GGUF left: the directory is handed over as it is), and the row's
/// problems name the link and its target rather than leave the difference
/// unsaid. A link inside the models dir counts and is no problem.
#[tokio::test]
async fn a_gguf_link_out_of_the_models_dir_is_named_and_not_counted() {
    let (_state, base, dir) = gateway(SelfAdmin::Full).await;
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(cache.path().join("blob"), [0u8; 64]).unwrap();
    let root = dir.path().join("pocket/linked");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("pocket-q8_0.gguf"), [0u8; 64]).unwrap();
    std::os::unix::fs::symlink(cache.path().join("blob"), root.join("pocket-f16.gguf")).unwrap();
    let sid = session(&base).await;
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "create", "model_id": "linked", "family": "pocket_tts",
               "path": "pocket/linked", "task": "tts", "weight_id": "q8_0"}),
    )
    .await;
    let get = || async {
        ok(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({"model_id": "linked"}),
        )
        .await
    };
    let got = get().await;
    let problems = got["problems"].to_string();
    assert!(
        problems.contains("point outside the models directory")
            && problems.contains(&format!(
                "pocket-f16.gguf -> {}",
                cache.path().join("blob").display()
            )),
        "{problems}"
    );
    assert_eq!(
        got["server_json"]["models"][0]["path"],
        "/models/pocket/linked"
    );

    // Linked inside the models dir instead: two GGUFs, the q8_0 one picked.
    std::fs::rename(cache.path().join("blob"), dir.path().join("blob")).unwrap();
    std::fs::remove_file(root.join("pocket-f16.gguf")).unwrap();
    std::os::unix::fs::symlink("../../blob", root.join("pocket-f16.gguf")).unwrap();
    let got = get().await;
    assert_eq!(got["problems"], json!([]), "{got}");
    assert_eq!(
        got["server_json"]["models"][0]["path"],
        "/models/pocket/linked/pocket-q8_0.gguf"
    );
}

/// A row whose directory is itself a link out of the models dir: every GGUF
/// in it is dangling to audio.cpp, though none is a link of its own, so lmgw
/// counts none and hands over the directory — and the row's problems name
/// the directory's link instead of saying nothing.
#[tokio::test]
async fn a_row_directory_linked_out_of_the_models_dir_is_named() {
    let (_state, base, dir) = gateway(SelfAdmin::Full).await;
    let elsewhere = tempfile::tempdir().unwrap();
    for f in ["pocket-q8_0.gguf", "pocket-f16.gguf"] {
        std::fs::write(elsewhere.path().join(f), [0u8; 64]).unwrap();
    }
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("pocket/away")).unwrap();
    let sid = session(&base).await;
    ok(
        &base,
        &sid,
        "lmgw__audio_model_set",
        json!({"action": "create", "model_id": "away", "family": "pocket_tts",
               "path": "pocket/away", "task": "tts", "weight_id": "q8_0"}),
    )
    .await;
    let got = ok(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({"model_id": "away"}),
    )
    .await;
    let problems = got["problems"].to_string();
    assert!(
        problems.contains("point outside the models directory")
            && problems.contains(&format!("pocket/away/ -> {}", elsewhere.path().display())),
        "{problems}"
    );
    assert_eq!(
        got["server_json"]["models"][0]["path"],
        "/models/pocket/away"
    );
}
