//! Device keys (client-apps design §1, WP2): pairing, the `Chat`
//! capability, the refusals at the door, and revocation.
//!
//! Driven over HTTP and a real WebSocket, for `owner_keys.rs`'s reason: the
//! Devices card posts `/api/op/*`, and a device dials the router — an op or a
//! close that is implemented but not wired is dead in exactly the way a
//! direct call cannot see.

use std::time::Duration;

use futures::StreamExt;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common::{self, Gw};

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
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
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
/// called — enough for a realtime session to open.
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
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// Pair a device with `extra` merged into the create, and hand back
/// `(id, key, the whole answer)`.
async fn pair(gw: &Gw, name: &str, extra: Value) -> (i64, String, Value) {
    let mut body = json!({ "kind": "device", "name": name });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    let (status, v) = op(gw, "key_create", body).await;
    assert_eq!(status, 200, "{v}");
    (
        v["id"].as_i64().unwrap(),
        v["key"].as_str().unwrap().to_string(),
        v,
    )
}

#[tokio::test]
async fn a_device_is_paired_hash_only_with_a_link_shown_once() {
    let (state, gw) = gateway().await;
    let (id, key, v) = pair(&gw, "desktop", json!({})).await;

    // Named and minted so a string found anywhere says what it is (§1.1).
    assert_eq!(v["name"], json!("device:desktop"), "{v}");
    assert!(key.starts_with("lmgw-device-"), "{v}");
    assert_eq!(key.len(), "lmgw-device-".len() + 64, "{v}");
    // The link: this gateway's address, the bare name, the key (§1.4). The
    // test gateway's bind address is loopback, so the note says so.
    let url = v["url"].as_str().unwrap();
    assert_eq!(
        v["link"].as_str().unwrap(),
        format!(
            "lmgw-pair:?v=1&url={}&name=desktop&key={key}",
            url.replace(':', "%3A").replace('/', "%2F")
        ),
        "{v}"
    );
    assert_eq!(v["url_note"], json!("reachable from this computer only"));

    // Hash only (L1): nothing to reveal, no plaintext stored.
    let plain: Option<String> = sqlx::query_scalar("SELECT key_plain FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(plain, None);
    let (status, v) = op(&gw, "key_reveal", json!({ "id": id })).await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("device key"), "{v}");

    // Listed as a device, prefilled `all` and no budget (§11 Q1), offline.
    let row = listed(&gw, "device:desktop").await;
    assert_eq!(row["kind"], json!("device"), "{row}");
    assert_eq!(row["scope_mode"], json!("all"), "{row}");
    assert_eq!(row["budget_micro"], json!(0), "{row}");
    assert_eq!(row["online"], json!([]), "{row}");
    assert_eq!(row["last_seen_at"], Value::Null, "{row}");
    assert!(row.get("key").is_none(), "{row}");

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
    let (_, _, v) = pair(
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
            "url": "https://lmgw.example.net/",
        }),
    )
    .await;
    assert!(
        v["link"]
            .as_str()
            .unwrap()
            .starts_with("lmgw-pair:?v=1&url=https%3A%2F%2Flmgw.example.net&name=phone&key="),
        "{v}"
    );
    assert_eq!(v["url_note"], Value::Null, "a tunnel is not loopback: {v}");
    let row = listed(&gw, "device:phone").await;
    assert_eq!(row["scope_mode"], json!("allow"));
    assert_eq!(row["scope_patterns"], json!("chatty"));
    assert_eq!(row["tool_scope_mode"], json!("deny"));
    assert_eq!(row["budget_micro"], json!(2_000_000));
    assert_eq!(row["budget_period"], json!("day"));
    assert_eq!(row["rpm_limit"], json!(30));
    assert_eq!(row["expires_at"], json!("2099-12-31"));
    assert_eq!(row["hosts_label"], json!("phone"));

    // A bad field refuses the create rather than leaving a wider key behind.
    for (body, said) in [
        (json!({ "scope_mode": "some" }), "unknown scope_mode"),
        (json!({ "expires_at": "soon" }), "not a date"),
        (
            json!({ "hosts_label": "lmgw" }),
            "lmgw's own tool namespaces",
        ),
        (json!({ "hosts_label": "phone" }), "already hosts tools"),
        (json!({ "hosts_label": "a__b" }), "'__'"),
        (json!({ "url": "ftp://x" }), "not an address"),
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
    let (_state, gw) = gateway().await;
    let (_, key, _) = pair(&gw, "desktop", json!({})).await;
    let device = bearer(&key);

    let (status, v) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 200, "{v}");
    let (status, _) = get_as(&device, &gw, "/v1/models").await;
    assert_eq!(status, 200);

    // The admin plane names what was presented, by the device's own name.
    for path in [
        "/api/status",
        "/audio-lab/api/models",
        "/image-lab/api/models",
    ] {
        let (status, v) = get_as(&device, &gw, path).await;
        assert_eq!(status, 403, "{path}: {v}");
        assert_eq!(v["code"], json!("forbidden"), "{path}: {v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .ends_with("the request presented a device key ('desktop')"),
            "{path}: {v}"
        );
    }

    // A client key does not hold the Chat API.
    let (_, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    let client = bearer(v["plaintext"].as_str().unwrap());
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
async fn disable_rotate_expiry_and_delete_answer_at_the_door() {
    let (_state, gw) = gateway().await;
    let (id, key, _) = pair(&gw, "desktop", json!({})).await;
    let device = bearer(&key);

    // Disabled: said by name, on the Chat API and on `/v1` alike (§1.6).
    let (status, v) = op(&gw, "key_set", json!({ "id": id, "enabled": false })).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_disabled"), "{v}");
    assert_eq!(
        v["message"],
        json!("device 'desktop' is disabled — enable it on Usage → Keys")
    );
    let (status, v) = get_as(&device, &gw, "/v1/models").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["error"]["code"], json!("device_disabled"), "{v}");
    op(&gw, "key_set", json!({ "id": id, "enabled": true })).await;

    // Rotated: the old key matches no row, the new one works (L1).
    let (status, v) = op(&gw, "key_rotate", json!({ "id": id })).await;
    assert_eq!(status, 200, "{v}");
    let new_key = v["key"].as_str().unwrap().to_string();
    assert_ne!(new_key, key);
    assert!(
        v["link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("&key={new_key}")),
        "{v}"
    );
    assert_eq!(v["id"], json!(id), "the row, its policy and history stay");
    let (status, v) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_key_unknown"), "{v}");
    assert_eq!(
        v["message"],
        json!("this device key is no longer valid (rotated or deleted) — pair the device again")
    );
    // The same on `/v1` and at the realtime handshake, in their shape.
    let resp = device
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({ "model": "chatty", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], json!("device_key_unknown"), "{v}");
    let mut req = format!("ws://{}/v1/realtime?model=chatty", gw.addr())
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
            let body = String::from_utf8_lossy(resp.body().as_deref().unwrap_or_default());
            let e = lmgw_client::requests::read_refusal(401, &body);
            assert_eq!(e.code, "device_key_unknown", "{body}");
        }
        other => panic!("expected the upgrade refused, got {other:?}"),
    }
    let device = bearer(&new_key);
    let (status, _) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 200);

    // Expired: on the Chat API too, with the date (L17).
    let (status, v) = op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "2020-01-01" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("key_expired"), "{v}");
    assert_eq!(
        v["message"],
        json!("device 'desktop' expired on 2020-01-01")
    );
    // The same words on `/v1` and at a realtime upgrade (review W2-18).
    let resp = device
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
    let mut req = format!("ws://{}/v1/realtime?model=chatty", gw.addr())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {new_key}").parse().unwrap(),
    );
    match tokio_tungstenite::connect_async(req).await {
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

    // Deleted: gone, like a rotated key, and answered alike.
    let (status, v) = op(&gw, "key_delete", json!({ "id": id })).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = get_as(&device, &gw, "/chat/api/threads").await;
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["code"], json!("device_key_unknown"), "{v}");
}

// ---------------------------------------------------------------------------
// A device's connections: online, last seen, and the 4003 close
// ---------------------------------------------------------------------------

async fn open_session(gw: &Gw, key: &str) -> Ws {
    let mut req = format!("ws://{}/v1/realtime?model=chatty", gw.addr())
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
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

/// The device's row once `pred` holds of it, failing after two seconds: its
/// last connection's guard drops as the session's task ends, just after the
/// close went out.
async fn row_once(gw: &Gw, name: &str, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..40 {
        let row = listed(gw, name).await;
        if pred(&row) {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{name} never reached the expected state: {}",
        listed(gw, name).await
    );
}

#[tokio::test]
async fn a_device_s_voice_session_is_online_and_stamps_last_seen() {
    let (_state, gw) = gateway().await;
    let (_, key, _) = pair(&gw, "desktop", json!({})).await;

    let ws = open_session(&gw, &key).await;
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
            "rotate",
            "key_unknown: device 'desktop' was rotated — pair it again",
        ),
        ("delete", "key_unknown: device 'desktop' was deleted"),
    ] {
        let (_state, gw) = gateway().await;
        let (id, key, _) = pair(&gw, "desktop", json!({})).await;
        let mut ws = open_session(&gw, &key).await;
        let (status, v) = match how {
            "disable" => op(&gw, "key_set", json!({ "id": id, "enabled": false })).await,
            "rotate" => op(&gw, "key_rotate", json!({ "id": id })).await,
            _ => op(&gw, "key_delete", json!({ "id": id })).await,
        };
        assert_eq!(status, 200, "{how}: {v}");
        let (code, reason) = closed_with(&mut ws, Duration::from_secs(5)).await;
        assert_eq!(code, 4003, "{how}");
        assert_eq!(reason, said, "{how}");
        // What a client reads of it: disabled waits, rotated and deleted
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
    let (_state, gw) = gateway().await;
    let (id, key, _) = pair(&gw, "desktop", json!({})).await;
    let mut ws = open_session(&gw, &key).await;

    // Set while the session is open: the watch re-reads it (`rearm`) and
    // wakes at the instant itself, with no request to trip over it.
    let at = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339();
    let (status, v) = op(&gw, "key_set", json!({ "id": id, "expires_at": at })).await;
    assert_eq!(status, 200, "{v}");
    let (code, reason) = closed_with(&mut ws, Duration::from_secs(6)).await;
    assert_eq!(code, 4003);
    assert_eq!(reason, "key_expired: device 'desktop' expired");
}

#[tokio::test]
async fn a_client_key_s_session_is_no_link_but_is_revoked_like_one() {
    let (_state, gw) = gateway().await;
    let (_, v) = op(&gw, "key_create", json!({ "name": "laptop" })).await;
    let id = v["id"].as_i64().unwrap();
    let mut ws = open_session(&gw, v["plaintext"].as_str().unwrap()).await;
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
    let mut ws = open_session(&gw, v["plaintext"].as_str().unwrap()).await;
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
    pair(&gw, "desk", json!({ "hosts_label": "board" })).await;
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
    assert!(
        !rows
            .iter()
            .any(|r| r.tool_prefix.eq_ignore_ascii_case("board")),
        "no server took the device's label"
    );
}

/// Review W2-12's gaps at the door, each over the real path.
#[tokio::test]
async fn the_door_refuses_a_device_on_every_way_in() {
    let (_state, gw) = gateway().await;
    let (id, _, _) = pair(
        &gw,
        "desk",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;

    // Rotate keeps the row's policy, not only its id.
    let (s, v) = op(&gw, "key_rotate", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    let row = listed(&gw, "device:desk").await;
    assert_eq!(
        (&row["scope_mode"], &row["scope_patterns"]),
        (&json!("allow"), &json!("chatty")),
        "{row}"
    );
    let key = v["key"].as_str().unwrap().to_string();
    assert_ne!(key, "", "{v}");

    // A device key is no admin token on /mcp/admin.
    let resp = reqwest::Client::new()
        .post(format!("{gw}/mcp/admin"))
        .header("x-lmgw-admin-token", &key)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "{}", resp.text().await.unwrap());

    // Expired: a Chat POST and the realtime upgrade refuse it too.
    op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "2020-01-01" }),
    )
    .await;
    let resp = bearer(&key)
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({ "model_alias": "chatty" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], "key_expired", "{v}");
    assert_eq!(
        upgrade_status(&gw, &[("authorization", &format!("Bearer {key}"))]).await,
        401
    );

    // Disabled: the upgrade refuses it, by header and by the browser's
    // subprotocol alike.
    op(
        &gw,
        "key_set",
        json!({ "id": id, "expires_at": "", "enabled": false }),
    )
    .await;
    assert_eq!(
        upgrade_status(&gw, &[("authorization", &format!("Bearer {key}"))]).await,
        401
    );
    let offered = format!("realtime, openai-insecure-api-key.{key}");
    assert_eq!(
        upgrade_status(&gw, &[("sec-websocket-protocol", &offered)]).await,
        401
    );
}

/// The realtime handshake's status for `headers` (101 when it upgrades).
async fn upgrade_status(gw: &Gw, headers: &[(&str, &str)]) -> u16 {
    let mut req = format!("ws://{}/v1/realtime?model=chatty", gw.addr())
        .into_client_request()
        .unwrap();
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
    let (a, _, _) = pair(&gw, "desk", json!({})).await;
    let (b, _, _) = pair(&gw, "phone", json!({})).await;
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

/// A Disable, Rotate or Delete whose reload fails afterwards (review W2-6,
/// W3-7): the row is written, and the published snapshot no longer admits
/// the key — the old value is refused at once, not at the next reload that
/// works. An owner key's Rotate too, and the new value works.
#[tokio::test]
async fn a_failed_reload_does_not_keep_admitting_a_revoked_key() {
    let (state, gw) = gateway().await;
    let (disabled, disabled_key, _) = pair(&gw, "one", json!({})).await;
    let (rotated, rotated_key, _) = pair(&gw, "two", json!({})).await;
    let (deleted, deleted_key, _) = pair(&gw, "three", json!({})).await;
    // Every reload fails from here: the snapshot reads this table.
    sqlx::query("ALTER TABLE prices RENAME TO prices_away")
        .execute(&state.db)
        .await
        .unwrap();
    let threads = |key: &str| {
        let client = bearer(key);
        let gw = gw.clone();
        async move { get_as(&client, &gw, "/chat/api/threads").await }
    };

    op(&gw, "key_set", json!({ "id": disabled, "enabled": false })).await;
    let (s, v) = threads(&disabled_key).await;
    assert_eq!((s, &v["code"]), (401, &json!("device_disabled")), "{v}");

    let (s, rotate) = op(&gw, "key_rotate", json!({ "id": rotated })).await;
    assert_eq!(s, 200, "{rotate}");
    assert!(
        rotate["message"]
            .as_str()
            .unwrap()
            .contains("could not be reloaded"),
        "{rotate}"
    );
    let (s, v) = threads(&rotated_key).await;
    assert_eq!((s, &v["code"]), (401, &json!("device_key_unknown")), "{v}");
    let (s, v) = threads(rotate["key"].as_str().unwrap()).await;
    assert_eq!(s, 200, "the new value works: {v}");

    op(&gw, "key_delete", json!({ "id": deleted })).await;
    let (s, v) = threads(&deleted_key).await;
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

/// A key's `/mcp` notification stream ends when the key is rotated (review
/// W3-11): without a frame, as MCP has none for it.
#[tokio::test]
async fn a_rotate_ends_a_device_s_mcp_notification_stream() {
    let (_state, gw) = gateway().await;
    let (id, key, _) = pair(&gw, "desktop", json!({})).await;
    let client = bearer(&key);
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
    let (s, v) = op(&gw, "key_rotate", json!({ "id": id })).await;
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

/// A minted pairing link the shell may still hand off once goes with its
/// key's Rotate (replaced by the new link) and Delete (review W3-15): it
/// pairs nothing any more.
#[tokio::test]
async fn a_rotate_or_a_delete_forgets_the_minted_link() {
    let (state, gw) = gateway().await;
    let (id, _, created) = pair(&gw, "desktop", json!({})).await;
    let first = created["link"].as_str().unwrap().to_string();
    let (s, rotated) = op(&gw, "key_rotate", json!({ "id": id })).await;
    assert_eq!(s, 200, "{rotated}");
    assert!(!state.devices.minted.take(&first), "the old link is gone");
    let second = rotated["link"].as_str().unwrap().to_string();
    let (s, v) = op(&gw, "key_delete", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        !state.devices.minted.take(&second),
        "a deleted key's link is gone"
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
        json!({ "kind": "device", "name": "Küche" }),
    )
    .await;
    assert_eq!(status, 200, "letters of any script are fine: {v}");
}
