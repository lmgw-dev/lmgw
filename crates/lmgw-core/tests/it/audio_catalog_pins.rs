//! Audio catalog downloads at the revision a spec pins, end to end through
//! the real `/api` plane against a mock hub and a mock CDN.
//!
//! Covers the whole `audio.catalog_revision` contract: under `pinned` (the
//! default) a pinned package is listed and fetched at its commit, the row
//! records the commit the hub named on its own redirect (not the CDN's
//! answer, which does not carry it), a pin the hub does not have fails
//! without touching `main`, and the update check compares against the pin;
//! under `latest` the update check and a re-download take `main`.
//!
//! Owns `HF_ENDPOINT` and `LMGW_AUDIO_CATALOG_ENDPOINT` behind
//! `common::process_env_lock`, like `audio_catalog.rs`.

use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

/// The commit the spec pins the Fun-ASR packages to.
const PIN: &str = "607a30d783dfa663caf39e06633721c8d4cfcd7e";
/// A commit the hub does not have (the third-party repo was rewritten).
const GONE: &str = "00000000000000000000000000000000000000aa";
/// What `main` points at when the latest is fetched.
const MAIN_AT: &str = "1111111111111111111111111111111111111111";
const REPO: &str = "third/Fun-ASR-GGUF";

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

/// The download row `id` once it is `done` or `failed`.
async fn settled(base: &Gw, id: i64) -> Value {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let v = get_json(base, "/api/hf/downloads").await;
        let row = v["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .cloned()
            .unwrap();
        if row["status"] == "done" || row["status"] == "failed" {
            return row;
        }
    }
    panic!("download {id} never settled");
}

fn package<'a>(catalog: &'a Value, id: &str) -> &'a Value {
    catalog["families"][0]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == json!(id))
        .unwrap()
}

/// Requests the hub received for `method path`.
async fn hits(hub: &MockServer, m: &str, p: &str) -> usize {
    hub.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == m && r.url.path() == p)
        .count()
}

fn spec_json() -> Value {
    json!({
        "family": "fun_asr",
        "display_name": "Fun-ASR",
        "category": "asr",
        "tasks": ["asr"],
        "modes": ["offline"],
        // The family default pins the third-party repo.
        "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": REPO, "revision": PIN } },
        "packages": [
            { "id": "fun_q8", "display_name": "q8_0", "format": "gguf", "precision": "q8_0",
              "files": ["q8/model.gguf"], "download": null },
            // Listed at the pin, but the file there is gone by transfer time.
            { "id": "fun_f16", "display_name": "f16", "format": "gguf", "precision": "f16",
              "files": ["f16/model.gguf"] },
            // The hub is rate-limiting when this one is fetched at the pin.
            { "id": "fun_busy", "display_name": "busy", "format": "gguf", "precision": "q4",
              "files": ["busy/model.gguf"] },
            { "id": "fun_gone", "display_name": "gone", "format": "gguf", "precision": "q4",
              "files": ["gone/model.gguf"],
              "download": { "kind": "huggingface_snapshot", "repo": "third/Gone-GGUF", "revision": GONE } },
        ],
    })
}

#[tokio::test]
async fn catalog_downloads_follow_the_spec_pin_unless_set_to_latest() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    let cdn = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [{"path": "model_specs/fun_asr.json"}],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/fun_asr.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec_json()))
        .mount(&hub)
        .await;
    let tree = |files: &[&str]| {
        let v: Vec<Value> = files
            .iter()
            .map(|f| json!({"type": "file", "path": f}))
            .collect();
        ResponseTemplate::new(200).set_body_json(v)
    };
    // main lost the q8 file in a re-upload; the pin still has it.
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/main")))
        .respond_with(tree(&["f16/model.gguf"]))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/{PIN}")))
        .respond_with(tree(&[
            "q8/model.gguf",
            "f16/model.gguf",
            "busy/model.gguf",
        ]))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/models/third/Gone-GGUF/tree/main"))
        .respond_with(tree(&["gone/model.gguf"]))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/models/third/Gone-GGUF/tree/{GONE}")))
        .respond_with(ResponseTemplate::new(404).insert_header("x-error-code", "RevisionNotFound"))
        .mount(&hub)
        .await;

    // The pinned q8 file: the hub's 302 names the commit, the CDN serves the
    // bytes without it — and never sees the token.
    let pinned_bytes = b"GGUF\x00pinned q8".to_vec();
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/{PIN}/q8/model.gguf")))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("x-repo-commit", PIN)
                .insert_header("location", format!("{}/blob/q8-pinned", cdn.uri()).as_str()),
        )
        .expect(1)
        .mount(&hub)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/{REPO}/resolve/{PIN}/q8/model.gguf")))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("x-repo-commit", PIN)
                .insert_header("location", format!("{}/blob/q8-pinned", cdn.uri()).as_str()),
        )
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/blob/q8-pinned"))
        .and(|r: &Request| !r.headers.contains_key("authorization"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"cdn-pinned\"")
                .set_body_bytes(pinned_bytes.clone()),
        )
        .mount(&cdn)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/blob/q8-pinned"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"cdn-pinned\""))
        .mount(&cdn)
        .await;
    // main's q8 (once `latest` re-downloads it): the hub's 302 names the
    // commit main points at, the CDN does not. Unlike a pinned row, a main row
    // has no requested commit to fall back on, so MAIN_AT recorded on the row
    // can only have come from the hub's hop.
    let latest_bytes = b"GGUF\x00latest q8".to_vec();
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/main/q8/model.gguf")))
        .and(|r: &Request| r.headers.contains_key("authorization"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("x-repo-commit", MAIN_AT)
                .insert_header("location", format!("{}/blob/q8-main", cdn.uri()).as_str()),
        )
        .expect(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/blob/q8-main"))
        .and(|r: &Request| !r.headers.contains_key("authorization"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"main-q8\"")
                .set_body_bytes(latest_bytes.clone()),
        )
        .expect(1)
        .mount(&cdn)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/{REPO}/resolve/main/q8/model.gguf")))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"main-q8\""))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{REPO}/resolve/{PIN}/busy/model.gguf")))
        .respond_with(ResponseTemplate::new(429))
        .mount(&hub)
        .await;
    // Never asked: a pin that fails is not swapped for main.
    for never in [
        format!("/{REPO}/resolve/main/busy/model.gguf"),
        format!("/{REPO}/resolve/main/f16/model.gguf"),
        "/third/Gone-GGUF/resolve/main/gone/model.gguf".to_string(),
    ] {
        Mock::given(method("GET"))
            .and(path(never))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&hub)
            .await;
    }
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = models_dir.path().display().to_string();
    settings.hf_token = "tok".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    // A fresh install follows pins, and says so where settings are read.
    let s = get_json(&base, "/api/settings-full").await;
    assert_eq!(s["audio"]["catalog_revision"], json!("pinned"), "{s}");

    // Refresh lists the repo at main and at the pin.
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    crate::common::round_trips::<lmgw_api_types::AudioCatalogAnswer>(
        "audio_catalog refresh",
        &body,
    );
    let warnings = body["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{body}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .starts_with("could not list third/Gone-GGUF at 0000000 on Hugging Face"),
        "{warnings:?}"
    );

    let v = get_json(&base, "/api/audio/catalog").await;
    let q8 = package(&v, "fun_q8");
    assert_eq!(q8["pinned_commit"], json!(PIN), "{q8}");
    assert_eq!(q8["pin_followed"], json!(true));
    // Published at the pin, which is what the download takes — though main
    // lost it.
    assert_eq!(q8["unpublished_files"], json!([]), "{q8}");
    assert_eq!(q8["availability_note"], json!(""), "{q8}");
    let gone = package(&v, "fun_gone");
    assert_eq!(gone["pinned_commit"], json!(GONE));
    assert!(
        gone["availability_note"]
            .as_str()
            .unwrap()
            .starts_with("could not check whether third/Gone-GGUF at 0000000 publishes"),
        "{gone}"
    );

    // A pin the hub does not have: refused at queue time, naming the setting.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "fun_asr", "package": "fun_gone"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains(&format!("has no revision {GONE}")), "{msg}");
    assert!(
        msg.contains("does not fall back to main") && msg.contains("audio.catalog_revision"),
        "{msg}"
    );

    // The pinned package: listed and fetched at the pin.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "fun_asr", "package": "fun_q8"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    crate::common::round_trips::<lmgw_api_types::AudioCatalogAnswer>(
        "audio_catalog download",
        &body,
    );
    assert_eq!(body["revision"], json!(PIN));
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("at commit 607a30d, the one the spec pins"),
        "{body}"
    );
    let q8_id = body["downloads"][0]["id"].as_i64().unwrap();
    let row = settled(&base, q8_id).await;
    assert_eq!(row["status"], "done", "{row}");
    assert_eq!(row["requested_revision"], json!(PIN));
    assert_eq!(
        row["resolved_commit"],
        json!(PIN),
        "from the hub's redirect, not the CDN"
    );
    let on_disk = models_dir.path().join(format!("{REPO}/q8/model.gguf"));
    assert_eq!(std::fs::read(&on_disk).unwrap(), pinned_bytes);
    let v = get_json(&base, "/api/audio/catalog").await;
    assert_eq!(
        package(&v, "fun_q8")["downloaded_from"],
        json!("downloaded at commit 607a30d, the pinned one")
    );

    // Listed at the pin, gone from it by the time the file is fetched: the
    // row fails with the pin named, and main is not asked.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "fun_asr", "package": "fun_f16"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let f16 = settled(&base, body["downloads"][0]["id"].as_i64().unwrap()).await;
    assert_eq!(f16["status"], "failed", "{f16}");
    let err = f16["error"].as_str().unwrap();
    assert!(
        err.contains("404") && err.contains("does not fall back to main"),
        "{err}"
    );
    // A rate limit at the pin says nothing about the pin: no advice to give
    // it up, and still no fetch at main.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "fun_asr", "package": "fun_busy"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let busy = settled(&base, body["downloads"][0]["id"].as_i64().unwrap()).await;
    assert_eq!(busy["status"], "failed", "{busy}");
    let err = busy["error"].as_str().unwrap();
    assert!(err.contains("429"), "{err}");
    assert!(!err.contains("fall back"), "{err}");

    // The update check compares the pinned row with its pin: no update.
    let (status, body) = op(
        &base,
        "hf_set",
        json!({"action": "check_updates", "target": "audio"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let main_q8 = format!("/{REPO}/resolve/main/q8/model.gguf");
    assert_eq!(hits(&hub, "HEAD", &main_q8).await, 0, "main is not asked");
    assert!(
        hits(
            &hub,
            "HEAD",
            &format!("/{REPO}/resolve/{PIN}/q8/model.gguf")
        )
        .await
            >= 1
    );
    let row = settled(&base, q8_id).await;
    assert_eq!(row["status"], "done", "no update the spec does not endorse");

    // `latest`: the catalog says the pin is not followed, the update check
    // compares with main, and a re-download takes main.
    let (status, body) = op(
        &base,
        "settings_set",
        json!({"audio_catalog_revision": "latest"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = op(
        &base,
        "settings_set",
        json!({"audio_catalog_revision": "newest"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let v = get_json(&base, "/api/audio/catalog").await;
    let q8 = package(&v, "fun_q8");
    assert_eq!(q8["pin_followed"], json!(false));
    assert_eq!(q8["pinned_commit"], json!(PIN));
    // At main the q8 file is not published: the listing main was taken at.
    assert_eq!(q8["unpublished_files"], json!(["q8/model.gguf"]), "{q8}");

    let (_, _) = op(
        &base,
        "hf_set",
        json!({"action": "check_updates", "target": "audio"}),
    )
    .await;
    assert_eq!(hits(&hub, "HEAD", &main_q8).await, 1);
    let v = get_json(&base, "/api/hf/downloads").await;
    let row = v["downloads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(q8_id))
        .unwrap()
        .clone();
    assert_eq!(row["status"], "update_available", "{row}");

    let (status, body) = op(
        &base,
        "hf_set",
        json!({"action": "redownload", "id": q8_id, "target": "audio"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let row = settled(&base, q8_id).await;
    assert_eq!(row["status"], "done", "{row}");
    assert_eq!(row["requested_revision"], json!("main"));
    assert_eq!(
        row["resolved_commit"],
        json!(MAIN_AT),
        "from the hub's redirect: a main download has no commit to fall back on"
    );
    assert_eq!(std::fs::read(&on_disk).unwrap(), latest_bytes);

    // A row from before revisions were recorded: both null — the revision
    // was main then, the commit is unknown.
    sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target, status) \
         VALUES ('o/old', 'm.gguf', 'o/old/m.gguf', 'audio', 'done')",
    )
    .execute(&state.db)
    .await
    .unwrap();
    let v = get_json(&base, "/api/hf/downloads").await;
    let old = v["downloads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["repo"] == "o/old")
        .unwrap()
        .clone();
    assert!(
        old["requested_revision"].is_null() && old["resolved_commit"].is_null(),
        "{old}"
    );

    // Both overrides go back as they were when `_env` drops.
}

/// The pin a spec names today, not the one a row was taken at, is what
/// `pinned` follows (REVIEW-2 M1): the spec moves its pin, and the update
/// check offers the new one and Update fetches it; a file of the package
/// downloaded at `main` before pins were followed joins the pin instead of
/// tracking `main` on its own. A flag one setting raised is cleared when the
/// other finds the file current (L4), and a re-queue during a live transfer
/// leaves the row saying what the transfer fetched (L3).
#[tokio::test]
async fn pinned_follows_the_pin_the_spec_names_now() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    const MOVED: &str = "third/Moved-GGUF";
    const NEW: &str = "2222222222222222222222222222222222222222";

    let spec = |pin: &str| {
        json!({
            "family": "moved", "display_name": "Moved", "category": "asr",
            "tasks": ["asr"], "modes": ["offline"],
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": MOVED, "revision": pin } },
            "packages": [
                { "id": "m", "display_name": "m", "format": "gguf", "files": ["m.gguf"] },
                { "id": "old", "display_name": "old", "format": "gguf", "files": ["old.gguf"] },
            ],
        })
    };
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [{"path": "model_specs/moved.json"}],
        })))
        .mount(&hub)
        .await;
    // The first refresh reads the spec at PIN; every later one at NEW.
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/moved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec(PIN)))
        .up_to_n_times(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/moved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec(NEW)))
        .mount(&hub)
        .await;
    for rev in ["main", PIN, NEW] {
        Mock::given(method("GET"))
            .and(path(format!("/api/models/{MOVED}/tree/{rev}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"type": "file", "path": "m.gguf"},
                {"type": "file", "path": "old.gguf"},
            ])))
            .mount(&hub)
            .await;
    }
    // A file at a revision: its bytes, the commit on the answer, an ETag per
    // revision (old.gguf at PIN is not what main had).
    let at = |rev: &str, file: &str| format!("/{MOVED}/resolve/{rev}/{file}");
    let commit_of = |rev: &str| match rev {
        "main" => MAIN_AT.to_string(),
        r => r.to_string(),
    };
    for (rev, file, delay_ms) in [
        (PIN, "m.gguf", 2000u64),
        (NEW, "m.gguf", 0),
        (NEW, "old.gguf", 0),
    ] {
        Mock::given(method("GET"))
            .and(path(at(rev, file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-repo-commit", commit_of(rev).as_str())
                    .insert_header("etag", format!("\"{file}@{}\"", &rev[..4]).as_str())
                    .set_body_bytes(format!("{file} at {rev}").into_bytes())
                    .set_delay(std::time::Duration::from_millis(delay_ms)),
            )
            .expect(1)
            .mount(&hub)
            .await;
    }
    for (rev, file) in [
        (PIN, "m.gguf"),
        (PIN, "old.gguf"),
        (NEW, "m.gguf"),
        (NEW, "old.gguf"),
        ("main", "m.gguf"),
        ("main", "old.gguf"),
    ] {
        Mock::given(method("HEAD"))
            .and(path(at(rev, file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", format!("\"{file}@{}\"", &rev[..4]).as_str()),
            )
            .mount(&hub)
            .await;
    }
    // A pinned package's file is never fetched at main, under pinned.
    Mock::given(method("GET"))
        .and(path(at("main", "old.gguf")))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
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
    let set_mode = |mode: &'static str| {
        let base = base.clone();
        async move {
            let (status, body) = op(
                &base,
                "settings_set",
                json!({"audio_catalog_revision": mode}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
    };
    let check = || {
        let base = base.clone();
        async move {
            let (status, body) = op(
                &base,
                "hf_set",
                json!({"action": "check_updates", "target": "audio"}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
    };
    let redownload = |id: i64| {
        let base = base.clone();
        async move {
            op(
                &base,
                "hf_set",
                json!({"action": "redownload", "id": id, "target": "audio"}),
            )
            .await
        }
    };

    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");

    // old.gguf was downloaded at main before lmgw followed pins: a row with
    // no requested revision, main's file on disk.
    sqlx::query(
        "INSERT INTO hf_models (repo, file, dest_path, target, status, etag) \
         VALUES (?1, 'old.gguf', ?2, 'audio', 'done', 'old.gguf@main')",
    )
    .bind(MOVED)
    .bind(format!("{MOVED}/old.gguf"))
    .execute(&state.db)
    .await
    .unwrap();
    std::fs::create_dir_all(models_dir.path().join(MOVED)).unwrap();
    std::fs::write(
        models_dir.path().join(MOVED).join("old.gguf"),
        b"old at main",
    )
    .unwrap();
    let old_id: i64 = sqlx::query_scalar("SELECT id FROM hf_models WHERE file = 'old.gguf'")
        .fetch_one(&state.db)
        .await
        .unwrap();

    // m.gguf at PIN — slow, so a redownload and a re-queue arrive mid-transfer.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "moved", "package": "m"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m_id = body["downloads"][0]["id"].as_i64().unwrap();
    set_mode("latest").await;
    let (status, body) = redownload(m_id).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("already downloading"),
        "{body}"
    );
    let row = store::get_hf_model(&state.db, m_id).await.unwrap().unwrap();
    assert_eq!(
        row.requested_revision.as_deref(),
        Some(PIN),
        "a refused redownload leaves the live row's revision alone"
    );
    // A catalog download under latest re-queues the in-flight file at main;
    // the transfer that runs still fetches the pin, and the row ends saying so.
    let (status, body) = op(
        &base,
        "audio_catalog",
        json!({"action": "download", "family": "moved", "package": "m"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let row = settled(&base, m_id).await;
    assert_eq!(row["status"], "done", "{row}");
    assert_eq!(row["requested_revision"], json!(PIN), "{row}");
    assert_eq!(row["resolved_commit"], json!(PIN), "{row}");
    set_mode("pinned").await;

    // Under pinned, the main row is compared with the pin, not with main.
    check().await;
    assert_eq!(hits(&hub, "HEAD", &at("main", "old.gguf")).await, 0);
    let old = store::get_hf_model(&state.db, old_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.status, "update_available", "{old:?}");
    let m = store::get_hf_model(&state.db, m_id).await.unwrap().unwrap();
    assert_eq!(m.status, "done", "at its pin: nothing to offer");

    // The spec moves its pin.
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/audio/catalog").await;
    let pkg = package(&v, "m");
    assert_eq!(pkg["pinned_commit"], json!(NEW));
    let from = pkg["downloaded_from"].as_str().unwrap();
    assert!(
        from.starts_with("downloaded at commit 607a30d, a pin the spec has moved from")
            && from.contains("the spec pins 2222222"),
        "{from}"
    );

    // The check offers NEW for both rows, and Update fetches it.
    check().await;
    assert!(hits(&hub, "HEAD", &at(NEW, "m.gguf")).await >= 1);
    let m = store::get_hf_model(&state.db, m_id).await.unwrap().unwrap();
    assert_eq!(m.status, "update_available", "{m:?}");
    for id in [m_id, old_id] {
        let (status, body) = redownload(id).await;
        assert_eq!(status, 200, "{body}");
        let row = settled(&base, id).await;
        assert_eq!(row["status"], "done", "{row}");
        assert_eq!(row["requested_revision"], json!(NEW), "{row}");
        assert_eq!(row["resolved_commit"], json!(NEW), "{row}");
    }
    assert_eq!(
        std::fs::read(models_dir.path().join(MOVED).join("old.gguf")).unwrap(),
        format!("old.gguf at {NEW}").into_bytes()
    );
    let v = get_json(&base, "/api/audio/catalog").await;
    assert_eq!(
        package(&v, "m")["downloaded_from"],
        json!("downloaded at commit 2222222, the pinned one")
    );

    // latest flags m against main; back under pinned the file matches its
    // pin, so the flag goes.
    set_mode("latest").await;
    check().await;
    let m = store::get_hf_model(&state.db, m_id).await.unwrap().unwrap();
    assert_eq!(m.status, "update_available");
    set_mode("pinned").await;
    check().await;
    let m = store::get_hf_model(&state.db, m_id).await.unwrap().unwrap();
    assert_eq!(m.status, "done", "the flag latest raised is cleared");

    // Both overrides go back as they were when `_env` drops.
}

/// A spec grows a file and moves its pin in one go — authors re-test the
/// files together. Complete install under pinned fetches the new file at
/// the new pin, and the installed GGUF from the old pin with it: one
/// package is not left from two commits. A sibling package sharing that
/// GGUF does the same.
#[tokio::test]
async fn complete_install_under_pinned_brings_the_installed_files_to_the_pin() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    const GROWN: &str = "third/Grown-GGUF";
    const NEW: &str = "3333333333333333333333333333333333333333";

    let spec = |pin: &str, files: Value, sibling: Value| {
        json!({
            "family": "grown", "display_name": "Grown", "category": "asr",
            "tasks": ["asr"], "modes": ["offline"],
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": GROWN, "revision": pin } },
            "packages": [
                { "id": "p", "display_name": "p", "format": "gguf", "files": files },
                { "id": "sib", "display_name": "sib", "format": "gguf", "files": sibling },
            ],
        })
    };
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [{"path": "model_specs/grown.json"}],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/grown.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec(
            PIN,
            json!(["m.gguf"]),
            json!(["s.gguf"]),
        )))
        .up_to_n_times(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/grown.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec(
            NEW,
            json!(["m.gguf", "voice.gguf"]),
            json!(["s.gguf", "voice.gguf"]),
        )))
        .mount(&hub)
        .await;
    for rev in ["main", PIN, NEW] {
        Mock::given(method("GET"))
            .and(path(format!("/api/models/{GROWN}/tree/{rev}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"type": "file", "path": "m.gguf"},
                {"type": "file", "path": "s.gguf"},
                {"type": "file", "path": "voice.gguf"},
            ])))
            .mount(&hub)
            .await;
    }
    let at = |rev: &str, file: &str| format!("/{GROWN}/resolve/{rev}/{file}");
    for (rev, file, times) in [
        (PIN, "m.gguf", 1u64),
        (PIN, "s.gguf", 1),
        (NEW, "m.gguf", 1),
        (NEW, "voice.gguf", 1),
        (NEW, "s.gguf", 1),
    ] {
        Mock::given(method("GET"))
            .and(path(at(rev, file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-repo-commit", rev)
                    .insert_header("etag", format!("\"{file}@{}\"", &rev[..4]).as_str())
                    .set_body_bytes(format!("{file} at {rev}").into_bytes()),
            )
            .expect(times)
            .mount(&hub)
            .await;
    }
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = models_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let download = |package: &'static str| {
        let base = base.clone();
        async move {
            let (status, body) = op(
                &base,
                "audio_catalog",
                json!({"action": "download", "family": "grown", "package": package}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };

    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    for package in ["p", "sib"] {
        let body = download(package).await;
        settled(&base, body["downloads"][0]["id"].as_i64().unwrap()).await;
    }

    // The spec moves to NEW and grows voice.gguf in both packages.
    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let body = download("p").await;
    assert_eq!(body["files_queued"], 2, "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("it lacks: voice.gguf")
            && message.contains("from another commit than the pin")
            && message.contains("m.gguf"),
        "{message}"
    );
    for d in body["downloads"].as_array().unwrap() {
        let row = settled(&base, d["id"].as_i64().unwrap()).await;
        assert_eq!(row["status"], "done", "{row}");
        assert_eq!(row["requested_revision"], json!(NEW), "{row}");
    }
    assert_eq!(
        std::fs::read(models_dir.path().join(GROWN).join("m.gguf")).unwrap(),
        format!("m.gguf at {NEW}").into_bytes()
    );

    // The sibling: voice.gguf is at NEW now, s.gguf still at PIN.
    let body = download("sib").await;
    assert_eq!(body["files_queued"], 1, "{body}");
    let id = body["downloads"][0]["id"].as_i64().unwrap();
    assert_eq!(body["downloads"][0]["file"], "s.gguf", "{body}");
    let row = settled(&base, id).await;
    assert_eq!(row["requested_revision"], json!(NEW), "{row}");
    // Every file is at the pin: nothing more to fetch.
    let body = download("p").await;
    assert_eq!(body["files_queued"], 0, "{body}");
}

/// A pin the spec moved without touching this file: the update check finds
/// the same bytes at the new pin, flags nothing, and the row records that
/// pin — so nothing goes on promising an update that never comes. A file
/// that did change is flagged, and keeps the revision it was taken at.
#[tokio::test]
async fn an_unchanged_file_at_a_moved_pin_is_recorded_at_that_pin() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    const NEW: &str = "4444444444444444444444444444444444444444";
    for (file, etag) in [("same.gguf", "\"same\""), ("moved.gguf", "\"moved-v2\"")] {
        Mock::given(method("HEAD"))
            .and(path(format!("/o/r/resolve/{NEW}/{file}")))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", etag))
            .mount(&hub)
            .await;
    }
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    for (file, etag) in [("same.gguf", "same"), ("moved.gguf", "moved-v1")] {
        sqlx::query(
            "INSERT INTO hf_models (repo, file, dest_path, target, status, etag, \
             requested_revision, resolved_commit) \
             VALUES ('o/r', ?1, ?2, 'audio', 'done', ?3, ?4, ?4)",
        )
        .bind(file)
        .bind(format!("o/r/{file}"))
        .bind(etag)
        .bind(PIN)
        .execute(&state.db)
        .await
        .unwrap();
    }
    let rows = store::list_hf_models(&state.db).await.unwrap();
    for row in &rows {
        let changed = lmgw_core::hf::check_update_at(&state, row, NEW)
            .await
            .unwrap();
        assert_eq!(changed, row.file == "moved.gguf", "{}", row.file);
    }
    let rows = store::list_hf_models(&state.db).await.unwrap();
    let same = rows.iter().find(|r| r.file == "same.gguf").unwrap();
    assert_eq!(
        (
            same.status.as_str(),
            same.requested_revision.as_deref(),
            same.resolved_commit.as_deref()
        ),
        ("done", Some(NEW), Some(NEW))
    );
    let moved = rows.iter().find(|r| r.file == "moved.gguf").unwrap();
    assert_eq!(
        (
            moved.status.as_str(),
            moved.requested_revision.as_deref(),
            moved.resolved_commit.as_deref()
        ),
        ("update_available", Some(PIN), Some(PIN))
    );
}

/// A spec moves its pin and grows a file, and of the installed files one is
/// byte-identical at the new pin and one is not. Complete install asks the
/// hub for each one's ETag at the pin first, all at once: the unchanged
/// GGUF is recorded at the pin, its update flag taken back, and not fetched
/// again, the changed one comes along with the new file, and the message
/// names both.
#[tokio::test]
async fn complete_install_under_pinned_does_not_fetch_a_file_the_pin_left_unchanged() {
    let _env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    const KEPT: &str = "third/Kept-GGUF";
    const NEW: &str = "5555555555555555555555555555555555555555";

    let spec = |pin: &str, files: Value| {
        json!({
            "family": "kept", "display_name": "Kept", "category": "asr",
            "tasks": ["asr"], "modes": ["offline"],
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": KEPT, "revision": pin } },
            "packages": [{ "id": "p", "display_name": "p", "format": "gguf", "files": files }],
        })
    };
    Mock::given(method("GET"))
        .and(path("/repos/0xShug0/audio.cpp/git/trees/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": [{"path": "model_specs/kept.json"}],
        })))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/kept.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(spec(PIN, json!(["m.gguf", "t.gguf"]))),
        )
        .up_to_n_times(1)
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/0xShug0/audio.cpp/main/model_specs/kept.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(spec(NEW, json!(["m.gguf", "t.gguf", "voice.gguf"]))),
        )
        .mount(&hub)
        .await;
    for rev in ["main", PIN, NEW] {
        Mock::given(method("GET"))
            .and(path(format!("/api/models/{KEPT}/tree/{rev}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"type": "file", "path": "m.gguf"},
                {"type": "file", "path": "t.gguf"},
                {"type": "file", "path": "voice.gguf"},
            ])))
            .mount(&hub)
            .await;
    }
    let at = |rev: &str, file: &str| format!("/{KEPT}/resolve/{rev}/{file}");
    // m.gguf has the same bytes at both pins; t.gguf (the tokenizer) changed.
    let etag = |file: &str, rev: &str| match file {
        "m.gguf" => "\"m-same\"".to_string(),
        _ => format!("\"{file}@{}\"", &rev[..4]),
    };
    for (rev, file, times) in [
        (PIN, "m.gguf", 1u64),
        (PIN, "t.gguf", 1),
        (NEW, "m.gguf", 0),
        (NEW, "t.gguf", 1),
        (NEW, "voice.gguf", 1),
    ] {
        Mock::given(method("GET"))
            .and(path(at(rev, file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-repo-commit", rev)
                    .insert_header("etag", etag(file, rev).as_str())
                    .set_body_bytes(format!("{file} at {rev}").into_bytes()),
            )
            .expect(times)
            .mount(&hub)
            .await;
    }
    // Each HEAD at the pin is slow to answer: asked one after another, the
    // click would wait out both.
    const HEAD_TAKES: std::time::Duration = std::time::Duration::from_secs(2);
    for file in ["m.gguf", "t.gguf"] {
        Mock::given(method("HEAD"))
            .and(path(at(NEW, file)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", etag(file, NEW).as_str())
                    .set_delay(HEAD_TAKES),
            )
            .expect(1)
            .mount(&hub)
            .await;
    }
    std::env::set_var("LMGW_AUDIO_CATALOG_ENDPOINT", hub.uri());
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let state = AppState::init_for_tests().await.unwrap();
    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = models_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let download = || {
        let base = base.clone();
        async move {
            let (status, body) = op(
                &base,
                "audio_catalog",
                json!({"action": "download", "family": "kept", "package": "p"}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };

    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let body = download().await;
    for d in body["downloads"].as_array().unwrap() {
        settled(&base, d["id"].as_i64().unwrap()).await;
    }
    // Flagged by an earlier check under latest: recording the pin takes the
    // flag back, or Update would fetch the same bytes again.
    sqlx::query("UPDATE hf_models SET status = 'update_available' WHERE file = 'm.gguf'")
        .execute(&state.db)
        .await
        .unwrap();

    let (status, body) = op(&base, "audio_catalog", json!({"action": "refresh"})).await;
    assert_eq!(status, 200, "{body}");
    let asked = std::time::Instant::now();
    let body = download().await;
    let took = asked.elapsed();
    assert!(
        took < HEAD_TAKES * 2,
        "the HEADs at the pin went out one after another: {took:?}"
    );
    assert_eq!(body["files_queued"], 2, "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("it lacks: voice.gguf")
            && message.contains("from another commit than the pin")
            && message.contains("t.gguf")
            && message.contains("recorded at it rather than fetched again: m.gguf"),
        "{message}"
    );
    for d in body["downloads"].as_array().unwrap() {
        let row = settled(&base, d["id"].as_i64().unwrap()).await;
        assert_eq!(row["status"], "done", "{row}");
    }
    let rows = store::list_hf_models(&state.db).await.unwrap();
    let m = rows.iter().find(|r| r.file == "m.gguf").unwrap();
    assert_eq!(
        (
            m.status.as_str(),
            m.requested_revision.as_deref(),
            m.resolved_commit.as_deref()
        ),
        ("done", Some(NEW), Some(NEW))
    );
    assert_eq!(
        std::fs::read(models_dir.path().join(KEPT).join("m.gguf")).unwrap(),
        format!("m.gguf at {PIN}").into_bytes(),
        "the file on disk was not fetched again"
    );
    // Every file is at the pin now.
    let body = download().await;
    assert_eq!(body["files_queued"], 0, "{body}");
}
