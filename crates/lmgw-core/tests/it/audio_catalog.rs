//! audio.cpp spec catalog end-to-end through the real `/api` plane: refresh
//! against a mock catalog host (listing each package repo on the mock hub,
//! page by page), the snapshot mapping the browser renders, installing a
//! package through the shared HF download queue, and the path from
//! "downloaded" to a configured audio model.
//!
//! Each test fn owns the process-wide `LMGW_AUDIO_CATALOG_ENDPOINT` and
//! `HF_ENDPOINT` overrides (same reason as `hf_download.rs`), behind
//! `common::process_env_lock` since `tests/it` folded every suite into one
//! binary. It restores both afterward — `audio_catalog_live.rs` relies on
//! the unset default and takes the same lock.

use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

async fn get_json(base: &Gw, p: &str) -> Value {
    let resp = base
        .client()
        .get(format!("{base}{p}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "GET {p}");
    resp.json().await.unwrap()
}

/// `POST /api/op/{name}` → `(status, parsed body)`.
async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("op {name} is not JSON ({e}): {body}"));
    (status, v)
}

/// One family with four packages: the recommended one is installable from the
/// mock hub, one names no download source at all, one lists a file its repo
/// does not publish (the Orukeet case), and one comes from a repo the hub
/// cannot list.
fn spec_json() -> Value {
    json!({
        "family": "pocket_tts",
        "display_name": "Pocket TTS",
        "description": "Small multilingual TTS.",
        "category": "tts",
        "tasks": ["clone", "tts"],
        "modes": ["offline", "streaming"],
        "languages": ["en", "de"],
        "ui": { "recommended_package": "pocket_tts_english_q8_0" },
        "packages": [
            {
                "id": "pocket_tts_english_q8_0",
                "display_name": "English · q8_0",
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "pocket-tts/english",
                "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF" },
                "files": ["english/model.gguf"],
            },
            {
                "id": "pocket_tts_byo",
                "display_name": "Bring your own weights",
                "format": "gguf",
                "precision": "",
                "files": ["byo/model.gguf"],
            },
            {
                "id": "pocket_tts_orukeet_q8_0",
                "display_name": "Orukeet · q8_0",
                "format": "gguf",
                "precision": "q8_0",
                "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/PocketTTS-GGUF" },
                "files": ["orukeet/model.gguf"],
            },
            {
                "id": "pocket_tts_gone",
                "display_name": "Gone",
                "format": "gguf",
                "precision": "f16",
                "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/Gone-GGUF" },
                "files": ["gone/model.gguf"],
            },
        ],
    })
}

#[tokio::test]
async fn catalog_refresh_install_and_serve_round_trip() {
    let _env = common::process_env_lock().await;
    // One mock origin serves both hosts the catalog uses (git-trees listing +
    // raw spec files) and the Hugging Face hub the package downloads from.
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [
                {"path": "README.md"},
                {"path": "model_specs/pocket_tts.json"},
                // Not served by the mock: a spec file that fails while
                // another loads is a warning, not dropped unsaid.
                {"path": "model_specs/broken.json"},
                {"path": "model_specs/notes.txt"},
            ],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec_json()))
        .mount(&hub)
        .await;
    let weights = b"GGUF\x00pocket tts english".to_vec();
    let voice = b"alba voice embedding".to_vec();
    // The tree listing comes in two pages, as the hub pages a big repo: the
    // voice the spec grows later is only on the second, so both the refresh's
    // check and the download have to follow the `Link`.
    let tree = "/api/models/audio-cpp/PocketTTS-GGUF/tree/main";
    Mock::given(method("GET"))
        .and(path(tree))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!(
                        "<{}{tree}?recursive=true&cursor=p2>; rel=\"next\"",
                        hub.uri()
                    )
                    .as_str(),
                )
                .set_body_json(json!([
                    {"type": "file", "path": "english/model.gguf", "size": weights.len()},
                    {"type": "file", "path": "README.md", "size": 12},
                ])),
        )
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(tree))
        .and(query_param("cursor", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"type": "file", "path": "english/embeddings/alba.safetensors", "size": voice.len()},
        ])))
        .with_priority(1)
        .mount(&hub)
        .await;
    // Downloaded once: completing the install later must not fetch it again.
    Mock::given(method("GET"))
        .and(path(
            "/audio-cpp/PocketTTS-GGUF/resolve/main/english/model.gguf",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"etag-v1\"")
                .set_body_bytes(weights.clone()),
        )
        .expect(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/audio-cpp/PocketTTS-GGUF/resolve/main/english/embeddings/alba.safetensors",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(voice.clone()))
        .expect(1)
        .mount(&hub)
        .await;
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = models_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    // Nothing cached: the endpoint answers empty rather than hitting the
    // network, and installing is refused until an explicit refresh.
    let v = get_json(&base, "/api/audio/catalog").await;
    assert_eq!(v["fetched_at"], json!(""));
    assert_eq!(v["families"], json!([]));
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_english_q8_0"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("refresh"),
        "{body}"
    );

    // Refresh fetches every `model_specs/*.json` (and ignores the rest). The
    // spec file that failed and the repo the hub could not list do not fail
    // it: they are its warnings, counted in the message.
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["families"], json!(1));
    let warnings = body["warnings"].as_array().unwrap().clone();
    assert_eq!(warnings.len(), 2, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("2 warnings"),
        "{body}"
    );
    let warned = |needle: &str| {
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains(needle))
    };
    assert!(warned("model_specs/broken.json"), "{warnings:?}");
    assert!(warned("could not list audio-cpp/Gone-GGUF"), "{warnings:?}");

    let v = get_json(&base, "/api/audio/catalog").await;
    assert!(!v["fetched_at"].as_str().unwrap().is_empty());
    let fam = &v["families"][0];
    assert_eq!(fam["family"], json!("pocket_tts"));
    assert_eq!(fam["display_name"], json!("Pocket TTS"));
    assert_eq!(fam["category"], json!("tts"));
    assert_eq!(fam["languages"], json!(["en", "de"]));
    assert_eq!(fam["any_installed"], json!(false));
    let pkg = &fam["packages"][0];
    assert_eq!(pkg["id"], json!("pocket_tts_english_q8_0"));
    assert_eq!(pkg["recommended"], json!(true));
    assert_eq!(pkg["precision"], json!("q8_0"));
    assert_eq!(pkg["file_count"], json!(1));
    assert_eq!(pkg["repo"], json!("audio-cpp/PocketTTS-GGUF"));
    assert_eq!(pkg["installed"], json!(false));
    // Prefill for the create form: `clone` is not a server task, it maps to
    // `clon`; the path is the download layout's dir.
    assert_eq!(pkg["suggested_model_id"], json!("pocket-tts-english-q8-0"));
    assert_eq!(
        pkg["suggested_path"],
        json!("audio-cpp/PocketTTS-GGUF/english")
    );
    assert_eq!(pkg["suggested_task"], json!("clon"));
    // The family streams, so its rows do (audio-class gap 8): a streaming
    // row answers a plain request with one WAV too.
    assert_eq!(pkg["suggested_mode"], json!("streaming"));
    // The catalog keeps the warnings until the next refresh.
    assert_eq!(v["warnings"], json!(warnings));
    // Published or not, as the refresh listed it: the recommended package's
    // file is there, the Orukeet-like one is not, the unlistable repo's is
    // unknown — Download stays for that one, with the reason beside it.
    assert_eq!(pkg["unpublished_files"], json!([]));
    assert_eq!(pkg["availability_note"], json!(""));
    let orukeet = &fam["packages"][2];
    assert_eq!(orukeet["unpublished_files"], json!(["orukeet/model.gguf"]));
    assert!(
        orukeet["availability_note"]
            .as_str()
            .unwrap()
            .starts_with("audio-cpp/PocketTTS-GGUF has no orukeet/model.gguf (listed "),
        "{orukeet}"
    );
    let gone = &fam["packages"][3];
    assert_eq!(gone["unpublished_files"], json!([]));
    assert!(
        gone["availability_note"]
            .as_str()
            .unwrap()
            .starts_with("could not check whether audio-cpp/Gone-GGUF publishes these files"),
        "{gone}"
    );
    // The queue refuses the unpublished file, as it always did.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_orukeet_q8_0"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("not found in"),
        "{body}"
    );
    // A package with no download source in the spec cannot be installed.
    assert_eq!(fam["packages"][1]["repo"], json!(""));
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_byo"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("no download source"),
        "{body}"
    );

    // Unknown family/package and a missing argument are rejected the same way.
    for args in [
        json!({"action": "download", "family": "nope", "package": "pocket_tts_english_q8_0"}),
        json!({"action": "download", "family": "pocket_tts", "package": "nope"}),
        json!({"action": "download", "family": "pocket_tts"}),
        json!({"action": "install"}),
    ] {
        let (status, body) = op(&base, "audio_catalog", args.clone()).await;
        assert_eq!(status, 400, "{args} → {body}");
    }

    // Install: the package's files go through the shared HF download queue
    // with target `audio`, exactly like a wizard download.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_english_q8_0"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["files_queued"], json!(1));
    let dl_id = body["downloads"][0]["id"].as_i64().unwrap();
    assert_eq!(body["downloads"][0]["file"], json!("english/model.gguf"));

    let mut status = String::new();
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let v = get_json(&base, "/api/hf/downloads").await;
        let row = v["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(dl_id))
            .cloned()
            .unwrap();
        assert_eq!(row["target"], json!("audio"));
        status = row["status"].as_str().unwrap().to_string();
        if status == "done" || status == "failed" {
            break;
        }
    }
    assert_eq!(status, "done");
    assert_eq!(
        std::fs::read(
            models_dir
                .path()
                .join("audio-cpp/PocketTTS-GGUF/english/model.gguf")
        )
        .unwrap(),
        weights
    );

    // Downloaded → the browser flips to installed, with the on-disk size and
    // the tracked row id it polls progress from.
    let v = get_json(&base, "/api/audio/catalog").await;
    let pkg = &v["families"][0]["packages"][0];
    assert_eq!(pkg["installed"], json!(true));
    assert_eq!(pkg["partial"], json!(false));
    assert_eq!(pkg["served"], json!(false));
    assert!(!pkg["size"].as_str().unwrap().is_empty(), "{pkg}");
    assert_eq!(pkg["download_ids"], json!([dl_id]));
    assert_eq!(v["families"][0]["any_installed"], json!(true));

    // The prefill creates a real audio model (the old page's per-package
    // "serve" form; the row is never created behind the user's back).
    let (status, body) = op(
        &base,
        "audio_model_set",
        json!({
            "action": "create",
            "model_id": pkg["suggested_model_id"],
            "family": "pocket_tts",
            "path": pkg["suggested_path"],
            "task": pkg["suggested_task"],
            "mode": pkg["suggested_mode"],
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/audio/catalog").await;
    assert_eq!(v["families"][0]["served"], json!(true));
    assert_eq!(v["families"][0]["packages"][0]["served"], json!(true));

    // The spec grows a file after the package was installed (audio-class
    // gap 4: Pocket's English `alba` voice): the package is incomplete, names
    // what it lacks, and a download fetches only that.
    let mut grown = spec_json();
    grown["packages"][0]["files"] =
        json!(["english/model.gguf", "english/embeddings/alba.safetensors"]);
    grown["ui"]["builtin_voices"] = json!(["alba"]);
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(grown))
        .with_priority(1)
        .mount(&hub)
        .await;
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/audio/catalog").await;
    let pkg = &v["families"][0]["packages"][0];
    assert_eq!(pkg["installed"], json!(false), "{pkg}");
    assert_eq!(pkg["incomplete"], json!(true), "{pkg}");
    // The new file is on the listing's second page: published.
    assert_eq!(pkg["unpublished_files"], json!([]), "{pkg}");
    assert_eq!(
        pkg["missing_files"],
        json!(["english/embeddings/alba.safetensors"])
    );
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_english_q8_0"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["files_queued"], json!(1), "{body}");
    assert_eq!(
        body["downloads"][0]["file"],
        json!("english/embeddings/alba.safetensors")
    );
    assert!(body["message"].as_str().unwrap().starts_with("completing"));
    let alba = body["downloads"][0]["id"].as_i64().unwrap();
    let mut status = String::new();
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let v = get_json(&base, "/api/hf/downloads").await;
        let row = v["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(alba))
            .cloned()
            .unwrap();
        status = row["status"].as_str().unwrap().to_string();
        if status == "done" || status == "failed" {
            break;
        }
    }
    assert_eq!(status, "done");
    let v = get_json(&base, "/api/audio/catalog").await;
    let pkg = &v["families"][0]["packages"][0];
    assert_eq!(pkg["installed"], json!(true), "{pkg}");
    assert_eq!(pkg["missing_files"], json!([]));
    // Whole now: a download has nothing to fetch.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "pocket_tts", "package": "pocket_tts_english_q8_0"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["files_queued"], json!(0));

    // The row's voice list has the built-in voice now.
    let voices = "/v1/audio/voices?model=audio/pocket-tts-english-q8-0";
    let v = get_json(&base, voices).await;
    assert_eq!(v["voices"], json!(["alba"]), "{v}");
    assert_eq!(v["lmgw"]["missing"], json!([]));

    // A file that went from disk is missing again, though its row says done
    // — in the catalog, the voice list and the model's notes alike.
    std::fs::remove_file(
        models_dir
            .path()
            .join("audio-cpp/PocketTTS-GGUF/english/embeddings/alba.safetensors"),
    )
    .unwrap();
    let v = get_json(&base, "/api/audio/catalog").await;
    let pkg = &v["families"][0]["packages"][0];
    assert_eq!(pkg["incomplete"], json!(true), "{pkg}");
    assert_eq!(
        pkg["missing_files"],
        json!(["english/embeddings/alba.safetensors"])
    );
    let v = get_json(&base, voices).await;
    assert_eq!(v["lmgw"]["missing"], json!(["alba"]), "{v}");
    let v = get_json(&base, "/v1/models").await;
    let notes = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "audio/pocket-tts-english-q8-0")
        .map(|m| m["notes"].to_string())
        .unwrap();
    assert!(
        notes.contains("whose file is missing here: alba"),
        "{notes}"
    );

    // An unreachable catalog host fails loudly and leaves the cached snapshot
    // (and the page) intact.
    let dead = MockServer::start().await;
    let dead_uri = dead.uri();
    drop(dead);
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", &dead_uri);
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], json!("op_failed"));
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("listing audio.cpp specs"),
        "{body}"
    );
    let v = get_json(&base, "/api/audio/catalog").await;
    assert_eq!(v["families"][0]["family"], json!("pocket_tts"));

    // Both overrides go back as they were when `_env` drops.
}

/// A spec file that fails on a later refresh keeps its family as the refresh
/// before had it (REVIEW.md M2), through the real refresh: the carry is only
/// as good as `catalog_refresh` handing it the snapshot it replaces.
#[tokio::test]
async fn a_spec_file_that_fails_later_keeps_its_family() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [
                {"path": "model_specs/other.json"},
                {"path": "model_specs/pocket_tts.json"},
            ],
        })))
        .mount(&hub)
        .await;
    // A refresh that loads no spec at all fails as a whole; this one always
    // loads.
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/other.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "family": "other", "category": "tts", "tasks": ["tts"], "packages": [],
        })))
        .mount(&hub)
        .await;
    // Served once; the next refresh gets a 500 for it.
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec_json()))
        .up_to_n_times(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/pocket_tts.json"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&hub)
        .await;
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["families"], json!(2), "{body}");

    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("model_specs/pocket_tts.json")),
        "the failed file is said: {warnings:?}"
    );
    assert_eq!(body["families"], json!(2), "{body}");
    let v = get_json(&base, "/api/audio/catalog").await;
    let pocket = v["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["family"] == "pocket_tts")
        .unwrap_or_else(|| panic!("the family is kept: {v}"));
    assert_eq!(
        pocket["packages"][0]["id"],
        json!("pocket_tts_english_q8_0"),
        "carried with its packages"
    );

    // Both overrides go back as they were when `_env` drops.
}
