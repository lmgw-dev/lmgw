//! Devices (client-apps design §1, device link design §3, §5): pairing, the
//! `Chat` capability, the refusals at the door, and revocation.
//!
//! Driven over HTTP and a real WebSocket, for `owner_keys.rs`'s reason: the
//! Devices card posts `/api/op/*`, and a device dials the device listener
//! with its certificate (`support::device_pair`) — an op or a close that is
//! implemented but not wired is dead in exactly the way a direct call cannot
//! see.

use std::time::Duration;

use futures::StreamExt;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common::{self, Gw};
use crate::support::device_pair::{self, Paired};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn bearer(key: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {key}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    device_pair::op(gw, name, body).await
}

async fn get_as(client: &reqwest::Client, gw: &Gw, path: &str) -> (u16, Value) {
    let resp = client.get(format!("{gw}{path}")).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One row of `GET /api/usage/keys`, by name.
async fn listed(gw: &Gw, name: &str) -> Value {
    let (_, v) = get_as(&gw.client(), gw, "/api/usage/keys").await;
    v["keys"]
        .as_array()
        .unwrap_or_else(|| panic!("no keys array in {v}"))
        .iter()
        .find(|k| k["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| panic!("no key named '{name}' in {v}"))
}

/// A gateway with one chat alias (`chatty`) on an upstream that is never
/// called — enough for a realtime session to open — and its device listener
/// on.
async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('nowhere', 'openai', 'generic', 'http://127.0.0.1:9', '[]', 1000, 1, 0, '',
                 datetime('now'), datetime('now'), 0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id, param_overrides, enabled,
             created_at, updated_at)
         VALUES ('chatty', (SELECT id FROM upstreams WHERE name='nowhere'), 'm', '{}', 1,
                 datetime('now'), datetime('now'))",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    device_pair::listening(&state).await;
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// A device paired with `extra` merged into the create, enrolled and
/// confirmed.
async fn pair(state: &SharedState, gw: &Gw, name: &str, extra: Value) -> Paired {
    device_pair::pair(state, gw, name, extra).await
}

/// The link `key_create` mints for a device with `extra` merged in, and the
/// whole answer.
async fn create(gw: &Gw, name: &str, extra: Value) -> Value {
    let mut body = json!({ "kind": "device", "name": name, "local_only": true });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    let (status, v) = op(gw, "key_create", body).await;
    assert_eq!(status, 200, "{v}");
    v
}

#[tokio::test]
async fn a_device_is_paired_with_a_link_v2_and_holds_no_secret_of_lmgw_s() {
    let (state, gw) = gateway().await;
    let v = create(&gw, "desktop", json!({})).await;
    let id = v["id"].as_i64().unwrap();
    let port = device_pair::listening(&state).await;

    // Named so a string found anywhere says what it is (§1.1); the link v2
    // with the device listener's loopback URL, the CA's pin and a one-time
    // token, which `lmgw-client` reads (device link design §3.1).
    assert_eq!(v["name"], json!("device:desktop"), "{v}");
    assert_eq!(
        v["urls"],
        json!([format!("https://127.0.0.1:{port}")]),
        "{v}"
    );
    assert_eq!(v["local_only"], json!(true), "{v}");
    assert!(v.get("key").is_none(), "{v}");
    let link = lmgw_client::pairing::PairingLink::parse(v["link"].as_str().unwrap()).unwrap();
    assert_eq!(link.name, "desktop");
    assert_eq!(link.fp.to_string(), v["fp"].as_str().unwrap());
    assert_eq!(
        Some(link.fp.to_string()),
        state.devices.trust.ca().map(|ca| ca.fp().to_string())
    );

    // Nothing of lmgw's making is stored or revealed (L1): the row waits for
    // its device's key, the token is kept as its hash.
    let (plain, hash): (Option<String>, String) =
        sqlx::query_as("SELECT key_plain, key_hash FROM api_keys WHERE id = ?1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(plain, None);
    assert!(hash.starts_with("pending:"), "{hash}");
    let stored: String =
        sqlx::query_scalar("SELECT token_hash FROM device_pairing_tokens WHERE key_id = ?1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        stored,
        lmgw_core::devices::tokens::hash(link.token.expose())
    );
    let (status, v) = op(&gw, "key_reveal", json!({ "id": id })).await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("new pairing token"),
        "{v}"
    );

    // Listed as a device waiting for its device, prefilled `all` and no
    // budget (§11 Q1), offline.
    let row = listed(&gw, "device:desktop").await;
    assert_eq!(row["kind"], json!("device"), "{row}");
    assert_eq!(row["scope_mode"], json!("all"), "{row}");
    assert_eq!(row["budget_micro"], json!(0), "{row}");
    assert_eq!(row["online"], json!([]), "{row}");
    assert_eq!(row["last_seen_at"], Value::Null, "{row}");
    assert_eq!(row["pairing"]["state"], json!("waiting"), "{row}");
    assert_eq!(row["pairing"]["token"]["alive"], json!(true), "{row}");

    // One device per name.
    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "kind": "device", "name": "desktop" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("already paired"),
        "{v}"
    );
}

#[tokio::test]
async fn a_device_is_born_with_the_policy_its_form_confirmed() {
    let (_state, gw) = gateway().await;
    let v = create(
        &gw,
        "phone",
        json!({
            "scope_mode": "allow",
            "scope_patterns": "chatty\n\n",
            "tool_scope_mode": "deny",
            "tool_scope_patterns": "github__*",
            "budget_micro": 2_000_000,
            "budget_period": "day",
            "rpm_limit": 30,
            "expires_at": "2099-12-31",
            "hosts_label": "phone",
            "local_only": false,
            "urls": ["https://lmgw.example.net:8788/"],
        }),
    )
    .await;
    assert!(
        v["link"]
            .as_str()
            .unwrap()
            .starts_with("lmgw-pair:?v=2&name=phone&url=https%3A%2F%2Flmgw.example.net%3A8788&fp="),
        "{v}"
    );
    assert_eq!(v["local_only"], json!(false), "{v}");
    let row = listed(&gw, "device:phone").await;
    assert_eq!(row["scope_mode"], json!("allow"));
    assert_eq!(row["scope_patterns"], json!("chatty"));
    assert_eq!(row["tool_scope_mode"], json!("deny"));
    assert_eq!(row["budget_micro"], json!(2_000_000));
    assert_eq!(row["budget_period"], json!("day"));
    assert_eq!(row["rpm_limit"], json!(30));
    assert_eq!(row["expires_at"], json!("2099-12-31"));
    assert_eq!(row["hosts_label"], json!("phone"));
    assert_eq!(
        row["pairing"]["urls"],
        json!(["https://lmgw.example.net:8788"])
    );

    // A bad field refuses the create rather than leaving a wider device
    // behind.
    for (body, said) in [
        (json!({ "scope_mode": "some" }), "unknown scope_mode"),
        (json!({ "expires_at": "soon" }), "not a date"),
        (
            json!({ "hosts_label": "lmgw" }),
            "lmgw's own tool namespaces",
        ),
        (json!({ "hosts_label": "phone" }), "already hosts tools"),
        (json!({ "hosts_label": "a__b" }), "'__'"),
        (json!({ "urls": ["http://203.0.113.1:8788"] }), "https"),
        (json!({ "urls": [] }), "at least one URL"),
        (json!({ "local_only": "yes" }), "true or false"),
    ] {
        let mut b = json!({ "kind": "device", "name": "tablet" });
        for (k, val) in body.as_object().unwrap() {
            b[k] = val.clone();
        }
        let (status, v) = op(&gw, "key_create", b).await;
        assert_eq!(status, 400, "{body}: {v}");
        assert!(v["message"].as_str().unwrap().contains(said), "{body}: {v}");
    }
    let (_, keys) = get_as(&gw.client(), &gw, "/api/usage/keys").await;
    assert!(
        !keys.to_string().contains("device:tablet"),
        "a refused create left a row behind"
    );

    // The grant is the device's alone: a client key cannot take one, and no
    // MCP server may take a device's label as its prefix or name (§1.5).
    let (status, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    assert_eq!(status, 200, "{v}");
    let laptop = v["id"].as_i64().unwrap();
    let (status, v) = op(
        &gw,
        "key_set",
        json!({ "id": laptop, "hosts_label": "lap" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("not a device key"),
        "{v}"
    );
    let (status, v) = op(
        &gw,
        "mcp_server_set",
        json!({ "action": "create", "name": "phone", "transport": "http",
                "url": "http://127.0.0.1:9/mcp" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(v.to_string().contains("hosting label"), "{v}");
}

#[tokio::test]
async fn a_device_holds_the_chat_api_and_nothing_of_the_admin_plane() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;

    let (status, v) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 200, "{v}");
    let (status, _) = get_as(&device.client, &gw, "/v1/models").await;
    assert_eq!(status, 200);

    // The admin plane is off the device listener's face: the 404 an
    // unknown path is (device link design §4.6).
    for path in [
        "/api/status",
        "/audio-lab/api/models",
        "/image-lab/api/models",
    ] {
        let resp = device
            .client
            .get(format!("{gw}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
        assert!(resp.bytes().await.unwrap().is_empty(), "{path}");
    }

    // A client key does not hold the Chat API.
    let (_, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    let client = bearer(v["key"].as_str().unwrap());
    let (status, v) = get_as(&client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 403, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("this route needs chat;"),
        "{v}"
    );
}

#[tokio::test]
async fn disable_repair_expiry_and_delete_answer_at_the_door() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;
    let id = device.id;

    // Disabled: said by name, on the Chat API and on `/v1` alike (§1.6).
    let (status, v) = op(&gw, "key_set", json!({ "id": id, "enabled": false })).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_disabled"), "{v}");
    assert_eq!(
        v["message"],
        json!("device 'desktop' is disabled — enable it on Usage → Keys")
    );
    let (status, v) = get_as(&device.client, &gw, "/v1/models").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["error"]["code"], json!("device_disabled"), "{v}");
    op(&gw, "key_set", json!({ "id": id, "enabled": true })).await;

    // A new token that ends the key (device link design §5.2): the old
    // certificate's key opens no row, the new enrolment works once
    // confirmed.
    let (status, v) = op(
        &gw,
        "device_token",
        json!({ "id": id, "end_current": true }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["id"], json!(id), "the row, its policy and history stay");
    let (status, v) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_key_unknown"), "{v}");
    assert_eq!(
        v["message"],
        json!(
            "this device's key is no longer valid (re-paired or deleted) — pair the device again"
        )
    );
    // The same on `/v1` and at the realtime handshake, in their shape.
    let resp = device
        .client
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({ "model": "chatty", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], json!("device_key_unknown"), "{v}");
    match tokio_tungstenite::connect_async(realtime_at(&device.addr)).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
            let body = String::from_utf8_lossy(resp.body().as_deref().unwrap_or_default());
            let e = lmgw_client::requests::read_refusal(401, &body);
            assert_eq!(e.code, "device_key_unknown", "{body}");
        }
        other => panic!("expected the upgrade refused, got {other:?}"),
    }
    let link = device_pair::mint(&state, &gw, "device_token", json!({ "id": id })).await;
    let enrolled = device_pair::enrol(&link).await;
    device_pair::confirm(&gw, &enrolled).await;
    let device = device_pair::connected(enrolled, "device:desktop".into()).await;
    let (status, _) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 200);

    // Expired: on the Chat API too, with the date (L17).
    let (status, v) = op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "2020-01-01" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("key_expired"), "{v}");
    assert_eq!(
        v["message"],
        json!("device 'desktop' expired on 2020-01-01")
    );
    // The same words on `/v1` and at a realtime upgrade (review W2-18).
    let resp = device
        .client
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({ "model": "chatty", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["error"]["message"],
        json!("device 'desktop' expired on 2020-01-01"),
        "{v}"
    );
    match tokio_tungstenite::connect_async(realtime_at(&device.addr)).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
            let body = String::from_utf8_lossy(resp.body().as_deref().unwrap_or_default());
            assert!(
                body.contains("device 'desktop' expired on 2020-01-01"),
                "{body}"
            );
        }
        other => panic!("expected the upgrade refused, got {other:?}"),
    }
    op(&gw, "key_set", json!({ "id": id, "expires_at": "" })).await;

    // Deleted: gone, like a replaced key, and answered alike.
    let (status, v) = op(&gw, "key_delete", json!({ "id": id })).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_key_unknown"), "{v}");
}

// ---------------------------------------------------------------------------
// A device's connections: online, last seen, and the 4003 close
// ---------------------------------------------------------------------------

/// The realtime handshake at `addr` (a gateway's, or a device's proxy to
/// the device listener).
fn realtime_at(addr: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    format!("ws://{addr}/v1/realtime?model=chatty")
        .into_client_request()
        .unwrap()
}

/// A realtime session as a key (on the plain listener).
async fn open_session(gw: &Gw, key: &str) -> Ws {
    let mut req = realtime_at(&gw.addr().to_string());
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    opened(req).await
}

/// A realtime session as `device`, with its certificate.
async fn device_session(device: &Paired) -> Ws {
    opened(realtime_at(&device.addr)).await
}

async fn opened(req: tokio_tungstenite::tungstenite::handshake::client::Request) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let first = next_text(&mut ws).await;
    assert_eq!(first["type"], json!("session.created"), "{first}");
    ws
}

async fn next_text(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("the server said nothing for 5 s")
            .expect("the socket closed")
            .expect("the socket failed");
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text event, got {other:?}"),
        }
    }
}

/// The close the session ends with: `(code, reason)`.
async fn closed_with(ws: &mut Ws, within: Duration) -> (u16, String) {
    let frame = tokio::time::timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await
    .expect("the session was not closed")
    .expect("a close frame with a reason");
    (u16::from(frame.code), frame.reason.to_string())
}

/// The device's row once `pred` holds of it: its last connection's guard
/// drops as the session's task ends, just after the close went out.
async fn row_once(gw: &Gw, name: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let pred = &pred;
    crate::common::patience::until_async(
        &format!("{name} reaches the expected state"),
        || async move { pred(&listed(gw, name).await) },
    )
    .await;
    listed(gw, name).await
}

#[tokio::test]
async fn a_device_s_voice_session_is_online_and_stamps_last_seen() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;

    let ws = device_session(&device).await;
    let row = listed(&gw, "device:desktop").await;
    assert_eq!(row["online"], json!(["voice"]), "{row}");
    let opened = row["last_seen_at"]
        .as_str()
        .expect("stamped as it opened")
        .to_string();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&opened).is_ok(),
        "{opened}"
    );

    drop(ws);
    let row = row_once(&gw, "device:desktop", |r| r["online"] == json!([])).await;
    assert!(row["last_seen_at"].is_string(), "{row}");
}

#[tokio::test]
async fn revocation_closes_a_device_s_voice_session_with_4003_naming_why() {
    for (how, said) in [
        ("disable", "device_disabled: device 'desktop' was disabled"),
        (
            "new token",
            "key_unknown: the key of device 'desktop' was replaced — pair it again",
        ),
        ("delete", "key_unknown: device 'desktop' was deleted"),
    ] {
        let (state, gw) = gateway().await;
        let device = pair(&state, &gw, "desktop", json!({})).await;
        let id = device.id;
        let mut ws = device_session(&device).await;
        let (status, v) = match how {
            "disable" => op(&gw, "key_set", json!({ "id": id, "enabled": false })).await,
            "new token" => {
                op(
                    &gw,
                    "device_token",
                    json!({ "id": id, "end_current": true }),
                )
                .await
            }
            _ => op(&gw, "key_delete", json!({ "id": id })).await,
        };
        assert_eq!(status, 200, "{how}: {v}");
        let (code, reason) = closed_with(&mut ws, Duration::from_secs(5)).await;
        assert_eq!(code, 4003, "{how}");
        assert_eq!(reason, said, "{how}");
        // What a client reads of it: disabled waits, replaced and deleted
        // pair again.
        use lmgw_client::realtime::{close_kind, CloseKind, RevokeKind};
        let kind = if how == "disable" {
            RevokeKind::DeviceDisabled
        } else {
            RevokeKind::KeyUnknown
        };
        assert_eq!(
            close_kind(code, &reason),
            CloseKind::Revoked { kind },
            "{how}"
        );
    }
}

#[tokio::test]
async fn a_device_s_expiry_closes_its_session_at_that_moment() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;
    let mut ws = device_session(&device).await;

    // Set while the session is open: the watch re-reads it (`rearm`) and
    // wakes at the instant itself, with no request to trip over it.
    let at = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339();
    let (status, v) = op(&gw, "key_set", json!({ "id": device.id, "expires_at": at })).await;
    assert_eq!(status, 200, "{v}");
    let (code, reason) = closed_with(&mut ws, Duration::from_secs(6)).await;
    assert_eq!(code, 4003);
    assert_eq!(reason, "key_expired: device 'desktop' expired");
}

/// Device link §5.1, §13.1: a certificate whose `notAfter` passes while the
/// device's feed and voice session are open ends both with
/// `certificate_expired`, not before the date and within the watch's own
/// re-check bound of it (`devices::RECHECK`), with no request to trip over
/// it.
#[tokio::test]
async fn a_lapsing_certificate_ends_the_feed_and_the_session_with_certificate_expired() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;
    // A certificate for the device's own key that lapses in a few seconds:
    // the time to open both links with it.
    let (lapsing, not_after) =
        device_pair::lapsing(&state, &device, chrono::Duration::seconds(5)).await;
    let resp = lapsing
        .client
        .get(format!("{gw}/chat/api/feed"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut feed = crate::chat_feed::Feed::reading(resp);
    let mut ws = device_session(&lapsing).await;
    assert!(
        chrono::Utc::now() < not_after,
        "both links opened before the certificate lapsed"
    );
    let bound = (not_after - chrono::Utc::now())
        .to_std()
        .unwrap_or_default()
        + lmgw_core::devices::RECHECK;
    let said = "the certificate of device 'desktop' expired — pair it again";

    let (code, reason) = closed_with(&mut ws, bound).await;
    assert!(chrono::Utc::now() >= not_after, "not before the date");
    assert_eq!(code, 4003);
    assert_eq!(reason, format!("certificate_expired: {said}"));
    use lmgw_client::realtime::{close_kind, CloseKind, RevokeKind};
    assert_eq!(
        close_kind(code, &reason),
        CloseKind::Revoked {
            kind: RevokeKind::CertificateExpired
        }
    );

    feed.until(bound.as_secs() + 1, |f| {
        f.iter().any(|f| f.event == "revoked")
    })
    .await;
    let revoked = feed.named("revoked");
    assert_eq!(
        revoked[0].data,
        json!({ "reason": "certificate_expired", "message": said, "kind": "certificate_expired" }),
        "{:#?}",
        feed.frames
    );
}

#[tokio::test]
async fn a_client_key_s_session_is_no_link_but_is_revoked_like_one() {
    let (_state, gw) = gateway().await;
    let (_, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    let id = v["id"].as_i64().unwrap();
    let mut ws = open_session(&gw, v["key"].as_str().unwrap()).await;
    // Not counted online and nothing to stamp: only a device has either.
    let row = listed(&gw, "laptop").await;
    assert_eq!(row["online"], json!([]), "{row}");
    assert_eq!(row["last_seen_at"], Value::Null, "{row}");
    // But Disable means disable, for every kind (changed 2026-10-06): the
    // session ends now, with the reason, not at its next model call.
    op(&gw, "key_set", json!({ "id": id, "enabled": false })).await;
    let (code, reason) = closed_with(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(code, 4003);
    assert_eq!(reason, "revoked: key 'laptop' was disabled");
}

#[tokio::test]
async fn a_deleted_client_key_s_session_ends_too() {
    let (_state, gw) = gateway().await;
    let (_, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    let id = v["id"].as_i64().unwrap();
    let mut ws = open_session(&gw, v["key"].as_str().unwrap()).await;
    op(&gw, "key_delete", json!({ "id": id })).await;
    let (code, reason) = closed_with(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(code, 4003);
    assert_eq!(reason, "revoked: key 'laptop' was deleted");
}

/// An agent's tools are named by its id, so an agent may not take a
/// device's hosting label as its id, as a hand-made server may not take it
/// as its prefix (§1.1, review W2-8).
#[tokio::test]
async fn an_agent_cannot_register_its_tools_under_a_device_s_label() {
    let (state, gw) = gateway().await;
    create(&gw, "desk", json!({ "hosts_label": "board" })).await;
    let manifest = r#"{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "description": "an agent with tools",
  "model": { "alias": "chatty" },
  "run": {
    "kind": "container",
    "image": "localhost/board:1",
    "limits": { "memory_mb": 256, "cpus": 1.0, "pids": 64, "stop_grace_seconds": 1 },
    "service": { "port": 8080, "idle_seconds": 0, "start_timeout_seconds": 10 },
    "provides": { "mcp": "/mcp" }
  }
}"#;
    let (status, v) = op(
        &gw,
        "agent_set",
        json!({ "manifest": manifest, "replace": true }),
    )
    .await;
    assert!(v.to_string().contains("hosting label"), "{status}: {v}");
    let rows = lmgw_core::store::list_mcp_servers(&state.db).await.unwrap();
    // The device's own hosted-tools row holds it (§5.2), and no other.
    assert!(
        !rows
            .iter()
            .any(|r| r.tool_prefix.eq_ignore_ascii_case("board") && r.device_key_id.is_none()),
        "no server took the device's label"
    );
}

/// Review W2-12's gaps at the door, each over the real path.
#[tokio::test]
async fn the_door_refuses_a_device_on_every_way_in() {
    let (state, gw) = gateway().await;
    let device = pair(
        &state,
        &gw,
        "desk",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let id = device.id;

    // A new token keeps the row's policy, not only its id.
    let (s, v) = op(&gw, "device_token", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    let row = listed(&gw, "device:desk").await;
    assert_eq!(
        (&row["scope_mode"], &row["scope_patterns"]),
        (&json!("allow"), &json!("chatty")),
        "{row}"
    );
    // Kept until the new token's enrolment replaces it: the key works on.
    let (s, _) = get_as(&device.client, &gw, "/chat/api/threads").await;
    assert_eq!(s, 200);

    // The admin plane's door is off the device listener's face.
    let resp = device
        .client
        .post(format!("{gw}/mcp/admin"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "{}", resp.text().await.unwrap());

    // Expired: a Chat POST and the realtime upgrade refuse it too.
    op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "2020-01-01" }),
    )
    .await;
    let resp = device
        .client
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({ "model_alias": "chatty" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], "key_expired", "{v}");
    assert_eq!(upgrade_status(&device.addr, &[]).await, 401);

    // Disabled: the upgrade refuses it; a key subprotocol beside the
    // certificate is a credential the device listener refuses (D11).
    op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "", "enabled": false }),
    )
    .await;
    assert_eq!(upgrade_status(&device.addr, &[]).await, 401);
    op(&gw, "key_set", json!({ "id": id, "enabled": true })).await;
    let offered = "realtime, openai-insecure-api-key.lmgw-device-anything";
    assert_eq!(
        upgrade_status(&device.addr, &[("sec-websocket-protocol", offered)]).await,
        400
    );
}

/// The realtime handshake's status at `addr` with `headers` (101 when it
/// upgrades).
async fn upgrade_status(addr: &str, headers: &[(&str, &str)]) -> u16 {
    let mut req = realtime_at(addr);
    for (k, v) in headers {
        req.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("handshake failed below HTTP: {e}"),
    }
}

/// A hosting label is set and cleared on a device, refused for a case
/// variant of another device's (W2-12, W2-25), and a device's name is
/// printable text without ':' that no other key holds (W2-23).
#[tokio::test]
async fn labels_and_names_are_checked_as_they_are_written() {
    let (_state, gw) = gateway().await;
    let a = create(&gw, "desk", json!({})).await["id"].as_i64().unwrap();
    let b = create(&gw, "phone", json!({})).await["id"]
        .as_i64()
        .unwrap();
    let (s, v) = op(&gw, "key_set", json!({ "id": a, "hosts_label": "desk" })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        listed(&gw, "device:desk").await["hosts_label"],
        json!("desk")
    );
    let (s, v) = op(&gw, "key_set", json!({ "id": b, "hosts_label": "DESK" })).await;
    assert_eq!(s, 400, "a case variant is the same label: {v}");
    let (s, v) = op(&gw, "key_set", json!({ "id": a, "hosts_label": "" })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(listed(&gw, "device:desk").await["hosts_label"], Value::Null);

    for bad in ["two\nlines", "a:b"] {
        let (s, v) = op(&gw, "key_create", json!({ "kind": "device", "name": bad })).await;
        assert_eq!(s, 400, "{bad:?}: {v}");
    }
    op(&gw, "key_create", json!({ "name": "device:tv" })).await;
    let (s, v) = op(&gw, "key_create", json!({ "kind": "device", "name": "tv" })).await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("a client key"),
        "{v}"
    );
}

/// The device-tools grant (client-apps design L16 as changed 2026-10-15):
/// a device and a client key take any label a device could host, whether
/// one does or not, trimmed and once; a label no device could host is
/// refused by name; an owner key refuses one, and `[]` clears it.
#[tokio::test]
async fn a_device_tools_grant_takes_any_hostable_label_and_is_listed() {
    let (_state, gw) = gateway().await;
    let phone = create(
        &gw,
        "phone",
        json!({ "device_tools": [" kai ", "KAI", "desk"] }),
    )
    .await;
    let id = phone["id"].as_i64().unwrap();
    assert_eq!(
        listed(&gw, "device:phone").await["device_tools"],
        json!(["kai", "desk"]),
        "nothing hosts either yet: both kept"
    );
    let (s, v) = op(&gw, "key_set", json!({ "id": id, "device_tools": ["kai"] })).await;
    assert_eq!((s, &v["changed"]), (200, &json!(["device_tools"])), "{v}");
    let (s, v) = op(&gw, "key_set", json!({ "id": id, "device_tools": ["Kai"] })).await;
    assert_eq!((s, &v["changed"]), (200, &json!([])), "the same grant: {v}");
    for bad in ["kai desktop", "lmgw", "a__b", "kai_"] {
        let (s, v) = op(&gw, "key_set", json!({ "id": id, "device_tools": [bad] })).await;
        assert_eq!(s, 400, "{bad}: {v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains(&format!("device_tools: '{bad}'")),
            "{bad}: {v}"
        );
    }
    let (s, v) = op(&gw, "key_set", json!({ "id": id, "device_tools": [] })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(listed(&gw, "device:phone").await["device_tools"], json!([]));

    let (s, v) = op(
        &gw,
        "key_create",
        json!({ "name": "ci", "device_tools": ["kai"] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(listed(&gw, "ci").await["device_tools"], json!(["kai"]));
    let (s, v) = op(
        &gw,
        "key_create",
        json!({ "name": "ci-bad", "device_tools": "kai" }),
    )
    .await;
    assert_eq!(s, 400, "a list, not a string: {v}");
    let (s, v) = op(
        &gw,
        "key_create",
        json!({ "name": "boss", "kind": "owner", "device_tools": ["kai"] }),
    )
    .await;
    assert_eq!((s, &v["code"]), (400, &json!("refuse_owner")), "{v}");
}

/// A Disable, a new token that ends the key, or a Delete whose reload fails
/// afterwards (review W2-6, W3-7): the row is written, and the published
/// snapshot no longer admits the device — its certificate is refused at
/// once, not at the next reload that works. An owner key's Rotate too, and
/// the new value works.
#[tokio::test]
async fn a_failed_reload_does_not_keep_admitting_a_revoked_key() {
    let (state, gw) = gateway().await;
    let disabled = pair(&state, &gw, "one", json!({})).await;
    let ended = pair(&state, &gw, "two", json!({})).await;
    let deleted = pair(&state, &gw, "three", json!({})).await;
    // Every reload fails from here: the snapshot reads this table.
    sqlx::query("ALTER TABLE prices RENAME TO prices_away")
        .execute(&state.db)
        .await
        .unwrap();
    let threads = |device: &Paired| {
        let client = device.client.clone();
        let gw = gw.clone();
        async move { get_as(&client, &gw, "/chat/api/threads").await }
    };

    op(
        &gw,
        "key_set",
        json!({ "id": disabled.id, "enabled": false }),
    )
    .await;
    let (s, v) = threads(&disabled).await;
    assert_eq!((s, &v["code"]), (401, &json!("device_disabled")), "{v}");

    let (s, token) = op(
        &gw,
        "device_token",
        json!({ "id": ended.id, "end_current": true }),
    )
    .await;
    assert_eq!(s, 200, "{token}");
    assert!(
        token["message"]
            .as_str()
            .unwrap()
            .contains("could not be reloaded"),
        "{token}"
    );
    let (s, v) = threads(&ended).await;
    assert_eq!((s, &v["code"]), (401, &json!("device_key_unknown")), "{v}");

    op(&gw, "key_delete", json!({ "id": deleted.id })).await;
    let (s, v) = threads(&deleted).await;
    assert_eq!((s, &v["code"]), (401, &json!("device_key_unknown")), "{v}");

    // The dashboard's own key: rotated, its old value refused, the new one
    // handed back and working.
    let owner = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "owner:dashboard")
        .map(|k| k.id)
        .unwrap();
    let (s, v) = op(&gw, "key_rotate", json!({ "id": owner })).await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = get_as(&gw.client(), &gw, "/api/settings-full").await;
    assert_eq!(s, 401, "the old dashboard key is refused");
    let fresh = bearer(v["key"].as_str().unwrap());
    let (s, _) = get_as(&fresh, &gw, "/api/settings-full").await;
    assert_eq!(s, 200);
    sqlx::query("ALTER TABLE prices_away RENAME TO prices")
        .execute(&state.db)
        .await
        .unwrap();
}

/// A device's `/mcp` notification stream ends when its key ends (review
/// W3-11): without a frame, as MCP has none for it.
#[tokio::test]
async fn a_new_token_that_ends_the_key_ends_a_device_s_mcp_notification_stream() {
    let (state, gw) = gateway().await;
    let device = pair(&state, &gw, "desktop", json!({})).await;
    let client = &device.client;
    let init = client
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "device-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(init.status(), 200);
    let sid = init
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .expect("a session id");
    let mut stream = client
        .get(format!("{gw}/mcp"))
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let (s, v) = op(
        &gw,
        "device_token",
        json!({ "id": device.id, "end_current": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the stream did not end");
}

/// An owner key's Rotate ends the realtime session its old value opened
/// (review W3-11): the dashboard's own, closed 4003 with the reason.
#[tokio::test]
async fn an_owner_key_s_rotate_closes_its_realtime_session() {
    let (state, gw) = gateway().await;
    let mut ws = open_session(&gw, &gw.key).await;
    let id = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "owner:dashboard")
        .map(|k| k.id)
        .unwrap();
    let (s, v) = op(&gw, "key_rotate", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    let (code, reason) = closed_with(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(code, 4003);
    assert_eq!(reason, "revoked: owner key 'dashboard' was rotated");
}

/// A minted pairing link the shell may still hand off once goes when its
/// token ends: a new token (replaced by the new link), the QR card's end of
/// the token, and Delete (review W3-15; device link design §3.1).
#[tokio::test]
async fn a_new_token_an_ended_token_or_a_delete_forgets_the_minted_link() {
    let (state, gw) = gateway().await;
    let created = create(&gw, "desktop", json!({})).await;
    let id = created["id"].as_i64().unwrap();
    let first = created["link"].as_str().unwrap().to_string();
    let (s, again) = op(&gw, "device_token", json!({ "id": id })).await;
    assert_eq!(s, 200, "{again}");
    assert!(!state.devices.minted.take(&first), "the old link is gone");
    let second = again["link"].as_str().unwrap().to_string();
    let (s, v) = op(&gw, "device_token_end", json!({ "id": id })).await;
    assert_eq!((s, &v["ended"]), (200, &json!(true)), "{v}");
    assert!(
        !state.devices.minted.take(&second),
        "an ended token's link is gone"
    );
    let (s, third) = op(&gw, "device_token", json!({ "id": id })).await;
    assert_eq!(s, 200, "{third}");
    let (s, v) = op(&gw, "key_delete", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        !state.devices.minted.take(third["link"].as_str().unwrap()),
        "a deleted device's link is gone"
    );
}

/// A device name carries no invisible format characters (review W3-17): a
/// right-to-left override would make a close reason read backwards.
#[tokio::test]
async fn a_device_name_with_a_format_character_is_refused() {
    let (_state, gw) = gateway().await;
    for name in [
        "desk\u{202E}pot",
        "ph\u{200B}one",
        "\u{FEFF}tablet",
        "hiero\u{13430}glyph",
        "line\u{2028}break",
        "para\u{2029}graph",
    ] {
        let (status, v) = op(&gw, "key_create", json!({ "kind": "device", "name": name })).await;
        assert_eq!(status, 400, "{name:?}: {v}");
        assert!(v.to_string().contains("format characters"), "{v}");
    }
    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "kind": "device", "name": "Küche", "local_only": true }),
    )
    .await;
    assert_eq!(status, 200, "letters of any script are fine: {v}");
}
