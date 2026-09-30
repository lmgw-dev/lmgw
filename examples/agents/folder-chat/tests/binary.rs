//! The `folder-chat` binary itself, as lmgw runs it: started with the
//! service-mode environment, healthy before any model answers, syncing on its
//! own, and stopping on SIGTERM mid-sync well inside `stop_grace_seconds` —
//! the sync first, then the request drain — with the index consistent.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use common::fake::{models, seed, serve, Fake, Gate};
use common::harness::{sse_until, AUTHORITY, LOCAL_CLIENT, ORIGIN, PATIENCE};
use folder_chat::index::IndexDir;
use quickdoc_core::store;
use serde_json::json;

/// lmgw's default `run.limits.stop_grace_seconds`: podman SIGKILLs after it.
const STOP_GRACE: Duration = Duration::from_secs(10);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn starts_syncing_and_stops_cleanly_on_sigterm_mid_sync() {
    // Call 0 is the width probe; the first file's batch is held.
    let (gate, held) = Gate::closed(1);
    let (api, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        gate: Some(held),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    seed(&folder);
    let input = tmp.path().join("input.json");
    std::fs::write(
        &input,
        json!({
            "phase": "service",
            "agent": {"id": "folder-chat", "name": "Folder chat"},
            "config": {
                "folder": folder,
                "embed_model": "embed/fixture",
                "chat_model": "chat-small",
                "chunk_tokens": 400
            }
        })
        .to_string(),
    )
    .unwrap();
    let port = free_port();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_folder-chat"))
        .env_clear()
        .env("LMGW_PORT", port.to_string())
        .env("LMGW_APP_ORIGIN", ORIGIN)
        .env("LMGW_API_BASE", &api)
        .env("LMGW_INPUT", &input)
        .env("LMGW_PHASE", "service")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();

    // Healthy without the forwarded header, and before any sync finished.
    tokio::time::timeout(PATIENCE, async {
        loop {
            if let Ok(r) = http.get(format!("{base}/healthz")).send().await {
                if r.status() == 200 {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the binary never became healthy");

    // It started a sync by itself; a browser tab is watching it.
    let resp = http
        .get(format!("{base}/api/sync/events"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-lmgw-face", "app")
        .header("x-forwarded-for", LOCAL_CLIENT)
        .send()
        .await
        .unwrap();
    let seen = tokio::spawn(sse_until(resp, |_| false));
    let status: serde_json::Value = http
        .get(format!("{base}/api/status"))
        .header("x-forwarded-host", AUTHORITY)
        .header("x-lmgw-face", "app")
        .header("x-forwarded-for", LOCAL_CLIENT)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sync"]["run"], 1, "{status}");
    assert_eq!(status["sync"]["running"], true, "{status}");
    tokio::time::timeout(PATIENCE, async {
        while fake.embed_calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the start-up sync never reached its first batch");

    // SIGTERM mid-sync, with an SSE stream open.
    let pid = child.id().unwrap() as libc::pid_t;
    let t0 = Instant::now();
    // SAFETY: signalling our own child by its pid.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let exit = tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .expect("still running when podman would SIGKILL it")
        .unwrap();
    let took = t0.elapsed();
    let mut log = String::new();
    if let Some(mut e) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let _ = e.read_to_string(&mut log).await;
    }
    assert!(exit.success(), "{exit:?}\n{log}");
    assert!(
        took < Duration::from_secs(5),
        "a stop with nothing slow in flight is quick: {took:?}\n{log}"
    );
    assert!(log.contains("SIGTERM"), "{log}");
    // The sync stops first, then requests drain: left running, it would go
    // on embedding through SHUTDOWN_DRAIN.
    let stopped = log.find("stopped mid-way").expect(&log);
    let draining = log.find("draining in-flight requests").expect(&log);
    assert!(
        stopped < draining,
        "the sync stops before the drain:\n{log}"
    );
    assert!(
        log.contains("sync stopped because the app is shutting down"),
        "{log}"
    );
    // The browser's stream ended rather than holding the shutdown open.
    let events = tokio::time::timeout(PATIENCE, seen).await.unwrap().unwrap();
    assert!(events.iter().any(|e| e.data["type"] == "embedding"));
    drop(gate);

    // Consistent: the file that was mid-embedding is not marked current, so
    // the next sync redoes it.
    let idx = IndexDir::open(&folder).await.unwrap();
    let corpus = store::get_corpus_by_id(idx.pool(), "folder@1")
        .await
        .unwrap()
        .unwrap();
    assert!(idx.file_states(corpus.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn refuses_to_start_without_the_service_environment() {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_folder-chat"))
        .env_clear()
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("LMGW_PORT is not set"), "{err}");
}
