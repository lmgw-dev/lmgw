//! WebSocket upgrades over the device listener (device link design §4.3,
//! §4.4, WP2 review): a realtime session and a host link run over TLS with
//! the device's certificate, and turning the device listener off closes
//! them with 1001 as a gateway stop does, while a session on the plain
//! listener stays up.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lmgw_core::devices::MemoryKeys;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::support::device_link::{self, client_config};

/// The next text event of `ws`, within 10 s.
async fn next_event<S>(ws: &mut WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                other => panic!("expected a text event, got {other:?}"),
            }
        }
    })
    .await
    .expect("an event within 10 s")
}

/// The close `ws` gets within `secs`, skipping what comes before it.
async fn close_of<S>(ws: &mut WebSocketStream<S>, secs: u64) -> Option<CloseCode>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => return c.map(|c| c.code),
                Some(Ok(_)) => continue,
                other => panic!("expected a close frame, got {other:?}"),
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn turning_the_listener_off_closes_its_websockets_with_1001_and_not_the_plain_ones() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let keys = Arc::new(MemoryKeys::default());
    w.state.devices.keys.set(keys.clone());
    let (port, _) = device_link::arm_free(&w.state, |_| {}).await;
    let ca = w.state.devices.trust.ca().unwrap();
    let (identity, key_hash) = device_link::identity(&ca, 365, chrono::Utc::now());
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash, enabled, kind) VALUES ('device:phone', ?1, 1, \
         'device') RETURNING id",
    )
    .bind(&key_hash)
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    keys.set(&key_hash, id, true);
    // A hosting grant, so the host link opens (client-apps §5.1).
    let r =
        w.gw.client()
            .post(format!("{}/api/op/key_set", w.gw))
            .json(&json!({ "id": id, "hosts_label": "phone" }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);

    let tls_ws = |path: &'static str| {
        let cfg = Arc::new(client_config(
            ca.certificate(),
            Some(&identity),
            &[&rustls::version::TLS13],
        ));
        async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let tls = tokio_rustls::TlsConnector::from(cfg)
                .connect(
                    rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                    tcp,
                )
                .await
                .unwrap();
            let req = format!("ws://127.0.0.1:{port}{path}")
                .into_client_request()
                .unwrap();
            let (ws, resp) = tokio_tungstenite::client_async(req, tls)
                .await
                .unwrap_or_else(|e| panic!("the upgrade of {path} failed: {e}"));
            assert_eq!(resp.status().as_u16(), 101, "{path}");
            ws
        }
    };

    // On the device listener: a realtime session and the host link.
    let mut device_session = tls_ws("/v1/realtime?model=chatty").await;
    let created = next_event(&mut device_session).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let mut host_link = tls_ws("/mcp/host").await;
    // On the plain listener: the owner's session.
    let mut req = format!("ws://{}/v1/realtime?model=chatty", w.addr())
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("cookie", w.cookie().parse().unwrap());
    let (mut plain_session, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let created = next_event(&mut plain_session).await;
    assert_eq!(created["type"], "session.created", "{created}");

    let r =
        w.gw.client()
            .post(format!("{}/api/op/settings_set_full", w.gw))
            .json(&json!({ "devices": { "listener": false } }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);

    assert_eq!(
        close_of(&mut device_session, 10).await,
        Some(CloseCode::Away),
        "the device's session closes as at a stop"
    );
    assert_eq!(
        close_of(&mut host_link, 10).await,
        Some(CloseCode::Away),
        "and so does its host link"
    );
    assert_eq!(
        close_of(&mut plain_session, 1).await,
        None,
        "the plain listener's session stays up"
    );
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while w.state.stops.running_on(lmgw_core::server::Via::Devices) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ended.is_ok(), "no device session still runs");
    assert_eq!(
        w.state.stops.running_on(lmgw_core::server::Via::Plain),
        1,
        "the plain listener's session runs on"
    );
}
