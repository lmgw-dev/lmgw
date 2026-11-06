//! What the UI's download line reads (machine-check design §5, WP11):
//! a model download also fetches the image its class's models run in when
//! that image is not on this machine (D16) — through the `image_pull` job a
//! start would use, so the first request joins it — and starts nothing; and
//! a chat turn whose model waits for that download says so in its `state`
//! frames, with the bytes, before it says `loading` again.
//!
//! Podman's API is the scripted recording of [`super::engine_download`],
//! the registry its mock; the hub is a mock behind `HF_ENDPOINT`, and the
//! file download itself is a job that ends at once.

use lmgw_api_types::EngineFetch;
use lmgw_core::erased::Erased;
use lmgw_core::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome};

use super::engine_download::{engines, image_pulls, registry, scripted, ENGINE, TOTAL};
use super::*;

/// The model file's download: done at once, since the file is not what is
/// tested here.
struct Fetched;

impl JobExecutor for Fetched {
    fn kind(&self) -> JobKind {
        JobKind::HfDownload
    }

    fn run<'a>(&'a self, _ctx: JobCtx, _input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async { Ok(JobOutcome::Done(json!({}))) })
    }
}

/// A hub that lists one GGUF in `p/chat`, behind `HF_ENDPOINT` while the
/// guard lives.
async fn hub() -> (MockServer, common::EnvGuard) {
    let env = common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    Mock::given(method("GET"))
        .and(path("/api/models/p/chat/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"type": "file", "path": "chat-Q4_K_M.gguf", "size": 4096},
            {"type": "file", "path": "chat-Q8_0.gguf", "size": 8192},
        ])))
        .mount(&hub)
        .await;
    (hub, env)
}

async fn add(f: &Fixture) -> lmgw_api_types::HfAddResult {
    add_file(f, "chat-Q4_K_M.gguf").await
}

async fn add_file(f: &Fixture, file: &str) -> lmgw_api_types::HfAddResult {
    lmgw_core::ops::hf_add(&f.state, "p/chat", Some(file), None, "chat", false)
        .await
        .unwrap()
}

/// The chat class's image is missing: the model download begins its pull
/// and says which job it is, nothing starts and nothing waits for the card,
/// and the first request joins that job instead of pulling again.
#[tokio::test]
async fn a_model_download_fetches_the_missing_image_too_and_starts_nothing() {
    let (_hub, _env) = hub().await;
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.state.jobs.register(Arc::new(Fetched));
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;

    let added = add(&f).await;
    let engine = added.engine.expect("the image is fetched with the model");
    assert_eq!(engine.image, ENGINE);
    assert_eq!(engine.state, EngineFetch::Downloading);
    assert_eq!(engine.note, None);
    let job = engine.job_id.expect("the pull's job");
    let pulls = image_pulls(&f);
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].id, job);
    assert_eq!(
        pulls[0].key.as_deref(),
        Some(format!("image:{ENGINE}").as_str())
    );
    assert!(f.runs().is_empty(), "nothing started");
    assert_eq!(vram_status(&f.gateway).await["downloading"], json!([]));
    assert!(
        !added.message.contains(ENGINE),
        "the message stays the files' own: {}",
        added.message
    );

    // A first request now waits for that job, not a second download.
    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { chat(&gw).await.status() });
    until_vram(&f, "the request waiting for the same download", |v| {
        v["downloading"][0]["model"] == "chat-model"
    })
    .await;
    assert_eq!(image_pulls(&f).len(), 1, "one job");
    release.send(true).unwrap();
    assert_eq!(req.await.unwrap(), 200);
    assert_eq!(api.calls(), vec![ENGINE.to_string()], "one pull");
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// An image that is here, or one built on this machine, begins no pull:
/// the first says nothing, the second says why it cannot.
#[tokio::test]
async fn an_image_here_or_a_local_build_begins_no_pull() {
    let (_hub, _env) = hub().await;
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.state.jobs.register(Arc::new(Fetched));
    let (api, _release) = scripted(&f, None);
    engines(&f, "ghcr.io/ggml-org/llama.cpp:server-cuda", ENGINE).await;
    assert_eq!(add(&f).await.engine, None, "the chat image is here");

    let local = "localhost/lmgw-test/engine:cuda";
    engines(&f, local, ENGINE).await;
    f.world().missing_images.insert(local.into());
    let engine = add(&f).await.engine.expect("said, though not pulled");
    assert_eq!(engine.image, local);
    assert_eq!(engine.state, EngineFetch::Refused);
    assert_eq!(engine.job_id, None);
    let note = engine.note.unwrap_or_default();
    assert!(
        note.contains("built on this machine") && note.contains("Engines page"),
        "{note}"
    );
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
    assert!(f.runs().is_empty());
}

/// Two model downloads of one class at once begin one pull of its image:
/// both answers name the same job.
#[tokio::test]
async fn two_model_downloads_at_once_begin_one_pull() {
    let (_hub, _env) = hub().await;
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.state.jobs.register(Arc::new(Fetched));
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;

    let (a, b) = tokio::join!(
        add_file(&f, "chat-Q4_K_M.gguf"),
        add_file(&f, "chat-Q8_0.gguf")
    );
    let (a, b) = (a.engine.unwrap(), b.engine.unwrap());
    assert!(a.job_id.is_some(), "{a:?}");
    assert_eq!(a.job_id, b.job_id, "one job: {a:?} {b:?}");
    let pulls = image_pulls(&f);
    assert_eq!(pulls.len(), 1, "one pull");
    assert_eq!(Some(pulls[0].id), a.job_id);
    assert!(api.calls().len() <= 1, "{:?}", api.calls());
    assert!(f.runs().is_empty());
}

/// Podman cannot say whether the class's image is here (its storage
/// locked): nothing is fetched, as a start leaves such an image alone, and
/// the answer says so and why rather than reading as "the image is here".
#[tokio::test]
async fn an_image_podman_cannot_say_about_is_said_and_not_fetched() {
    let (_hub, _env) = hub().await;
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.state.jobs.register(Arc::new(Fetched));
    let (api, _release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;
    f.world().unknown_images.insert(ENGINE.into());

    let engine = add(&f).await.engine.expect("said");
    assert_eq!(engine.image, ENGINE);
    assert_eq!(engine.state, EngineFetch::NotKnown);
    assert_eq!(engine.job_id, None);
    let note = engine.note.unwrap_or_default();
    assert!(
        note.contains("is not known")
            && note.contains("database is locked")
            && note.contains("first start looks again"),
        "{note}"
    );
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
}

/// An image recipe whose every component is on disk already still fetches
/// the image class's image when it is missing (D37: getting the model is
/// what was asked), and starts nothing.
#[tokio::test]
async fn a_recipe_with_every_file_here_still_fetches_the_missing_image() {
    let (_hub, _env) = hub().await;
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, _release) = scripted(&f, None);
    let dir = tempfile::tempdir().unwrap();
    let recipe = lmgw_core::image_recipes::find("z-image-turbo").unwrap();
    for c in recipe.components {
        let p = dir.path().join(c.dest_rel_path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not really weights").unwrap();
    }
    let mut s = f.state.snapshot().settings.clone();
    s.image.models_dir = dir.path().display().to_string();
    s.image.image = ENGINE.into();
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.world().missing_images.insert(ENGINE.into());

    let added = lmgw_core::ops::image_recipe_add(&f.state, "z-image-turbo", None)
        .await
        .unwrap();
    assert_eq!(added.files_queued, 0);
    let engine = added.engine.expect("the image is fetched with the recipe");
    assert_eq!(engine.image, ENGINE);
    assert_eq!(engine.state, EngineFetch::Downloading);
    let pulls = image_pulls(&f);
    assert_eq!(pulls.len(), 1);
    assert_eq!(Some(pulls[0].id), engine.job_id);
    assert!(
        !added.message.contains(ENGINE),
        "the message stays the files' own: {}",
        added.message
    );
    assert!(f.runs().is_empty(), "nothing started");
}

/// The SSE frames of a turn's body, `(event, data)`.
fn frames(body: &str) -> Vec<(String, Value)> {
    crate::chat_actions::sse_events(body)
}

/// A chat turn whose model waits for its download says `downloading` with
/// the bytes, then `loading` once the download is done, then `ready`.
#[tokio::test]
async fn a_chat_turn_says_its_download_before_it_loads() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;
    let tid = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads", f.gateway))
        .json(&json!({"model_alias": "chat-model"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    let gateway = f.gateway.clone();
    let turn = tokio::spawn(async move {
        gateway
            .client()
            .post(format!("{gateway}/chat/api/threads/{tid}/send"))
            .json(&json!({"content": "hi"}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    until_vram(&f, "the turn's start downloading", |v| {
        v["downloading"][0]["phase"]["total_bytes"] == TOTAL
            && v["downloading"][0]["phase"]["done_bytes"].as_u64() > Some(0)
    })
    .await;
    // The turn reads its bytes at the pull's own pace; give it one look.
    tokio::time::sleep(Duration::from_millis(600)).await;
    release.send(true).unwrap();
    let body = turn.await.unwrap();

    let states: Vec<Value> = frames(&body)
        .into_iter()
        .filter(|(e, _)| e == "state")
        .map(|(_, d)| d)
        .collect();
    let said: Vec<&str> = states.iter().filter_map(|s| s["state"].as_str()).collect();
    let first_dl = said.iter().position(|s| *s == "downloading");
    let last_dl = said.iter().rposition(|s| *s == "downloading");
    let (Some(first_dl), Some(last_dl)) = (first_dl, last_dl) else {
        panic!("no downloading frame: {said:?}\n{body}");
    };
    assert_eq!(said[0], "loading", "{said:?}");
    assert!(first_dl > 0, "{said:?}");
    assert_eq!(
        &said[last_dl + 1..],
        ["loading", "ready"],
        "loading again after the download, then ready: {said:?}"
    );
    let moving = states
        .iter()
        .filter(|s| s["state"] == "downloading")
        .find(|s| s["total_bytes"] == TOTAL && s["done_bytes"].as_u64() > Some(0))
        .unwrap_or_else(|| panic!("a frame with bytes: {states:?}"));
    assert_eq!(moving["stage"], "chat");
    assert_eq!(moving["alias"], "chat-model");
    assert_eq!(moving["image"], ENGINE);
    assert!(moving.get("size_note").is_none(), "{moving}");
    // Every frame reads as the clients' type.
    for s in &states {
        let typed: lmgw_api_types::chat_voice::ModelState =
            serde_json::from_value(s.clone()).unwrap();
        assert_eq!(typed.state, s["state"].as_str().unwrap());
    }
    let ready = states.last().unwrap();
    assert!(
        ready["ms"].as_u64().is_some(),
        "the load's own time, the download not in it: {ready}"
    );
}
