//! A download into an audio row's directory, or a delete from it, changes
//! what the row renders (the direct-file pick reads the directory), with no
//! edit to the row. A running container that still mounts the old
//! `server.json` is stopped for apply, as an edit would stop it, and the
//! next request starts it on the fresh render.

use lmgw_core::jobs::hf_download;
use lmgw_core::runtime::Class;

use super::audio_residency::{add_audio_model, speak, start_idle};
use super::*;

/// The row's mounted `server.json`, as its container was started with it.
fn mounted(f: &Fixture, model_id: &str) -> Value {
    let path = lmgw_core::runtime::audio::config_dir(&f.state.data_dir, model_id)
        .join(lmgw_core::runtime::audio::CONFIG_FILE_NAME);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn until_status(f: &Fixture, id: i64, want: &str) {
    for _ in 0..200 {
        let row = store::get_hf_model(&f.state.db, id).await.unwrap().unwrap();
        if row.status == want {
            return;
        }
        assert_ne!(row.status, "failed", "{:?}", row.error);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("download {id} never reached {want}");
}

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The row `asr` over one GGUF in `o/asr`, `weight_id` q4_0, handed the
/// directory; returns that directory.
async fn one_gguf_row(f: &Fixture) -> std::path::PathBuf {
    add_audio_model(f, "asr", GIB, GIB, None).await;
    let dir = f._models_dir.path().join("o/asr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("asr-q4_0.gguf"), b"GGUF q4").unwrap();
    sqlx::query(
        "UPDATE audio_models SET path = 'o/asr', weight_id = 'q4_0' WHERE model_id = 'asr'",
    )
    .execute(&f.state.db)
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    dir
}

/// A row over one GGUF, `weight_id` q4_0: its container is up. A second
/// quantization lands in the directory through a download — the container
/// is stopped for apply, and the next request starts it handed the q4_0
/// file. Deleting that download again stops the restarted one in turn.
#[tokio::test]
async fn a_download_or_delete_beside_a_running_row_restarts_it_on_the_fresh_render() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    Mock::given(method("GET"))
        .and(path("/o/asr/resolve/main/asr-q8_0.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GGUF q8".to_vec()))
        .mount(&hub)
        .await;

    let f = fixture_n(8 * GIB, 6 * GIB, 3 * GIB, 0, 1).await;
    one_gguf_row(&f).await;
    start_idle(&f, "asr").await;
    assert_eq!(mounted(&f, "asr")["models"][0]["path"], "/models/o/asr");

    let id = store::upsert_hf_model(
        &f.state.db,
        "o/asr",
        "asr-q8_0.gguf",
        "o/asr/asr-q8_0.gguf",
        "audio",
    )
    .await
    .unwrap();
    let row = store::get_hf_model(&f.state.db, id).await.unwrap().unwrap();
    hf_download::start(&f.state, &row).await.unwrap();
    until_status(&f, id, "done").await;
    // The row turns done before the job compares the renders.
    until("the stop for apply", || !f.stops().is_empty()).await;
    assert_eq!(f.stops(), ["asr"], "stopped for apply");
    assert!(!f.state.runtime().contains(Class::Audio, "asr"));

    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(
        mounted(&f, "asr")["models"][0]["path"],
        "/models/o/asr/asr-q4_0.gguf"
    );

    lmgw_core::ops::hf_set(&f.state, "delete", Some(id), "audio")
        .await
        .unwrap();
    assert_eq!(f.stops(), ["asr", "asr"]);
}

// Both tests below capture the log, and they are the only ones that reach
// the lines they read: tracing caches a callsite's interest from the thread
// that hits it first while one capture is alive, and a thread without one
// would cache "never" for the other test's capture.

/// The files change while the row's container is still starting — on the
/// `server.json` rendered a moment before. The start is not cut: its request
/// is answered, on the old render, and the log says the container is left
/// for now. The reaper's next tick stops it once it is idle, and the next
/// request starts it on the fresh render.
#[tokio::test]
async fn files_changing_under_a_starting_container_leave_it_until_it_is_idle() {
    let f = fixture_n(8 * GIB, 6 * GIB, 3 * GIB, 0, 1).await;
    let dir = one_gguf_row(&f).await;
    let (log, capturing) = crate::common::captured_log::capture_log();
    let (release, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);
    let request = tokio::spawn({
        let gateway = f.gateway.clone();
        async move {
            let resp = gateway
                .client()
                .post(format!("{gateway}/v1/audio/speech"))
                .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            resp.bytes().await.unwrap();
            status
        }
    });
    let config = lmgw_core::runtime::audio::config_dir(&f.state.data_dir, "asr")
        .join(lmgw_core::runtime::audio::CONFIG_FILE_NAME);
    until("the start rendered its server.json", || config.exists()).await;
    assert_eq!(mounted(&f, "asr")["models"][0]["path"], "/models/o/asr");

    std::fs::write(dir.join("asr-q8_0.gguf"), b"GGUF q8").unwrap();
    let out = lmgw_core::runtime::audio::stop_stale(&f.state, "the download of q8_0").await;
    assert_eq!(
        out,
        [(
            "asr".to_string(),
            lmgw_core::runtime::audio::Settled::Left("starting".into())
        )]
    );
    assert_eq!(f.state.audio_stale.ids(), ["asr"]);
    release.send(true).unwrap();
    assert_eq!(request.await.unwrap(), 200, "the start is not cut");
    assert!(f.stops().is_empty(), "{:?}", f.stops());
    assert!(
        log.text().contains("while its container was starting"),
        "{}",
        log.text()
    );

    until("the request's claim let go", || {
        f.state.runtime().list().iter().all(|v| v.in_flight == 0)
    })
    .await;
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(f.stops(), ["asr"], "stopped once idle");
    assert!(f.state.audio_stale.ids().is_empty());
    assert!(
        log.text().contains("now idle, still ran the server.json"),
        "{}",
        log.text()
    );
    drop(capturing);
    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(
        mounted(&f, "asr")["models"][0]["path"],
        "/models/o/asr/asr-q4_0.gguf"
    );
}

/// The files change while the row's container serves a request: it is left
/// running, and the reaper's ticks leave it while the request lasts and
/// stop it at the first tick after.
#[tokio::test]
async fn files_changing_under_a_serving_container_stop_it_once_it_is_idle() {
    let f = fixture_n(8 * GIB, 6 * GIB, 3 * GIB, 0, 1).await;
    let dir = one_gguf_row(&f).await;
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, "asr").unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&f.state, &snap, &rt);
    let serving = f.state.runtime().acquire(&spec).await.unwrap();
    let (log, capturing) = crate::common::captured_log::capture_log();

    std::fs::write(dir.join("asr-q8_0.gguf"), b"GGUF q8").unwrap();
    let out = lmgw_core::runtime::audio::stop_stale(&f.state, "the download of q8_0").await;
    assert_eq!(
        out,
        [(
            "asr".to_string(),
            lmgw_core::runtime::audio::Settled::Left("serving 1 request(s)".into())
        )]
    );
    assert!(
        log.text()
            .contains("while its container was serving 1 request(s)"),
        "{}",
        log.text()
    );
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert!(f.stops().is_empty(), "{:?}", f.stops());
    assert_eq!(f.state.audio_stale.ids(), ["asr"]);

    drop(serving);
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(f.stops(), ["asr"]);
    assert!(f.state.audio_stale.ids().is_empty());
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(f.stops(), ["asr"], "forgotten once stopped");
    drop(capturing);
}
