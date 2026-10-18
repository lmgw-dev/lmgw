//! The device listener's TLS and its principal (device link design §4.2,
//! §4.3, §4.7, WP2): a client certificate mandatory while no pairing token
//! is pending; without one, enrolment alone, every other route a 401 with
//! `Connection: close` and no request row, the whole connection bounded; an
//! idle connection closed after `devices.header_timeout_s`; a confirmed
//! device's certificate reaching the feed, the Chat API and `/v1`; every
//! other certificate, credential and route refused by name.

use std::time::Duration;

use chrono::Utc;
use lmgw_api_types::device_link::code;
use serde_json::Value;

use crate::support::device_link::{
    self, closes_within, exchange, gateway, gateway_with, get, read_answer,
};

/// A token pending on some device row, as pairing (WP3) leaves one.
fn token_pending(gw: &device_link::DeviceGw) {
    gw.state
        .devices
        .pending
        .set(9999, Utc::now() + chrono::Duration::minutes(10));
}

#[tokio::test]
async fn with_no_token_pending_a_client_without_a_certificate_fails_the_handshake() {
    let gw = gateway().await;
    // TLS 1.3: the client finishes its side, and the server's refusal is
    // the first thing it reads.
    let refused = match gw.connect(None).await {
        Err(e) => e.to_string(),
        Ok(mut tls) => {
            match exchange(&mut tls, &get("/v1/models", ""), Duration::from_secs(5)).await {
                Err(e) => e.to_string(),
                Ok(raw) => panic!("served without a certificate: {raw:?}"),
            }
        }
    };
    assert!(
        refused.to_ascii_lowercase().contains("certificate"),
        "{refused}"
    );
}

#[tokio::test]
async fn without_a_certificate_only_enrolment_passes_and_every_answer_closes() {
    let gw = gateway().await;
    token_pending(&gw);
    let rows = gw.request_rows().await;
    for (path, openai) in [("/v1/models", true), ("/chat/api/threads", false)] {
        let mut tls = gw.connect(None).await.unwrap();
        let raw = exchange(&mut tls, &get(path, ""), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(raw.status, 401, "{path}: {raw:?}");
        assert_eq!(raw.header("connection"), Some("close"), "{path}");
        assert!(
            raw.closed || closes_within(&mut tls, Duration::from_secs(2)).await,
            "{path}: the connection ends with the answer"
        );
        let body: Value = serde_json::from_str(&raw.body).unwrap();
        let code = if openai {
            &body["error"]["code"]
        } else {
            &body["code"]
        };
        assert_eq!(code, code::CLIENT_CERTIFICATE_REQUIRED, "{path}: {body}");
    }
    // Whatever *Require API key* says (off here, which lets an anonymous
    // `/v1` call through on the plain listener), and whatever credential
    // comes along.
    let mut tls = gw.connect(None).await.unwrap();
    let raw = exchange(
        &mut tls,
        &get("/v1/models", "Authorization: Bearer lmgw-nothing\r\n"),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(raw.status, 401, "{raw:?}");
    // Enrolment's path is let through to its route, which reads the body
    // (not an enrolment here): never the 401.
    let mut tls = gw.connect(None).await.unwrap();
    let raw = exchange(
        &mut tls,
        "POST /devices/enrol HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{}",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(raw.status, 400, "{raw:?}");
    assert!(raw.body.contains("\"bad_request\""), "{raw:?}");
    assert_eq!(raw.header("connection"), Some("close"));
    // A route off the face stays the 404 an unknown path is.
    let mut tls = gw.connect(None).await.unwrap();
    let raw = exchange(&mut tls, &get("/api/status", ""), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(raw.status, 404, "{raw:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        gw.request_rows().await,
        rows,
        "a connection without a certificate writes no request row"
    );
}

#[tokio::test]
async fn a_connection_without_a_certificate_is_closed_after_its_bound() {
    let gw = gateway_with(|s| s.devices.unauthenticated_timeout_s = 1).await;
    token_pending(&gw);
    let mut tls = gw.connect(None).await.unwrap();
    let started = std::time::Instant::now();
    assert!(
        closes_within(&mut tls, Duration::from_secs(5)).await,
        "closed after devices.unauthenticated_timeout_s"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_idle_connection_is_closed_after_the_header_timeout() {
    let gw = gateway_with(|s| s.devices.header_timeout_s = 1).await;
    let device = gw.device("phone", true).await;
    // One that sends nothing after its handshake.
    let mut tls = gw.connect(Some(&device.identity)).await.unwrap();
    assert!(closes_within(&mut tls, Duration::from_secs(5)).await);
    // One kept alive after an answer.
    let mut tls = gw.connect(Some(&device.identity)).await.unwrap();
    let raw = exchange(&mut tls, &get("/v1/models", ""), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(raw.status, 200, "{raw:?}");
    assert!(!raw.closed, "kept alive");
    let started = std::time::Instant::now();
    assert!(closes_within(&mut tls, Duration::from_secs(5)).await);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_confirmed_device_reaches_the_feed_the_chat_api_and_v1() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let https = gw.https(Some(&device.identity));
    let models = https.get(gw.url("/v1/models")).send().await.unwrap();
    assert_eq!(models.status(), 200);
    let threads = https.get(gw.url("/chat/api/threads")).send().await.unwrap();
    assert_eq!(threads.status(), 200, "{:?}", threads.text().await);
    let mut feed = https
        .get(gw.url("/chat/api/feed"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(feed.status(), 200);
    let first = feed.chunk().await.unwrap().unwrap();
    assert!(
        String::from_utf8_lossy(&first).contains("event: hello"),
        "the feed is up"
    );
    // Over [::1] too, where the machine has it.
    if gw
        .state
        .devices
        .listener
        .bound()
        .iter()
        .any(|a| a.is_ipv6())
    {
        let mut tls = gw
            .connect_with("::1", Some(&device.identity), &[&rustls::version::TLS13])
            .await
            .unwrap();
        let raw = exchange(&mut tls, &get("/v1/models", ""), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(raw.status, 200, "{raw:?}");
    }
}

#[tokio::test]
async fn each_certificate_that_opens_no_working_device_is_refused_by_name() {
    let gw = gateway().await;
    let https = |id| gw.https(Some(id));

    // Another CA's certificate, and a lapsed one: the handshake.
    let other = gateway().await;
    let (foreign, _) = device_link::identity(&other.ca, 365, Utc::now());
    let (lapsed, _) = device_link::identity(&gw.ca, 1, Utc::now() - chrono::Duration::days(3));
    for id in [&foreign, &lapsed] {
        let r = https(id).get(gw.url("/v1/models")).send().await;
        assert!(r.is_err(), "{r:?}");
    }

    // A valid certificate whose key no row holds.
    let (stranger, _) = device_link::identity(&gw.ca, 365, Utc::now());
    let r = https(&stranger)
        .get(gw.url("/chat/api/threads"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], "device_key_unknown", "{body}");

    // Unconfirmed: 403 with its code, on /v1 and the Chat API alike.
    let pending = gw.device("tablet", false).await;
    let r = https(&pending.identity)
        .get(gw.url("/chat/api/threads"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], code::DEVICE_UNCONFIRMED, "{body}");
    let confirm = lmgw_api_types::device_link::confirm_code(
        &hex::decode(pending.key_hash.trim_start_matches("spki:"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    assert!(
        body["message"].as_str().unwrap().contains(&confirm),
        "{body}"
    );
    let r = https(&pending.identity)
        .get(gw.url("/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], code::DEVICE_UNCONFIRMED, "{body}");

    // Disabled: the next request on a kept-alive connection is refused.
    let phone = gw.device("phone", true).await;
    let mut tls = gw.connect(Some(&phone.identity)).await.unwrap();
    let raw = exchange(
        &mut tls,
        &get("/chat/api/threads", ""),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(raw.status, 200, "{raw:?}");
    sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?1")
        .bind(phone.id)
        .execute(&gw.state.db)
        .await
        .unwrap();
    gw.state.reload_snapshot().await.unwrap();
    let raw = exchange(
        &mut tls,
        &get("/chat/api/threads", ""),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(raw.status, 401, "{raw:?}");
    assert!(raw.body.contains("device_disabled"), "{raw:?}");
}

#[tokio::test]
async fn a_credential_beside_the_certificate_is_a_400() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let https = gw.https(Some(&device.identity));
    for (name, value) in [
        ("authorization", format!("Bearer {}", gw.gw.key)),
        ("x-api-key", gw.gw.key.clone()),
        ("cookie", format!("lmgw_session={}", gw.gw.key)),
    ] {
        for (path, openai) in [("/v1/models", true), ("/chat/api/threads", false)] {
            let r = https
                .get(gw.url(path))
                .header(name, &value)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{name} on {path}");
            let body: Value = r.json().await.unwrap();
            let code = if openai {
                &body["error"]["code"]
            } else {
                &body["code"]
            };
            assert_eq!(
                code,
                code::CREDENTIAL_ON_DEVICE_LISTENER,
                "{name} on {path}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn the_spa_the_dashboard_api_and_the_agent_proxy_are_not_on_the_device_face() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let https = gw.https(Some(&device.identity));
    for (method, path) in [
        ("GET", "/"),
        ("GET", "/ui/"),
        ("GET", "/settings/network"),
        ("GET", "/api/status"),
        ("POST", "/api/op/settings_set"),
        ("GET", "/agents/board/app/"),
        ("POST", "/mcp/admin"),
        ("GET", "/chat/api/threads/rows"),
        // A path known under another method is no 405 here.
        ("DELETE", "/api/status"),
        ("GET", "/no/such/path"),
    ] {
        let r = https
            .request(method.parse().unwrap(), gw.url(path))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{method} {path}");
    }
    // No CORS layer: a preflight is no CORS answer.
    let r = https
        .request(reqwest::Method::OPTIONS, gw.url("/v1/models"))
        .header("origin", "https://example.org")
        .header("access-control-request-method", "GET")
        .send()
        .await
        .unwrap();
    assert!(
        r.headers().get("access-control-allow-origin").is_none(),
        "{:?}",
        r.headers()
    );
}

#[tokio::test]
async fn tls_1_2_is_refused_and_a_second_connection_does_not_resume() {
    let gw = gateway().await;
    let device = gw.device("phone", true).await;
    let r = gw
        .connect_with(
            "127.0.0.1",
            Some(&device.identity),
            &[&rustls::version::TLS12],
        )
        .await;
    assert!(r.is_err(), "TLS 1.2 is refused");

    // One client config for both connections, so a ticket the first got
    // would be offered by the second.
    let cfg = std::sync::Arc::new(device_link::client_config(
        gw.ca.certificate(),
        Some(&device.identity),
        &[&rustls::version::TLS13],
    ));
    for round in 0..2 {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", gw.port))
            .await
            .unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(cfg.clone())
            .connect(
                rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                tcp,
            )
            .await
            .unwrap();
        let raw = exchange(
            &mut tls,
            &get("/v1/models", "Connection: close\r\n"),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(raw.status, 200);
        let (_, client) = tls.get_ref();
        assert_eq!(
            client.handshake_kind(),
            Some(rustls::HandshakeKind::Full),
            "round {round}: a full handshake, never a resumed one"
        );
    }
}

#[tokio::test]
async fn the_header_timeout_bounds_a_request_head_that_never_ends() {
    let gw = gateway_with(|s| s.devices.header_timeout_s = 1).await;
    let device = gw.device("phone", true).await;
    let mut tls = gw.connect(Some(&device.identity)).await.unwrap();
    use tokio::io::AsyncWriteExt;
    tls.write_all(b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await
        .unwrap();
    let raw = read_answer(&mut tls, Duration::from_secs(5)).await;
    let closed = match raw {
        Ok(raw) => raw.closed || raw.status == 408,
        Err(_) => true,
    };
    assert!(closed, "the head's bound ends it");
}

/// A handshake bound far beyond any wait here: an idle connection's place is
/// then held for the whole test, and nothing lapses by the clock.
const LONG_HANDSHAKE_S: u32 = 300;

/// Whether `tcp` is still open after a moment: no close within half a
/// second.
async fn still_open(tcp: &mut tokio::net::TcpStream) -> bool {
    !closes_within(tcp, Duration::from_millis(500)).await
}

/// §4.3 (review): connections that have not authenticated are bounded from
/// one address and from every address together, and at a bound the oldest
/// goes, not the newcomer: holding places means out-pacing real handshakes,
/// not idling. Every eviction is counted.
#[tokio::test]
async fn connections_not_authenticated_are_bounded_and_the_oldest_goes() {
    let gw = gateway_with(|s| {
        s.devices.unauthenticated_max = 3;
        s.devices.unauthenticated_per_peer = 2;
        s.devices.handshake_timeout_s = LONG_HANDSHAKE_S;
    })
    .await;
    // A LAN host (127.0.0.2) idles at the handshake.
    let mut idle_a = gw.tcp_from([127, 0, 0, 2]).await;
    gw.unauthenticated(1).await;
    let mut idle_b = gw.tcp_from([127, 0, 0, 2]).await;
    gw.unauthenticated(2).await;
    // Its third: its oldest goes, the newcomer stays.
    let mut third = gw.tcp_from([127, 0, 0, 2]).await;
    assert!(
        closes_within(&mut idle_a, Duration::from_secs(5)).await,
        "the peer's oldest was closed"
    );
    assert!(still_open(&mut third).await, "the newcomer stays");
    gw.unauthenticated(2).await;
    // Another address takes the last place; one more past the total closes
    // the oldest of all, whoever's it is.
    let _idle_c = gw.tcp_from([127, 0, 0, 3]).await;
    gw.unauthenticated(3).await;
    let mut fourth = gw.tcp_from([127, 0, 0, 4]).await;
    assert!(
        closes_within(&mut idle_b, Duration::from_secs(5)).await,
        "the oldest of all was closed"
    );
    assert!(still_open(&mut fourth).await);
    gw.unauthenticated(3).await;
    let status = gw.state.devices.listener.status(&gw.state);
    assert_eq!(status.evicted, 2, "{status:?}");
    let shown: Value = gw
        .gw
        .client()
        .get(format!("{}/api/status", gw.gw))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["devices"]["listener"]["evicted"], 2, "{shown}");
    assert_eq!(
        shown["devices"]["listener"]["unauthenticated"], 3,
        "{shown}"
    );
}

/// §4.3 (review S2): a stranger that fills both bounds with idle sockets,
/// from the device's own address among them, does not lock a paired device
/// out: its connection evicts the stranger's oldest, completes its
/// handshake, gives its place back and is served.
#[tokio::test]
async fn a_paired_device_gets_through_a_stranger_at_both_bounds() {
    let gw = gateway_with(|s| {
        s.devices.unauthenticated_max = 3;
        s.devices.unauthenticated_per_peer = 2;
        s.devices.handshake_timeout_s = LONG_HANDSHAKE_S;
    })
    .await;
    let device = gw.device("phone", true).await;
    // The per-address bound of the device's own address (127.0.0.1), and
    // the total with another.
    let mut oldest = gw.tcp_from([127, 0, 0, 1]).await;
    gw.unauthenticated(1).await;
    let _second = gw.tcp_from([127, 0, 0, 1]).await;
    let _third = gw.tcp_from([127, 0, 0, 2]).await;
    gw.unauthenticated(3).await;

    let mut tls = gw
        .connect_from([127, 0, 0, 1], Some(&device.identity))
        .await
        .unwrap();
    let raw = exchange(&mut tls, &get("/v1/models", ""), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(raw.status, 200, "{raw:?}");
    assert!(
        closes_within(&mut oldest, Duration::from_secs(5)).await,
        "the stranger's oldest made room"
    );
    // The device holds no place; the stranger's two others do.
    gw.unauthenticated(2).await;
    let status = gw.state.devices.listener.status(&gw.state);
    assert!(status.evicted >= 1, "{status:?}");
}

/// §4.3 (review S1): a certificate that chains to the CA but opens no
/// working device (here a disabled row's) does not authenticate: it holds a
/// place, lapses after `devices.unauthenticated_timeout_s`, and each of its
/// answers closes the connection, the gate naming the reason.
#[tokio::test]
async fn a_certificate_that_opens_no_working_device_holds_a_place_and_lapses() {
    let gw = gateway_with(|s| {
        s.devices.unauthenticated_timeout_s = 1;
        s.devices.handshake_timeout_s = LONG_HANDSHAKE_S;
    })
    .await;
    let phone = gw.device("phone", true).await;
    sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?1")
        .bind(phone.id)
        .execute(&gw.state.db)
        .await
        .unwrap();
    gw.state.reload_snapshot().await.unwrap();

    let mut tls = gw.connect(Some(&phone.identity)).await.unwrap();
    gw.unauthenticated(1).await;
    assert!(
        closes_within(&mut tls, Duration::from_secs(10)).await,
        "it lapsed"
    );
    gw.unauthenticated(0).await;

    let mut tls = gw.connect(Some(&phone.identity)).await.unwrap();
    let raw = exchange(
        &mut tls,
        &get("/chat/api/threads", ""),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(raw.status, 401, "{raw:?}");
    assert!(raw.body.contains("device_disabled"), "{raw:?}");
    assert_eq!(raw.header("connection"), Some("close"));
}

/// §4.3 (review item 7): while a token is pending a handshake may come
/// without a certificate, and then runs inside the connection's whole
/// bound: a socket that sends nothing closes after
/// `devices.unauthenticated_timeout_s`, long before the handshake's own.
#[tokio::test]
async fn a_handshake_that_may_come_without_a_certificate_runs_inside_the_whole_bound() {
    let gw = gateway_with(|s| {
        s.devices.unauthenticated_timeout_s = 1;
        s.devices.handshake_timeout_s = LONG_HANDSHAKE_S;
    })
    .await;
    token_pending(&gw);
    let mut tcp = gw.tcp_from([127, 0, 0, 1]).await;
    assert!(
        closes_within(&mut tcp, Duration::from_secs(20)).await,
        "closed long before devices.handshake_timeout_s ({LONG_HANDSHAKE_S} s)"
    );
}
