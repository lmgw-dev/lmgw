//! "Get it" and its engine image (device-ledger design, ruling 13, WP9):
//! the get waits for the image its class's models run in before it creates
//! the row, so the create reads the engine's flags from it; a download of
//! it that fails, or an image no registry gives, does not fail the get, and
//! the create's warning says the download's own sentence.
//!
//! Podman's API is a [`ScriptedPull`] of the compat-mode answer recorded
//! from Podman 5.8.9 (`tests/fixtures/pull/compat-stream.jsonl`), held
//! after its first lines until the test lets go; the registry is a mock
//! serving that image's index and amd64 manifest. The fake [`Podman`]
//! keeps the image's store.

use lmgw_api_types::model_get::{ModelGetStage, ModelGetState, ModelGetStatus};
use lmgw_api_types::EngineFetch;
use tokio::sync::watch;

use super::*;
use crate::support::scripted_pull::ScriptedPull;

/// A fully qualified reference on a registry that is clearly not real.
const ENGINE: &str = "registry.example/lmgw-test/engine:cuda";

async fn status(gw: &Gw, get_id: i64) -> ModelGetStatus {
    let (code, live) = op(gw, "model_get_status", json!({ "get_id": get_id })).await;
    assert_eq!(code, 200, "{live}");
    common::round_trips::<ModelGetStatus>("POST /api/op/model_get_status", &live)
}

/// The registry, serving the recorded image's index and amd64 manifest.
async fn registry(state: &SharedState) -> MockServer {
    let reg = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/lmgw-test/engine/manifests/cuda"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../fixtures/pull/index.json"),
            "application/vnd.oci.image.index.v1+json",
        ))
        .mount(&reg)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/lmgw-test/engine/manifests/sha256:98d673f18a1aac43da744209873cb79323e11706f909251bcfb131828b95559d",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../fixtures/pull/manifest-amd64.json"),
            "application/vnd.oci.image.manifest.v1+json",
        ))
        .mount(&reg)
        .await;
    state.builds.set_registry_client(
        lmgw_core::backends::oci::RegistryClient::new()
            .unwrap()
            .with_endpoint("registry.example", reg.uri()),
    );
    reg
}

/// The chat class runs in `image`, which is not on this machine, and
/// Podman's API pulls it as the recording says, held after its first
/// lines; the image is in the store once the answer ends. `refuse`: the API
/// refuses every pull with this message instead. The sender lets the held
/// part go.
async fn engine_missing(w: &World, image: &str, refuse: Option<&str>) -> watch::Sender<bool> {
    let lines: Vec<String> = include_str!("../../fixtures/pull/compat-stream.jsonl")
        .lines()
        .map(str::to_string)
        .collect();
    let split = lines
        .iter()
        .position(|l| l.contains("\"Download complete\"") && l.contains("f7bdfd728ac2"))
        .expect("the recording has the large layer's completion");
    let podman = w.podman.clone();
    let (api, release) = ScriptedPull::new(
        lines[..split].to_vec(),
        lines[split..].to_vec(),
        refuse,
        Arc::new(move || {
            if let Some(e) = podman.engine.lock().unwrap().as_mut() {
                e.here = true;
            }
        }),
    );
    w.state.builds.set_pull_api(api);
    *w.podman.engine.lock().unwrap() = Some(Engine::default());
    let mut settings = w.state.snapshot().settings.clone();
    settings.router.image = image.into();
    store::save_settings(&w.state.db, &settings).await.unwrap();
    w.state.reload_snapshot().await.unwrap();
    release
}

fn row_of(state: &SharedState, model_id: &str) -> Option<i64> {
    state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == model_id)
        .map(|m| m.id)
}

/// The engine image arrives after the files: the get says it waits for
/// the engine and creates nothing meanwhile; once the image is here the
/// row is created, its flags read from the image, and no warning says
/// they were not checked.
#[tokio::test]
async fn a_get_whose_engine_comes_after_its_files_creates_the_row_once_it_is_here() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let _reg = registry(&w.state).await;
    let release = engine_missing(&w, ENGINE, None).await;

    let started = get(&w.gw, json!({"repo": REPO, "file": WEIGHTS})).await;
    let engine = started
        .engine
        .clone()
        .expect("the op begins the image's pull");
    assert_eq!(engine.image, ENGINE);
    assert_eq!(engine.state, EngineFetch::Downloading);
    let get_id = started.get_id.unwrap();

    let gw = w.gw.clone();
    patience::until_async("the get to wait for the engine", || {
        let gw = gw.clone();
        async move { status(&gw, get_id).await.stage == Some(ModelGetStage::WaitingForEngine) }
    })
    .await;
    let s = status(&w.gw, get_id).await;
    assert_eq!(s.state, ModelGetState::Running);
    assert!(
        s.downloads.iter().all(|d| d.status == "done"),
        "the files are here: {:?}",
        s.downloads
    );
    // Nothing created, nothing read from an image that is not here.
    assert_eq!(row_of(&w.state, "demo-chat"), None);
    assert_eq!(
        w.podman
            .engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .help_missing,
        0
    );
    let job = w
        .state
        .jobs
        .live_one(started.job_id.unwrap())
        .expect("the get's job");
    assert_eq!(job.stage, "waiting for the engine");

    release.send(true).unwrap();
    let (row, done, error) = ended(&w.state, get_id).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.expect("a result");
    assert!(done.created);
    assert_eq!(row_of(&w.state, "demo-chat"), Some(done.row_id));
    assert!(
        !done
            .warnings
            .iter()
            .any(|w| w.contains("--help") || w.contains("not validated")),
        "the flags were checked: {:?}",
        done.warnings
    );
    let e = w.podman.engine.lock().unwrap();
    let e = e.as_ref().unwrap();
    assert!(e.here);
    assert_eq!(e.help_missing, 0, "never read before the image was here");
    assert!(e.help_here > 0, "the create read the image's flags");
    drop(release);
    assert!(w.podman.starts().is_empty(), "{:?}", w.podman.starts());
}

/// The engine's download fails: the get does not fail, since the files
/// are here; the row is created, and the warning that its flags were not
/// checked says the download's own sentence, not podman's "image not
/// known".
#[tokio::test]
async fn a_get_whose_engine_download_fails_creates_the_row_with_the_downloads_sentence() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let _reg = registry(&w.state).await;
    let _release = engine_missing(&w, ENGINE, Some("manifest unknown: manifest unknown")).await;

    let started = get(&w.gw, json!({"repo": REPO, "file": WEIGHTS})).await;
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.expect("a result");
    assert!(done.created);
    assert_eq!(row_of(&w.state, "demo-chat"), Some(done.row_id));
    let said: Vec<&String> = done
        .warnings
        .iter()
        .filter(|w| w.contains("--help"))
        .collect();
    assert_eq!(
        said,
        [&format!(
            "could not read llama-server --help from image '{ENGINE}': it is not on this \
             machine, and its download failed: podman pull {ENGINE} failed: manifest unknown: \
             manifest unknown — the extra args and flag values were not validated"
        )],
        "{:?}",
        done.warnings
    );
    assert!(
        !done.warnings.iter().any(|w| w.contains("image not known")),
        "{:?}",
        done.warnings
    );
}

/// An image no registry gives (a build on this machine): nothing is
/// downloaded or waited for, and the row's warning says why the image
/// cannot be fetched.
#[tokio::test]
async fn a_get_whose_engine_no_registry_gives_creates_the_row_and_says_so() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let local = "localhost/lmgw-llama-server:dev";
    let _release = engine_missing(&w, local, None).await;

    let started = get(&w.gw, json!({"repo": REPO, "file": WEIGHTS})).await;
    assert_eq!(
        started.engine.as_ref().map(|e| e.state),
        Some(EngineFetch::Refused)
    );
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.expect("a result");
    let said: Vec<&String> = done
        .warnings
        .iter()
        .filter(|w| w.contains("--help"))
        .collect();
    assert_eq!(
        said,
        [&format!(
            "could not read llama-server --help from image '{local}': it is not on this \
             machine, and it cannot be downloaded: it was built on this machine, not pulled — \
             run its build again on the Engines page — the extra args and flag values were not \
             validated"
        )],
        "{:?}",
        done.warnings
    );
    assert!(w
        .state
        .jobs
        .live_of_kind(lmgw_core::jobs::JobKind::ImagePull)
        .is_empty());
}
