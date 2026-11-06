//! The engine download against the real Podman (machine-check design §5):
//! a start's pull through this instance's private `podman system service`,
//! its compat-mode byte counts, and the registry manifest's total — what the
//! scripted tests in `vram_admission::engine_download` stand in for.
//!
//! Gated on `LMGW_LIVE_PULL=<reference>`, the image to pull, because it
//! needs the network and writes to the user's image store, which a running
//! lmgw shares. Name an image nothing uses, ideally pinned by digest; the
//! test refuses one that is already on the machine (it never re-pulls what
//! something may start from), and removes exactly the image it pulled when
//! it is done. Run it as
//!
//! LMGW_LIVE_PULL=docker.io/library/busybox@sha256:… cargo nextest run
//! --workspace -E 'test(pull_live)' --run-ignored only --no-capture

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lmgw_core::backends::pull::{self, Bytes, Presence, ServicePull};
use lmgw_core::runtime::registry::{Registry, TokioRunner};
use lmgw_core::state::AppState;

#[tokio::test]
#[ignore = "needs the network and the user's podman; set LMGW_LIVE_PULL=<reference>"]
async fn a_pull_through_the_private_service_reports_bytes() {
    let Ok(image) = std::env::var("LMGW_LIVE_PULL") else {
        eprintln!("SKIP: set LMGW_LIVE_PULL to an image reference nothing uses");
        return;
    };
    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(TokioRunner),
        reqwest::Client::new(),
    )));
    match pull::presence(&state, &image).await {
        Presence::Missing => {}
        other => panic!("{image} must not be on this machine before the test ({other:?})"),
    }
    state
        .builds
        .set_pull_api(Arc::new(ServicePull::for_instance("live-pull-test")));

    let began = Instant::now();
    let seen: Arc<Mutex<Vec<(u128, Bytes)>>> = Arc::default();
    let log = seen.clone();
    let pulled = pull::pull_and_wait(&state, &image, move |b| {
        log.lock().unwrap().push((began.elapsed().as_millis(), b));
    })
    .await;
    let seen = seen.lock().unwrap().clone();
    for (ms, b) in &seen {
        eprintln!("{ms:>6} ms  {} of {:?}", b.done, b.total);
    }
    let row = lmgw_core::store::latest_job_by_key(
        &state.db,
        "image_pull",
        &format!("image:{}", pull::start_reference(&image).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    eprintln!(
        "job: {} {:?}\nprogress: {}",
        row.status, row.error, row.progress
    );
    if let Some(r) = &row.result {
        eprintln!("result: {r}");
    }
    // Exactly the image this test pulled, whatever happened above.
    let rmi = std::process::Command::new("podman")
        .args(["rmi", image.as_str()])
        .output()
        .unwrap();
    eprintln!(
        "podman rmi {image}: {}{}",
        String::from_utf8_lossy(&rmi.stdout),
        String::from_utf8_lossy(&rmi.stderr)
    );
    pulled.unwrap();
    assert!(!seen.is_empty(), "the wait heard the bytes");
    // The job's last word, which the wait's sampling may have missed.
    let progress: serde_json::Value = serde_json::from_str(&row.progress).unwrap();
    assert!(
        progress["total"].is_u64(),
        "the manifest gave a total: {progress}"
    );
    assert_eq!(progress["done"], progress["total"], "{progress}");
}
