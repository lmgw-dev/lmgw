//! The start itself (headless server design §1, §12 "Start"): `prepare` and
//! `serve` in this process, on the host seams the core's tests use — a
//! podman that answers nothing and records every call, no GPU, a machine
//! with stated facts — and a scratch data dir. Nothing reaches the
//! machine's podman, and every listener is a loopback port the OS picks.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::deployment::{Deployment, PinnedSource};
use lmgw_server::env::ServerEnv;
use lmgw_server::start;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::support::{container_env, host, host_with_secret, recorder, EmptyPodman};

/// `GET path` on `addr`: the status line.
async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut said = String::new();
    conn.read_to_string(&mut said).await.unwrap();
    said.lines().next().unwrap_or_default().to_string()
}

#[tokio::test]
async fn it_starts_with_lmgw_deployment_container() {
    let dir = tempfile::tempdir().unwrap();
    let podman = Arc::new(EmptyPodman::default());
    let (exit, codes) = recorder();
    let (signals, mut rx) = mpsc::channel(2);
    let ready = start::prepare(
        &container_env(dir.path(), false),
        host(&podman),
        &mut rx,
        &exit,
    )
    .await
    .unwrap()
    .expect("no signal: the start is ready to serve");
    let state = ready.state.clone();
    assert_eq!(state.deployment(), Deployment::Container);
    assert!(!state.dev(), "a production start");
    let s = state.snapshot().settings.clone();
    assert_eq!(s.bind_addr, "127.0.0.1:0", "LMGW_BIND_ADDR is pinned");
    assert!(
        s.container_seeded && s.auth_enabled,
        "the seeds are written"
    );
    use PinnedSource::{ImageDefault as Image, Kept, Variable as Var};
    assert_eq!(
        state.start_facts().sources,
        [Var, Kept, Image, Image, Image, Image, Kept, Image]
    );
    assert_eq!(state.stop_budget().unwrap().unit_seconds, Some(95));
    assert!(dir.path().join("lmgw-server.lock").exists());
    assert!(
        podman.calls.lock().unwrap().is_empty(),
        "nothing before serving asks podman: {:?}",
        podman.calls.lock().unwrap()
    );

    // It serves, and stops on SIGTERM through the real teardown.
    let addr = ready.listener.local_addr().unwrap();
    let serving = tokio::spawn(start::serve_until_quit(
        ready.state,
        ready.listener,
        ready.tls,
        rx,
        |_| panic!("one signal never exits"),
        |st| async move { lmgw_core::runtime::lifecycle::shutdown(&st).await },
    ));
    assert_eq!(get(addr, "/api/version").await, "HTTP/1.1 200 OK");
    signals.send("SIGTERM").await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), serving)
        .await
        .expect("the stop ends within its bound")
        .unwrap()
        .unwrap();
    assert!(codes.lock().unwrap().is_empty());
    drop(ready.lock);
}

#[tokio::test]
async fn a_dev_instance_in_container_mode_leaves_the_production_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let podman = Arc::new(EmptyPodman::default());
    let (exit, _) = recorder();
    let (_signals, mut rx) = mpsc::channel(2);
    let mut env = container_env(dir.path(), true);
    env.dev_container_prefix = Some("lmgw-wp1-test".into());
    let ready = start::prepare(&env, host(&podman), &mut rx, &exit)
        .await
        .unwrap()
        .unwrap();
    let s = ready.state.snapshot().settings.clone();
    assert!(ready.state.dev());
    assert_eq!(ready.state.deployment(), Deployment::Container);
    assert_eq!(s.container_prefix, "lmgw-wp1-test");
    assert_eq!(s.bind_addr, "127.0.0.1:0", "still the pinned address");
}

#[tokio::test]
async fn a_dev_instance_outside_a_container_binds_its_lmgw_bind_addr() {
    let dir = tempfile::tempdir().unwrap();
    let podman = Arc::new(EmptyPodman::default());
    let (exit, _) = recorder();
    let (_signals, mut rx) = mpsc::channel(2);
    let env = ServerEnv {
        deployment: Deployment::Desktop,
        ..container_env(dir.path(), true)
    };
    let ready = start::prepare(&env, host(&podman), &mut rx, &exit)
        .await
        .unwrap()
        .unwrap();
    let local = ready.listener.local_addr().unwrap();
    assert!(local.ip().is_loopback() && local.port() != 0, "{local}");
    let s = ready.state.snapshot().settings.clone();
    assert_eq!(s.container_prefix, "lmgw-dev");
    assert_eq!(
        s.bind_addr, "127.0.0.1:0",
        "the dev step stored its address"
    );
    assert!(!s.container_seeded, "a desktop start seeds nothing");
    assert!(!s.auth_enabled);
}

#[tokio::test]
async fn a_quit_during_the_start_serves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let podman = Arc::new(EmptyPodman::default());
    let (exit, codes) = recorder();
    let (signals, mut rx) = mpsc::channel(2);
    signals.send("SIGTERM").await.unwrap();
    let ready = start::prepare(
        &container_env(dir.path(), false),
        host(&podman),
        &mut rx,
        &exit,
    )
    .await
    .unwrap();
    assert!(ready.is_none(), "stopped during the start");
    assert!(codes.lock().unwrap().is_empty(), "one signal never exits");
    // The data dir was opened to the end: the database is whole.
    assert!(dir.path().join("lmgw.sqlite").exists());

    // `serve` ends cleanly on it too.
    let dir = tempfile::tempdir().unwrap();
    let (signals, rx) = mpsc::channel(2);
    signals.send("SIGTERM").await.unwrap();
    start::serve(
        container_env(dir.path(), false),
        host(&podman),
        rx,
        exit.clone(),
    )
    .await
    .unwrap();
}

/// The image's wrapping key (§9, WP8): a container-mode start with the
/// secret wraps the device CA's key under it; the next start without it
/// leaves the CA down with a sentence naming the secret, and one with it
/// again opens the CA.
#[tokio::test]
async fn a_container_start_wraps_the_device_ca_under_the_wrap_key_secret() {
    use lmgw_core::devices::tls::{KeyMode, Trust, TrustError, WrapKey};
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let secret = dir.path().join("lmgw-wrap-key");
    std::fs::write(
        &secret,
        format!("{}\n", WrapKey::generate().unwrap().to_hex().as_str()),
    )
    .unwrap();
    let podman = Arc::new(EmptyPodman::default());
    let (exit, _) = recorder();
    let start = |host| {
        let env = container_env(&data, false);
        let exit = exit.clone();
        async move {
            let (_signals, mut rx) = mpsc::channel(2);
            start::prepare(&env, host, &mut rx, &exit)
                .await
                .unwrap()
                .unwrap()
        }
    };

    let ready = start(host_with_secret(&podman, &secret)).await;
    let ca = ready.state.devices.trust.open(&data, true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Wrapped);
    assert_eq!(Trust::key_mode_on_disk(&data), Some(KeyMode::Wrapped));
    drop(ready);

    let ready = start(host(&podman)).await;
    let e = ready
        .state
        .devices
        .trust
        .open(&data, true)
        .await
        .unwrap_err();
    assert!(matches!(e, TrustError::WrapKeyMissing(_)), "{e}");
    assert!(e.to_string().contains("/nonexistent/lmgw-wrap-key"), "{e}");
    drop(ready);

    let ready = start(host_with_secret(&podman, &secret)).await;
    let again = ready.state.devices.trust.open(&data, false).await.unwrap();
    assert_eq!(again.fp(), ca.fp());
}

/// A plain key on the data volume and the secret there: the start itself
/// wraps it (D102), before anything opens the CA — the device listener
/// off, as `prepare` leaves it.
#[tokio::test]
async fn a_container_start_wraps_a_plain_key_before_the_listener_opens_it() {
    use lmgw_core::devices::tls::{KeyMode, Trust, WrapKey};
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let secret = dir.path().join("lmgw-wrap-key");
    let podman = Arc::new(EmptyPodman::default());
    let (exit, _) = recorder();
    let start = |host| {
        let env = container_env(&data, false);
        let exit = exit.clone();
        async move {
            let (_signals, mut rx) = mpsc::channel(2);
            start::prepare(&env, host, &mut rx, &exit)
                .await
                .unwrap()
                .unwrap()
        }
    };

    // A start without the secret makes the CA plain.
    let ready = start(host_with_secret(&podman, &secret)).await;
    let ca = ready.state.devices.trust.open(&data, true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Plain);
    drop(ready);

    // The owner makes the secret; the next start wraps the key, nothing
    // opening the CA.
    std::fs::write(
        &secret,
        format!("{}\n", WrapKey::generate().unwrap().to_hex().as_str()),
    )
    .unwrap();
    let ready = start(host_with_secret(&podman, &secret)).await;
    assert_eq!(Trust::key_mode_on_disk(&data), Some(KeyMode::Wrapped));
    let again = ready.state.devices.trust.open(&data, false).await.unwrap();
    assert_eq!(again.fp(), ca.fp());
    assert_eq!(again.key_mode(), KeyMode::Wrapped);
}
