//! The download before a model's first start (machine-check design §5,
//! D15): a start whose engine image is not on the machine pulls it through
//! the `image_pull` job before admission — nothing evicted, nothing reserved,
//! no place in the queue, the queue's budget after it — and the start then
//! runs with `--pull=never`.
//!
//! Podman's API is a [`ScriptedPull`] that streams the compat-mode answer
//! recorded from Podman 5.8.9 (`tests/fixtures/pull/compat-stream.jsonl`)
//! and holds the rest of it until the test lets go; the registry is a mock
//! serving that image's index and amd64 manifest, so the total is the
//! manifest's.

use lmgw_core::backends::oci::RegistryClient;
use lmgw_core::jobs::{self, JobKind};
use lmgw_core::vram::stage_watch::{follow, WaitPhase};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::support::scripted_pull::ScriptedPull;

/// A fully qualified reference on a registry that is clearly not real.
pub(super) const ENGINE: &str = "registry.example/lmgw-test/engine:cuda";

/// What the recorded stream adds up to: the manifest's config and layers,
/// less the 32-byte empty layer Podman reported it already had.
pub(super) const TOTAL: u64 = 308_481_184 - 32;

fn recorded() -> Vec<String> {
    include_str!("../../fixtures/pull/compat-stream.jsonl")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Podman's API on `f` as a [`ScriptedPull`] of the recorded stream, split
/// after its first `Downloading` lines; the sender lets the rest go, and the
/// image is in the store once the answer ends.
pub(super) fn scripted(
    f: &Fixture,
    refuse: Option<&str>,
) -> (Arc<ScriptedPull>, watch::Sender<bool>) {
    let lines = recorded();
    let split = lines
        .iter()
        .position(|l| l.contains("\"Download complete\"") && l.contains("f7bdfd728ac2"))
        .expect("the recording has the 79 MB layer's completion");
    let world = f.world.clone();
    let (api, tx) = ScriptedPull::new(
        lines[..split].to_vec(),
        lines[split..].to_vec(),
        refuse,
        Arc::new(move || world.lock().unwrap().missing_images.clear()),
    );
    f.state.builds.set_pull_api(api.clone());
    (api, tx)
}

/// The registry, serving the recorded image's index and amd64 manifest.
pub(super) async fn registry(f: &Fixture) -> MockServer {
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
    f.state.builds.set_registry_client(
        RegistryClient::new()
            .unwrap()
            .with_endpoint("registry.example", reg.uri()),
    );
    reg
}

/// The class images: `chat` and `aux` as given, and [`ENGINE`] missing.
pub(super) async fn engines(f: &Fixture, chat: &str, aux: &str) {
    let mut s = f.state.snapshot().settings.clone();
    s.router.image = chat.into();
    s.aux_router.image = aux.into();
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.world().missing_images.insert(ENGINE.into());
}

pub(super) fn image_pulls(f: &Fixture) -> Vec<lmgw_core::jobs::JobView> {
    f.state.jobs.live_of_kind(JobKind::ImagePull)
}

/// The whole of §5 on one start: the embedder's engine is missing while the
/// idle chat model fills the card. The download runs before admission —
/// the chat model stays, nothing is reserved, the request holds no place in
/// the queue and asks for no gate — and outlasts the queue's budget; once it
/// is done the start is admitted with a fresh budget, evicts, and runs with
/// `--pull=never`.
#[tokio::test]
async fn a_missing_engine_downloads_before_admission_and_holds_nothing_meanwhile() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    engines(&f, "ghcr.io/ggml-org/llama.cpp:server-cuda", ENGINE).await;
    set_queue_timeout(&f, 1).await;
    let (waits, asks) = (f.state.vram.waits_begun(), f.state.vram.gate_asks());

    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { embed(&gw).await.status() });
    let v = until_vram(&f, "the download's bytes in the vram view", |v| {
        v["downloading"][0]["phase"]["total_bytes"] == TOTAL
            && v["downloading"][0]["phase"]["done_bytes"].as_u64() > Some(0)
    })
    .await;
    let d = &v["downloading"][0];
    assert_eq!(d["model"], "embed-model");
    assert_eq!(d["container"], "aux");
    assert_eq!(d["position"], 0, "no place in the queue");
    assert_eq!(d["phase"]["kind"], "downloading");
    assert_eq!(d["phase"]["image"], ENGINE);
    assert!(d["stage"].as_str().unwrap().contains("downloading"), "{d}");
    assert_eq!(v["queue"], json!([]));
    assert_eq!(f.state.vram.waits_begun(), waits, "nothing queued");
    assert_eq!(f.state.vram.gate_asks(), asks, "no gate asked for");
    assert!(f.stops().is_empty(), "nothing evicted");
    assert!(f.state.vram.pending_starts().is_empty(), "nothing reserved");
    assert!(f.world().loaded.contains("chat-model"));

    // The job behind it, in bytes.
    let pulls = image_pulls(&f);
    assert_eq!(pulls.len(), 1);
    assert_eq!(
        pulls[0].key.as_deref(),
        Some(format!("image:{ENGINE}").as_str())
    );
    assert_eq!(pulls[0].total, Some(TOTAL));
    assert!(pulls[0].done > 0 && pulls[0].done < TOTAL);

    // Longer than the queue's budget: it has not started yet.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    release.send(true).unwrap();
    assert_eq!(
        req.await.unwrap(),
        200,
        "the budget began after the download"
    );
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
    let argv = f.world().argv["embed-model"].clone();
    assert!(argv.iter().any(|a| a == "--pull=never"), "{argv:?}");
    assert_eq!(api.calls(), vec![ENGINE.to_string()]);
    assert_eq!(vram_status(&f.gateway).await["downloading"], json!([]));

    let row = store::latest_job_by_key(&f.state.db, "image_pull", &format!("image:{ENGINE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "done", "{:?}", row.error);
    let progress: Value = serde_json::from_str(&row.progress).unwrap();
    assert_eq!(progress["done"], TOTAL);
    assert_eq!(progress["total"], TOTAL);
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    let log: Vec<&str> = result["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(log.contains(&"4f4fb700ef54: Already exists"), "{log:?}");
    assert!(
        !log.iter().any(|l| l.contains("Downloading")),
        "byte counts stay out of the log: {log:?}"
    );
    assert!(
        log.last().unwrap().contains("1 already on this machine"),
        "{log:?}"
    );
}

/// Two models on one missing image wait for one download, side by side.
#[tokio::test]
async fn two_starts_of_one_image_share_one_download() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    engines(&f, ENGINE, ENGINE).await;

    let (a, b) = (f.gateway.clone(), f.gateway.clone());
    let chat_req = tokio::spawn(async move { chat(&a).await.status() });
    let embed_req = tokio::spawn(async move { embed(&b).await.status() });
    // Both waiting, and the one pull's bytes moving for each.
    until_vram(&f, "both starts downloading", |v| {
        v["downloading"].as_array().is_some_and(|d| {
            d.len() == 2
                && d.iter()
                    .all(|w| w["phase"]["done_bytes"].as_u64() > Some(0))
        })
    })
    .await;
    assert_eq!(image_pulls(&f).len(), 1, "one job");
    assert_eq!(api.calls().len(), 1, "one pull");
    release.send(true).unwrap();
    assert_eq!(chat_req.await.unwrap(), 200);
    assert_eq!(embed_req.await.unwrap(), 200);
    assert_eq!(api.calls().len(), 1);
    assert_eq!(f.runs().len(), 2);
}

/// A caller that follows its wait hears the download first, with its bytes,
/// then admission's own phases.
#[tokio::test]
async fn a_followed_start_hears_the_download_first() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let state = f.state.clone();
    let admitted = tokio::spawn(async move {
        follow(tx, lmgw_core::vram::admit(&state, &route, "chat-model"))
            .await
            .map(|h| h.is_some())
    });
    let mut phases = Vec::new();
    // Until the bytes reach the held-back part of the recording.
    loop {
        let p = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a phase")
            .expect("the channel is open");
        let far = matches!(&p, WaitPhase::Downloading { total_bytes: Some(t), done_bytes, .. }
            if *t == TOTAL && *done_bytes > 0);
        phases.push(p);
        if far {
            break;
        }
    }
    release.send(true).unwrap();
    assert!(admitted.await.unwrap().unwrap(), "admitted");
    while let Ok(p) = rx.try_recv() {
        phases.push(p);
    }
    assert_eq!(
        phases[0],
        WaitPhase::Downloading {
            image: ENGINE.into(),
            done_bytes: 0,
            total_bytes: None,
            size_note: None,
        }
    );
    let last_download = phases
        .iter()
        .rposition(|p| matches!(p, WaitPhase::Downloading { .. }))
        .unwrap();
    assert!(
        phases[last_download + 1..]
            .iter()
            .all(|p| !matches!(p, WaitPhase::Downloading { .. })),
        "{phases:?}"
    );
    assert!(
        phases[last_download + 1..].contains(&WaitPhase::Starting),
        "admission after the download: {phases:?}"
    );
}

/// A download that fails answers the request plainly: the model, the
/// image, Podman's reason and where the image is set — no config key, and
/// nothing started or stopped.
#[tokio::test]
async fn a_failed_download_names_the_image_and_where_it_is_set() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, _release) = scripted(&f, Some("manifest unknown: manifest unknown"));
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    for part in [
        "The engine chat model 'chat-model' runs in could not be downloaded, so the model could \
         not start",
        ENGINE,
        "manifest unknown",
        "The engine is chosen in Settings → Runtimes → Chat → Image.",
    ] {
        assert!(msg.contains(part), "{part:?} in {msg}");
    }
    assert!(!msg.contains("router.image"), "no config key: {msg}");
    assert!(f.runs().is_empty() && f.stops().is_empty());
    assert_eq!(vram_status(&f.gateway).await["downloading"], json!([]));
}

/// Cancelling the pull ends the wait with that reason, and drops Podman's
/// answer, which ends the pull on its side.
#[tokio::test]
async fn a_canceled_download_ends_the_wait_and_the_answer() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;

    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { chat(&gw).await });
    until_vram(&f, "the download under way", |v| {
        v["downloading"][0]["phase"]["done_bytes"].as_u64() > Some(0)
    })
    .await;
    let id = image_pulls(&f)[0].id;
    jobs::cancel(&f.state, id).await.unwrap();
    let resp = req.await.unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains(&format!("its download (job {id}) was canceled")),
        "{msg}"
    );
    let mut wait = common::patience::Wait::new("Podman's answer dropped");
    while !api.hung_up() {
        wait.again(None).await;
    }
    assert!(f.runs().is_empty());
}
