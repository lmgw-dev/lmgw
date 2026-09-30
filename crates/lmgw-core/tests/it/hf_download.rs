//! HF model manager end-to-end against a mock hub (§15): tracked-row CRUD,
//! the streaming download into the models dir — now as an `hf_download` job
//! (§9c) — and the ETag update check.
//!
//! Since WP3 of the image-generation design it also covers the **per-target
//! file kinds** (§7.1): the three llama.cpp-shaped classes stay GGUF-only, and
//! `image` takes the whole stable-diffusion.cpp set, because a pipeline's VAE
//! and text encoders are `.safetensors` and a GGUF-only gate would make the
//! class undownloadable. And the **gated-repo sentence**: a 401 from the hub
//! is a licence that has not been accepted, not an HTTP status to print.
//!
//! The hub-touching tests share one fn: it owns the process-wide `HF_ENDPOINT`
//! override, behind `common::process_env_lock` since `tests/it` folded every
//! suite into one binary. The purely local rules get their own.

use lmgw_core::hf;
use lmgw_core::jobs::{self, hf_download, JobKind};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewImageModel};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Per-target file kinds and destination paths (no hub, no I/O)
// ---------------------------------------------------------------------------

/// The one rule that decides whether this class can be downloaded at all.
#[test]
fn accepted_file_kinds_are_per_target() {
    for t in ["chat", "aux", "audio"] {
        assert_eq!(hf::accepted_extensions(t), &[".gguf"], "{t}");
        assert!(hf::accepts_file(t, "m-Q4_K_M.gguf"));
        assert!(!hf::accepts_file(t, "ae.safetensors"), "{t} is GGUF-only");
        assert!(!hf::accepts_file(t, "v1-5-pruned.ckpt"), "{t}");
    }
    for f in [
        "z_image_turbo-Q4_K.gguf",
        "split_files/vae/ae.safetensors",
        "v1-5-pruned-emaonly.ckpt",
        "model.pt",
        "model.pth",
        // Case is not a property of a file kind.
        "SD_XL_BASE_1.0.SAFETENSORS",
    ] {
        assert!(hf::accepts_file("image", f), "image should accept {f}");
    }
    assert!(!hf::accepts_file("image", "README.md"));
    assert!(!hf::accepts_file("image", "config.json"));
    assert!(hf::accepted_extensions_phrase("image").contains(".safetensors"));

    // The fourth target is a real one now, and it resolves to its own dir.
    assert_eq!(hf::TARGETS.len(), 4);
    assert_eq!(hf::normalize_target("image").unwrap(), "image");
    assert_eq!(hf::models_dir_setting("image"), "image.models_dir");
    let mut settings = lmgw_core::config::Settings::default();
    settings.image.models_dir = "/srv/sdcpp".into();
    assert_eq!(hf::models_dir_for_target(&settings, "image"), "/srv/sdcpp");
    settings.image.models_dir.clear();
    let err = hf::models_dir_or_refuse(&settings, "image").unwrap_err();
    assert!(err.contains("image.models_dir"), "{err}");
}

/// A pipeline's components sit several directories deep inside their repo —
/// the layout has to survive that, and still refuse to leave the models dir.
#[test]
fn nested_component_paths_keep_the_owner_repo_file_layout() {
    assert_eq!(
        hf::dest_rel_path("Comfy-Org/z_image_turbo", "split_files/vae/ae.safetensors").unwrap(),
        "Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors"
    );
    assert_eq!(
        hf::dest_rel_path(
            "QuantStack/Qwen-Image-GGUF",
            "VAE/Qwen_Image-VAE.safetensors"
        )
        .unwrap(),
        "QuantStack/Qwen-Image-GGUF/VAE/Qwen_Image-VAE.safetensors"
    );
    assert!(hf::dest_rel_path("a/b", "split_files/../../escape.safetensors").is_err());
}

/// A 401 is a licence nobody accepted, and the message has to say that rather
/// than print a status code at an owner who cannot act on it.
#[test]
fn the_gated_sentence_names_the_repo_and_both_things_that_must_be_true() {
    let msg = hf::gated_message("black-forest-labs/FLUX.1-dev");
    assert!(msg.contains("gated repo"), "{msg}");
    assert!(
        msg.contains("Hugging Face token under Settings → Tokens & updates"),
        "{msg}"
    );
    assert!(msg.contains("accept the licence on the hub"), "{msg}");
    assert!(msg.contains("black-forest-labs/FLUX.1-dev"), "{msg}");

    use reqwest::StatusCode;
    assert!(hf::is_gated_response(StatusCode::UNAUTHORIZED, "", false));
    assert!(hf::is_gated_response(
        StatusCode::FORBIDDEN,
        r#"{"error":"Access to model black-forest-labs/FLUX.1-dev is restricted."}"#,
        false
    ));
    // A 403 that is about something else stays an HTTP error: turning a rate
    // limit into licence advice would send the owner to the wrong page.
    assert!(!hf::is_gated_response(
        StatusCode::FORBIDDEN,
        r#"{"error":"rate limit reached, try again in 60s"}"#,
        false
    ));
    assert!(!hf::is_gated_response(
        StatusCode::NOT_FOUND,
        "gated",
        false
    ));
}

/// The same 401 means two different things, and only one of them is about a
/// licence: with a token on the request the token is what the hub rejected,
/// and "accept the licence" is advice for a page the owner has already
/// accepted. Both sentences say what to do next.
#[test]
fn a_401_with_a_token_reports_the_token_not_the_licence() {
    use reqwest::StatusCode;
    let repo = "black-forest-labs/FLUX.1-dev";
    let body = r#"{"error":"Invalid credentials in Authorization header"}"#;

    let with = hf::hub_refusal(StatusCode::UNAUTHORIZED, body, repo, true).expect("a sentence");
    assert!(with.contains("token was rejected (401)"), "{with}");
    assert!(with.contains("Invalid credentials"), "{with}");
    assert!(with.contains("Settings → Tokens & updates"), "{with}");
    assert!(!with.contains("gated repo"), "{with}");

    let without = hf::hub_refusal(StatusCode::UNAUTHORIZED, body, repo, false).expect("a sentence");
    assert!(without.contains("gated repo"), "{without}");
    assert!(without.contains(repo), "{without}");

    // A HEAD has no body to quote, and the sentence still stands on its own.
    let head = hf::hub_refusal(StatusCode::UNAUTHORIZED, "", repo, true).expect("a sentence");
    assert!(
        head.ends_with("check the Hugging Face token under Settings → Tokens & updates"),
        "{head}"
    );
    // A 403 the hub calls restricted is a licence problem with or without a
    // token — that one really is about accepting terms.
    let gated_403 = hf::hub_refusal(
        StatusCode::FORBIDDEN,
        r#"{"error":"Access to model … is restricted."}"#,
        repo,
        true,
    )
    .expect("a sentence");
    assert!(gated_403.contains("gated repo"), "{gated_403}");
    assert!(hf::hub_refusal(StatusCode::NOT_FOUND, "", repo, true).is_none());
}

#[tokio::test]
async fn download_flow_and_update_check() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());

    let body = b"GGUF\x00fake model bytes".to_vec();
    Mock::given(method("GET"))
        .and(path("/o/r/resolve/main/m.gguf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"etag-v1\"")
                .set_body_bytes(body.clone()),
        )
        .mount(&hub)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/o/r/resolve/main/m.gguf"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"etag-v2\""))
        .mount(&hub)
        .await;

    // App state with a real models dir.
    let state = AppState::init_for_tests().await.unwrap();
    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = models_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    // Upsert is idempotent per (repo, file, target) and re-queues.
    let id = store::upsert_hf_model(&state.db, "o/r", "m.gguf", "o/r/m.gguf", "chat")
        .await
        .unwrap();
    let again = store::upsert_hf_model(&state.db, "o/r", "m.gguf", "o/r/m.gguf", "chat")
        .await
        .unwrap();
    assert_eq!(id, again);

    // Download and wait for the terminal status.
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    let job_id = hf_download::start(&state, &row).await.unwrap().id();
    let mut status = String::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        status = store::get_hf_model(&state.db, id)
            .await
            .unwrap()
            .unwrap()
            .status;
        if status == "done" || status == "failed" {
            break;
        }
    }
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(status, "done", "error: {:?}", row.error);
    assert_eq!(row.etag.as_deref(), Some("etag-v1"));
    assert_eq!(row.size_bytes, Some(body.len() as i64));
    let dest = models_dir.path().join("o/r/m.gguf");
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!dest.with_file_name("m.gguf.part").exists());

    // The transfer left a job row behind: terminal, byte-accurate, and no
    // longer live. `hf_downloads` keeps its shape, with the live columns null
    // now that nothing is running.
    let job = store::get_job(&state.db, job_id).await.unwrap().unwrap();
    assert_eq!(job.kind, JobKind::HfDownload.as_str());
    assert_eq!(job.key.as_deref(), Some(format!("hf:{id}").as_str()));
    assert_eq!(job.status, "done");
    let jobs_seen = jobs::list(&state, Some(JobKind::HfDownload), false, 0)
        .await
        .unwrap();
    assert_eq!(jobs_seen.len(), 1);
    assert_eq!(jobs_seen[0].done, body.len() as u64);
    assert_eq!(jobs_seen[0].percent, Some(100));
    assert!(state.jobs.live().is_empty());
    let view = lmgw_core::ops::hf_downloads(&state).await.unwrap();
    let entry = &view["downloads"][0];
    assert_eq!(entry["status"], "done");
    assert!(entry["received_bytes"].is_null());
    assert!(entry["job_id"].is_null());
    assert_eq!(view["active"], 0);

    // Cancelling something that is not transferring is an error, not a quiet
    // no-op — the same path `lmgw__hf_set action=cancel` takes.
    let err = lmgw_core::ops::hf_set(&state, "cancel", Some(id), "chat")
        .await
        .unwrap_err();
    assert!(err.contains("no download is running"), "{err}");

    // Remote etag differs → flagged update_available.
    assert!(hf::check_update(&state, &row).await.unwrap());
    let row = store::get_hf_model(&state.db, id).await.unwrap().unwrap();
    assert_eq!(row.status, "update_available");

    // Listing and delete round-trip.
    assert_eq!(store::list_hf_models(&state.db).await.unwrap().len(), 1);
    store::delete_hf_model(&state.db, id).await.unwrap();
    assert!(store::list_hf_models(&state.db).await.unwrap().is_empty());

    // -------------------------------------------------------------------
    // The image target (image-generation design §7.1)
    // -------------------------------------------------------------------

    // A pipeline component: a `.safetensors`, nested inside its repo, in a
    // repo whose listing also carries files no class downloads.
    const VAE_REPO: &str = "Comfy-Org/z_image_turbo";
    const VAE_FILE: &str = "split_files/vae/ae.safetensors";
    let vae_bytes = b"safetensors-ish".to_vec();
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{VAE_REPO}/tree/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "type": "file", "path": VAE_FILE, "size": 335_304_388u64 },
            { "type": "file", "path": "split_files/diffusion_models/z_image_turbo_bf16.safetensors",
              "size": 12_309_866_400u64 },
            { "type": "file", "path": "split_files/text_encoders/qwen_3_4b.safetensors",
              "size": 8_044_982_048u64 },
            { "type": "file", "path": "README.md", "size": 10 },
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{VAE_REPO}/resolve/main/{VAE_FILE}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"vae-v1\"")
                .set_body_bytes(vae_bytes.clone()),
        )
        .mount(&hub)
        .await;

    // `hf_repo target=image` lists the safetensors with the pipeline roles;
    // `target=chat` sees nothing in the same repo at all.
    let image_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = image_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let listed = lmgw_core::ops::hf_repo(&state, VAE_REPO, None, "image")
        .await
        .unwrap();
    assert_eq!(listed["target"], "image");
    let roles: Vec<(&str, &str)> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["file"].as_str().unwrap(), f["role"].as_str().unwrap()))
        .collect();
    assert_eq!(
        roles,
        vec![
            (VAE_FILE, "vae"),
            (
                "split_files/diffusion_models/z_image_turbo_bf16.safetensors",
                "diffusion"
            ),
            (
                "split_files/text_encoders/qwen_3_4b.safetensors",
                "text_encoder"
            ),
        ],
        "the README is not a weights file and the roles are the pipeline's"
    );
    let chat_err = lmgw_core::ops::hf_repo(&state, VAE_REPO, None, "chat")
        .await
        .unwrap_err();
    assert!(
        chat_err.contains("no .gguf files"),
        "the chat target is still GGUF-only: {chat_err}"
    );

    // …and `hf_add target=image` actually downloads one.
    let added = lmgw_core::ops::hf_add(&state, VAE_REPO, Some(VAE_FILE), None, "image", true)
        .await
        .unwrap();
    assert_eq!(added["target"], "image");
    assert_eq!(added["files_queued"], 1);
    assert_eq!(
        added["companions"],
        json!([]),
        "an image pipeline's components are in other repos"
    );
    let vae_id = added["downloads"][0]["id"].as_i64().unwrap();
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let r = store::get_hf_model(&state.db, vae_id)
            .await
            .unwrap()
            .unwrap();
        if r.status == "done" || r.status == "failed" {
            break;
        }
    }
    let row = store::get_hf_model(&state.db, vae_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "done", "error: {:?}", row.error);
    assert_eq!(row.target, "image");
    assert_eq!(row.dest_path, format!("{VAE_REPO}/{VAE_FILE}"));
    let dest = image_dir.path().join(&row.dest_path);
    assert_eq!(std::fs::read(&dest).unwrap(), vae_bytes);

    // The chat target refuses the same file by name, rather than writing a
    // `.safetensors` into the llama-server tree where nothing can load it.
    let refused = lmgw_core::ops::hf_add(&state, VAE_REPO, Some(VAE_FILE), None, "chat", true)
        .await
        .unwrap_err();
    assert!(
        refused.contains("is not a file the chat class loads") && refused.contains(".gguf"),
        "{refused}"
    );

    // `gguf_files target=image` lists the kinds the class loads, with the
    // rows that use each one — a shared VAE legitimately appears in several.
    store::insert_image_model(
        &state.db,
        &NewImageModel {
            model_id: "z".into(),
            files: serde_json::from_value(json!({
                "diffusion_model": "leejet/Z/d.gguf",
                "vae": format!("{VAE_REPO}/{VAE_FILE}"),
            }))
            .unwrap(),
            args: Default::default(),
            modes: vec!["img_gen".into()],
            edit: false,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 0,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let files = lmgw_core::modelinfo::gguf_files(&state, None, Some("image"))
        .await
        .unwrap();
    assert_eq!(files["target"], "image");
    let entry = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == format!("{VAE_REPO}/{VAE_FILE}"))
        .expect("the safetensors is listed");
    assert_eq!(entry["role_guess"], "vae");
    assert_eq!(entry["used_by"], json!(["z"]));
    assert_eq!(entry["size_bytes"], vae_bytes.len() as u64);
    assert!(
        files["note"].as_str().unwrap().contains(".safetensors"),
        "the note says the image target is not GGUF-only: {}",
        files["note"]
    );
    // The chat listing, over its own dir, is untouched by any of this.
    let chat = lmgw_core::modelinfo::gguf_files(&state, None, Some("chat"))
        .await
        .unwrap();
    assert_eq!(chat["kinds"], json!([".gguf"]));

    // -------------------------------------------------------------------
    // A gated repo (§2.7, §12.1)
    // -------------------------------------------------------------------

    const GATED: &str = "black-forest-labs/FLUX.1-schnell";
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{GATED}/tree/main")))
        .respond_with(ResponseTemplate::new(401).set_body_string("Invalid credentials"))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{GATED}/resolve/main/ae.safetensors")))
        .respond_with(ResponseTemplate::new(401).set_body_string("Invalid credentials"))
        .mount(&hub)
        .await;

    // The listing fails before a download is ever queued, and says why.
    let err = lmgw_core::ops::hf_repo(&state, GATED, None, "image")
        .await
        .unwrap_err();
    assert!(err.contains("gated repo"), "{err}");
    assert!(err.contains("accept the licence on the hub"), "{err}");
    assert!(err.contains(GATED), "{err}");

    // And a row that reaches the transfer anyway carries the same sentence as
    // its `error`, which is what the Downloads page and lmgw__hf_downloads
    // show — not "GET …: 401 Unauthorized".
    let gated_id = store::upsert_hf_model(
        &state.db,
        GATED,
        "ae.safetensors",
        &format!("{GATED}/ae.safetensors"),
        "image",
    )
    .await
    .unwrap();
    let gated_row = store::get_hf_model(&state.db, gated_id)
        .await
        .unwrap()
        .unwrap();
    let _ = hf_download::start(&state, &gated_row).await.unwrap();
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let r = store::get_hf_model(&state.db, gated_id)
            .await
            .unwrap()
            .unwrap();
        if r.status == "failed" || r.status == "done" {
            break;
        }
    }
    let r = store::get_hf_model(&state.db, gated_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.status, "failed");
    let recorded = r.error.unwrap_or_default();
    assert!(recorded.contains("gated repo"), "{recorded}");
    assert!(recorded.contains(GATED), "{recorded}");
    assert!(!recorded.contains("401"), "no bare status: {recorded}");
}
