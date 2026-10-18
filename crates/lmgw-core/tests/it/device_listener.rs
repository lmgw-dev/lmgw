//! The device listener's life (device link design §4.1, §4.4, §9.2, WP2):
//! its settings act at once — on, off, a move of scope or port — each stop
//! ending its own streams and never the plain listener's; the first start
//! writes its port; a failed bind leaves the plain listener serving and says
//! why; a gateway stop stops both; the status and the save's answer say what
//! it does.

use std::time::Duration;

use lmgw_api_types::device_link::DeviceListenerState;
use lmgw_core::config::DevicesListen;
use lmgw_core::erased::Erased;
use lmgw_core::state::AppState;
use serde_json::{json, Value};

use crate::common::{self, dashboard_key};
use crate::support::device_link::{self, exchange, free_port, gateway, get};

/// Save `devices` through the dashboard's op, as the Settings page does;
/// the answer's message.
async fn save(gw: &common::Gw, devices: Value) -> String {
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({ "devices": devices }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body.to_string()
}

/// Read `resp` until it ends; whether it did within `secs`.
async fn ends_within(resp: &mut reqwest::Response, secs: u64) -> bool {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while let Ok(Some(_)) = resp.chunk().await {}
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn turning_it_off_ends_its_feeds_and_leaves_the_plain_listener_s() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let mut device_feed = gw
        .https(Some(&device.identity))
        .get(gw.url("/chat/api/feed"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(device_feed.status(), 200);
    device_feed.chunk().await.unwrap().unwrap();
    let mut plain_feed = gw
        .gw
        .client()
        .get(format!("{}/chat/api/feed", gw.gw))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(plain_feed.status(), 200);
    plain_feed.chunk().await.unwrap().unwrap();

    let status = gw.state.devices.listener.status(&gw.state);
    assert_eq!(status.connections, 1, "{status:?}");
    assert_eq!(status.requests, 1, "the feed is in flight: {status:?}");

    let answer = save(&gw.gw, json!({ "listener": false })).await;
    assert!(answer.contains("the device listener is off"), "{answer}");
    assert!(answer.contains("1 device connection ended"), "{answer}");
    assert!(
        ends_within(&mut device_feed, 10).await,
        "the device feed ended"
    );
    assert!(
        !ends_within(&mut plain_feed, 1).await,
        "the plain listener's feed is open still"
    );
    let r = tokio::net::TcpStream::connect(("127.0.0.1", gw.port)).await;
    assert!(r.is_err(), "its port is closed");
    let status = gw.state.devices.listener.status(&gw.state);
    assert_eq!(status.state, DeviceListenerState::Off, "{status:?}");

    // On again: a new generation, which the stop before is not.
    let answer = save(&gw.gw, json!({ "listener": true })).await;
    assert!(
        answer.contains("the device listener listens on https://127.0.0.1"),
        "{answer}"
    );
    let r = gw
        .https(Some(&device.identity))
        .get(gw.url("/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_move_rebinds_at_once_and_the_old_port_refuses() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let old = gw.port;
    let new = free_port();
    let answer = save(&gw.gw, json!({ "port": new })).await;
    assert!(
        answer.contains(&format!("https://127.0.0.1:{new}")),
        "{answer}"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", old))
            .await
            .is_err(),
        "the old port refuses"
    );
    let r = reqwest::Client::builder()
        .tls_backend_preconfigured(device_link::client_config(
            gw.ca.certificate(),
            Some(&device.identity),
            &[&rustls::version::TLS13],
        ))
        .build()
        .unwrap()
        .get(format!("https://127.0.0.1:{new}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // And back to the same port it had, at once: its socket was closed.
    let answer = save(&gw.gw, json!({ "port": old })).await;
    assert!(
        answer.contains(&format!("https://127.0.0.1:{old}")),
        "{answer}"
    );
    let r = gw
        .https(Some(&device.identity))
        .get(gw.url("/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn loopback_binds_the_loopback_pair_and_all_binds_every_address() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let bound = gw.state.devices.listener.bound();
    assert!(bound.iter().all(|a| a.ip().is_loopback()), "{bound:?}");
    assert!(bound.iter().any(|a| a.is_ipv4()), "{bound:?}");

    let answer = save(&gw.gw, json!({ "listen": "all" })).await;
    assert!(
        answer.contains(&format!("https://[::]:{}", gw.port)),
        "{answer}"
    );
    let status = gw.state.devices.listener.status(&gw.state);
    assert_eq!(status.listen.as_deref(), Some("all"), "{status:?}");
    // Dual-stack: an IPv4 client reaches `[::]`.
    let r = gw
        .https(Some(&device.identity))
        .get(gw.url("/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let answer = save(&gw.gw, json!({ "listen": "127.0.0.1" })).await;
    assert!(
        answer.contains(&format!("https://127.0.0.1:{}", gw.port)),
        "{answer}"
    );
    assert_eq!(
        gw.state.devices.listener.bound(),
        [std::net::SocketAddr::from(([127, 0, 0, 1], gw.port))]
    );
}

#[tokio::test]
async fn the_first_start_writes_its_port_and_a_later_bind_addr_leaves_it() {
    let state = AppState::init_for_tests().await.unwrap();
    // The HTTP port is the number below a free one.
    let port = free_port();
    let mut s = state.snapshot().settings.clone();
    s.bind_addr = format!("127.0.0.1:{}", port - 1);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state.devices.listener.arm(&state).await;
    let gw = common::serve(state.clone()).await;

    let answer = save(&gw, json!({ "listener": true })).await;
    assert!(answer.contains("devices.port is written"), "{answer}");
    assert!(
        answer.contains(&format!("https://127.0.0.1:{port}")),
        "{answer}"
    );
    assert_eq!(state.snapshot().settings.devices.port, Some(port));
    // bind_addr moves; the device port stays where the links name it.
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({ "bind_addr": "127.0.0.1:1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(state.snapshot().settings.devices.port, Some(port));
    assert_eq!(
        state.devices.listener.bound()[0].port(),
        port,
        "nothing moved"
    );

    // A setting written by hand without a port gets it at the next start.
    let fresh = AppState::init_for_tests().await.unwrap();
    let port = free_port();
    let mut s = fresh.snapshot().settings.clone();
    s.bind_addr = format!("127.0.0.1:{}", port - 1);
    s.devices.listener = true;
    lmgw_core::store::save_settings(&fresh.db, &s)
        .await
        .unwrap();
    fresh.reload_snapshot().await.unwrap();
    fresh.devices.listener.arm(&fresh).await;
    assert_eq!(fresh.snapshot().settings.devices.port, Some(port));
    assert_eq!(fresh.devices.listener.bound()[0].port(), port);
}

#[tokio::test]
async fn a_failed_bind_leaves_the_plain_listener_serving_and_says_why() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let state = AppState::init_for_tests().await.unwrap();
    device_link::arm(&state, port, |s| {
        s.devices.listen = DevicesListen::Addr("127.0.0.1".parse().unwrap())
    })
    .await;
    let gw = common::serve(state.clone()).await;
    let status: Value = gw
        .client()
        .get(format!("{gw}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listener = &status["devices"]["listener"];
    assert_eq!(listener["state"], "error", "{listener}");
    let error = listener["error"].as_str().unwrap();
    assert!(error.contains(&format!("127.0.0.1:{port}")), "{error}");
    assert!(error.contains("devices.port"), "{error}");
    // The plain listener served that very request; and a freed port is
    // taken by the next save.
    drop(taken);
    let answer = save(&gw, json!({ "listen": "loopback", "port": free_port() })).await;
    assert!(
        answer.contains("the device listener listens on"),
        "{answer}"
    );
}

#[tokio::test]
async fn the_status_says_where_it_listens_and_what_it_holds() {
    let gw = gateway().await;
    let status: Value = gw
        .gw
        .client()
        .get(format!("{}/api/status", gw.gw))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listener = &status["devices"]["listener"];
    assert_eq!(listener["state"], "listening", "{listener}");
    assert_eq!(listener["listen"], "loopback");
    assert_eq!(listener["port"], gw.port);
    assert!(
        listener["urls"][0]
            .as_str()
            .unwrap()
            .starts_with("https://127.0.0.1:"),
        "{listener}"
    );
    assert_eq!(status["devices"]["ca"]["state"], "ready");
}

#[tokio::test]
async fn a_gateway_stop_stops_both_listeners() {
    let state = AppState::init_for_tests().await.unwrap();
    let keys = std::sync::Arc::new(lmgw_core::devices::MemoryKeys::default());
    state.devices.keys.set(keys.clone());
    let port = free_port();
    let mut s = state.snapshot().settings.clone();
    s.devices.listener = true;
    s.devices.port = Some(port);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let key = dashboard_key(&state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let st = state.clone();
    let server = tokio::spawn(async move {
        lmgw_core::server::serve(
            st,
            listener,
            async {
                let _ = stopped.await;
            },
            None,
        )
        .await
    });
    // The device listener comes up beside it.
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.devices.listener.bound().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the device listener started with the gateway");
    let ca = state.devices.trust.ca().unwrap();
    let (identity, key_hash) = device_link::identity(&ca, 365, chrono::Utc::now());
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash, enabled, kind) VALUES ('device:p', ?1, 1, \
         'device') RETURNING id",
    )
    .bind(&key_hash)
    .fetch_one(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    keys.set(&key_hash, id, true);
    let mut device_feed = reqwest::Client::builder()
        .tls_backend_preconfigured(device_link::client_config(
            ca.certificate(),
            Some(&identity),
            &[&rustls::version::TLS13],
        ))
        .build()
        .unwrap()
        .get(format!("https://127.0.0.1:{port}/chat/api/feed"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(device_feed.status(), 200);
    device_feed.chunk().await.unwrap().unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/api/status"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .expect("the gateway stopped within its bound")
        .unwrap()
        .unwrap();
    assert!(
        ends_within(&mut device_feed, 5).await,
        "the device feed ended"
    );
    assert!(state.devices.listener.bound().is_empty());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "the device port is closed"
    );
}

#[tokio::test]
async fn the_server_certificate_is_chosen_by_the_connection_s_local_address() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    // The handshake picked the loopback leaf by the local address the
    // connection arrived on, which `Conn::local` carries on.
    let mut tls = gw.connect(Some(&device.identity)).await.unwrap();
    let raw = exchange(&mut tls, &get("/v1/models", ""), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(raw.status, 200);
    let leaves = gw.state.devices.trust.leaves.leaves();
    assert_eq!(leaves.len(), 1);
    assert_eq!(
        leaves[0].slot,
        lmgw_core::devices::tls::LeafSlot::Loopback,
        "chosen by the connection's local address"
    );
}

#[tokio::test]
async fn what_follows_it_sees_every_start_move_and_stop() {
    let gw = gateway().await;
    let mut watch = gw.state.devices.listener.watch();
    let first = watch.borrow_and_update().clone().expect("it serves");
    assert!(first.addrs.iter().all(|a| a.port() == gw.port), "{first:?}");
    assert_eq!(first.served_at.via, lmgw_core::server::Via::Devices);

    let new = free_port();
    save(&gw.gw, json!({ "port": new })).await;
    assert!(watch.has_changed().unwrap());
    let moved = watch.borrow_and_update().clone().expect("it serves");
    assert!(moved.addrs.iter().all(|a| a.port() == new), "{moved:?}");
    assert!(
        moved.served_at.at > first.served_at.at,
        "a new generation: the old start's stop ends what it opened"
    );
    save(&gw.gw, json!({ "listener": false })).await;
    assert!(watch.borrow_and_update().is_none());
}

/// Review S3: a handshake has no request in progress, so a stop ends it at
/// once: with an idle socket at the handshake, the stop's drain finishes
/// whole, nothing closed by force at the end of the bound.
#[tokio::test]
async fn a_stop_ends_a_handshake_that_never_finishes_at_once() {
    let gw = device_link::gateway_with(|s| s.devices.handshake_timeout_s = 300).await;
    let _idle = gw.tcp_from([127, 0, 0, 1]).await;
    gw.unauthenticated(1).await;
    let drained = tokio::time::timeout(
        lmgw_core::server::STOP_WITHIN * 2,
        gw.state
            .devices
            .listener
            .disarm(&gw.state, gw.epoch, lmgw_core::server::STOP_WITHIN),
    )
    .await
    .expect("the disarm returns within its bound");
    assert!(drained, "drained whole: the handshake ended at the stop");
}

/// A Secret Service whose answer waits until the test releases it: an
/// unlock prompt nobody answers.
struct PromptStore {
    inner: std::sync::Arc<lmgw_core::devices::tls::MemoryWrapStore>,
    asked: tokio::sync::Notify,
    answer: tokio::sync::Notify,
}

impl lmgw_core::devices::tls::WrapKeyStore for PromptStore {
    fn get<'a>(
        &'a self,
        dir: &'a std::path::Path,
    ) -> Erased<
        'a,
        Result<Vec<lmgw_core::devices::tls::WrapKey>, lmgw_core::devices::tls::StoreError>,
    > {
        Box::pin(async move {
            self.asked.notify_one();
            self.answer.notified().await;
            self.inner.get(dir).await
        })
    }

    fn add<'a>(
        &'a self,
        dir: &'a std::path::Path,
        key: &'a lmgw_core::devices::tls::WrapKey,
    ) -> Erased<'a, Result<(), lmgw_core::devices::tls::StoreError>> {
        Box::pin(async move { self.inner.add(dir, key).await })
    }

    fn prune<'a>(
        &'a self,
        dir: &'a std::path::Path,
        keep: &'a lmgw_core::devices::tls::WrapKey,
    ) -> Erased<'a, Result<usize, lmgw_core::devices::tls::StoreError>> {
        Box::pin(async move { self.inner.prune(dir, keep).await })
    }
}

/// Review: the wallet's unlock prompt waits for as long as nobody answers
/// it. A gateway stop during it returns within its bound, the listener's
/// lock is not held by it, and the start it cancelled never binds.
#[tokio::test]
async fn a_stop_during_the_wallet_s_prompt_neither_waits_nor_binds_after_it() {
    use lmgw_core::devices::tls::{MemoryWrapStore, Trust, WrapKeyStore};
    let state = AppState::init_for_tests().await.unwrap();
    // A wrapped CA in this gateway's data directory, made beside it.
    let mem = std::sync::Arc::new(MemoryWrapStore::default());
    let maker = Trust::default();
    maker.set_store(mem.clone());
    maker.open(&state.data_dir, true).await.unwrap();
    let prompt = std::sync::Arc::new(PromptStore {
        inner: mem,
        asked: tokio::sync::Notify::new(),
        answer: tokio::sync::Notify::new(),
    });
    state
        .devices
        .trust
        .set_store(prompt.clone() as std::sync::Arc<dyn WrapKeyStore>);
    let port = free_port();
    let mut s = state.snapshot().settings.clone();
    s.devices.listener = true;
    s.devices.port = Some(port);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let key = dashboard_key(&state);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let st = state.clone();
    let server = tokio::spawn(async move {
        lmgw_core::server::serve(
            st,
            listener,
            async {
                let _ = stopped.await;
            },
            None,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), prompt.asked.notified())
        .await
        .expect("the start asked the Secret Service");

    // The plain listener serves meanwhile, a save takes the settings lock
    // and answers, and the status reads.
    let r = reqwest::Client::new()
        .post(format!("{base}/api/op/settings_set_full"))
        .bearer_auth(&key)
        .json(&json!({ "devices": { "token_minutes": 20 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let status = state.devices.listener.status(&state);
    assert_eq!(status.state, DeviceListenerState::Off, "{status:?}");

    let stopping = std::time::Instant::now();
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the stop did not wait on the prompt")
        .unwrap()
        .unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(5));
    // The cancelled opening is gone with the server's task, which has
    // returned: nothing is left to bind, and the port is the test's to take.
    assert!(state.devices.listener.bound().is_empty());
    let free = std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("the device listener never bound its port");
    drop(free);
    // The epoch was taken back: a later start serves again.
    prompt.answer.notify_one();
    let epoch = tokio::time::timeout(Duration::from_secs(10), state.devices.listener.arm(&state))
        .await
        .expect("the next start is not held by the cancelled one");
    assert!(epoch > 0);
    assert_eq!(state.devices.listener.bound()[0].port(), port);
}
